//! PC-to-phone side: listing what's shared and serving downloads.
//!
//! A name only resolves if it shows up in a directory listing. That one
//! rule blocks traversal (`../x`, `a/b`), absolute paths, Windows device
//! names (`CON`), and alternate data streams (`file.txt:stream`), because
//! none of those can appear as an entry name. The resolved path is then
//! canonicalized and checked against the canonical shared folder, so a
//! symlink pointing outside is refused too.
//!
//! The one exception is a file the person at the PC shared from elsewhere
//! (see [`crate::share`]). Those resolve by their listed name to the exact
//! path that was registered, and nothing else.

use std::collections::HashSet;
use std::fs::{self, DirEntry, Metadata};
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, UNIX_EPOCH};

use axum::Json;
use axum::body::Body;
use axum::extract::{ConnectInfo, Path as UrlPath, Request, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures::StreamExt;
use serde::Serialize;
use tower_http::services::ServeFile;

use crate::sanitize::is_partial;
use crate::server::AppState;
use crate::share::{Extra, View};
use crate::{human, upload};

/// Read size for downloads. tower-http's 64 KB default means more
/// round trips to the blocking thread pool than needed on fast Wi-Fi.
const READ_CHUNK: usize = 256 * 1024;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct FileEntry {
    pub name: String,
    pub size: u64,
    /// Seconds since the Unix epoch.
    pub modified: u64,
}

#[derive(Serialize)]
pub struct FileList {
    /// The shared folder's name, so the phone can show what it's looking at.
    pub folder: String,
    pub files: Vec<FileEntry>,
}

/// A file that may be shown and served.
struct Visible {
    name: String,
    /// Canonical path, inside the shared folder.
    path: PathBuf,
    meta: Metadata,
}

/// Files modified or created more recently than these are checked for a
/// writer still holding them open. Older files skip the check, so listing a
/// folder of old files costs no extra I/O.
const SETTLE_TIME: Duration = Duration::from_secs(3);
/// A Windows copy gets a fresh creation time when it starts, and keeps it.
/// This covers copies of up to ~100 GB at 30 MB/s.
const COPY_WINDOW: Duration = Duration::from_secs(60 * 60);

/// One look at the shared folder.
#[derive(Debug, Default)]
pub struct Scan {
    /// Ready files, newest first.
    pub files: Vec<FileEntry>,
    /// Files left out because another program is still writing them.
    pub busy: usize,
}

/// Files in the shared folder (top level only) and files shared from
/// elsewhere, newest first.
pub fn list(view: &View) -> io::Result<Vec<FileEntry>> {
    scan(view).map(|s| s.files)
}

pub fn scan(view: &View) -> io::Result<Scan> {
    let dir = &view.dir;
    let mut scan = Scan::default();
    let mut taken = HashSet::new();
    for entry in fs::read_dir(dir)?.filter_map(Result::ok) {
        taken.insert(entry.file_name().to_string_lossy().into_owned());
        if let Some(v) = inspect(&entry, dir) {
            scan.add(v);
        }
    }
    for (name, extra) in extra_names(&mut taken, view) {
        if let Some(v) = inspect_extra(extra, name) {
            scan.add(v);
        }
    }
    scan.files.sort_by(|a, b| {
        b.modified
            .cmp(&a.modified)
            .then_with(|| a.name.cmp(&b.name))
    });
    Ok(scan)
}

impl Scan {
    fn add(&mut self, v: Visible) {
        if is_being_written(&v) {
            self.busy += 1;
            return;
        }
        self.files.push(FileEntry {
            modified: modified_secs(&v.meta),
            size: v.meta.len(),
            name: v.name,
        });
    }
}

/// The names files shared from elsewhere go by on the phone: their own,
/// or numbered when an entry in the folder (`taken`) or an earlier extra
/// already has it. Uploads keep their names; an extra moves aside. Files
/// that are in the shared folder now are left out, since they're listed
/// already.
fn extra_names<'a>(taken: &mut HashSet<String>, view: &'a View) -> Vec<(String, &'a Extra)> {
    let mut names = Vec::new();
    for extra in &view.extras {
        if extra.path.parent() == Some(view.dir.as_path()) {
            continue;
        }
        let name = (0..)
            .map(|n| upload::numbered_name(&extra.name, n))
            .find(|n| !taken.contains(n))
            .unwrap_or_default();
        taken.insert(name.clone());
        names.push((name, extra));
    }
    names
}

/// A file shared from elsewhere, if it's still a file at the same place.
fn inspect_extra(extra: &Extra, name: String) -> Option<Visible> {
    // Canonicalizing again catches the file being swapped for a link to
    // somewhere else since it was shared.
    let path = fs::canonicalize(&extra.path)
        .ok()
        .filter(|p| *p == extra.path)?;
    let meta = fs::metadata(&path).ok().filter(Metadata::is_file)?;
    Some(Visible { name, path, meta })
}

/// True while another program is still copying or saving the file.
///
/// Windows (Explorer, `CopyFileEx`) sets the full size before writing the
/// data. Without this check the phone could download a 2 GB file that is
/// mostly zeros. While copying, the copier holds the file open for writing.
/// At the end, the lock goes away and the modified time is set to the
/// source's. Measured on Windows 11.
///
/// The modified time alone can't tell when to check. On a busy disk it
/// stalled for 15 s mid-copy. The creation time is set when the copy
/// starts, so a young file of either kind gets the lock check.
fn is_being_written(v: &Visible) -> bool {
    let younger_than = |time: io::Result<std::time::SystemTime>, limit: Duration| {
        time.ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age < limit)
    };
    let young =
        younger_than(v.meta.modified(), SETTLE_TIME) || younger_than(v.meta.created(), COPY_WINDOW);
    young && locked_for_writing(&v.path)
}

/// Tries to open the file while refusing to share write access. Windows
/// refuses that with a sharing violation if someone has it open for writing.
/// The probe handle lives for microseconds and only runs on just-modified files.
#[cfg(windows)]
fn locked_for_writing(path: &Path) -> bool {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_SHARE_READ: u32 = 0x1;
    const FILE_SHARE_DELETE: u32 = 0x4;
    const ERROR_SHARING_VIOLATION: i32 = 32;
    match fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_DELETE)
        .open(path)
    {
        Ok(_) => false,
        Err(e) => e.raw_os_error() == Some(ERROR_SHARING_VIOLATION),
    }
}

/// Linux and macOS have no mandatory locks to detect a writer with, so
/// files show up as soon as they appear.
#[cfg(not(windows))]
fn locked_for_writing(_path: &Path) -> bool {
    false
}

/// The canonical path of the listed file called exactly `name`, if any.
pub fn resolve(view: &View, name: &str) -> Option<PathBuf> {
    // Cheap rejections before scanning the folder. The scan below is the
    // real guarantee; these only skip pointless work.
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\', '\0']) {
        return None;
    }
    let dir = &view.dir;
    // An unreadable folder (a drive unplugged, say) still leaves the
    // files shared from elsewhere.
    let entries: Vec<DirEntry> = fs::read_dir(dir)
        .map(|rd| rd.filter_map(Result::ok).collect())
        .unwrap_or_default();
    let found = match entries.iter().find(|entry| entry.file_name() == name) {
        // The name belongs to the folder, even if that entry isn't shown
        // (hidden, a subfolder): extras are numbered around every entry.
        Some(entry) => inspect(entry, dir),
        None => {
            let mut taken = entries
                .iter()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect();
            extra_names(&mut taken, view)
                .into_iter()
                .find(|(n, _)| n == name)
                .and_then(|(n, extra)| inspect_extra(extra, n))
        }
    };
    found.filter(|v| !is_being_written(v)).map(|v| v.path)
}

/// Decides whether a directory entry is a shareable file.
fn inspect(entry: &DirEntry, dir: &Path) -> Option<Visible> {
    // Names that aren't valid Unicode can't round-trip through JSON and URLs.
    let name = entry.file_name().into_string().ok()?;
    if is_partial(&name) || is_hidden(entry, &name) {
        return None;
    }
    let file_type = entry.file_type().ok()?;
    if !(file_type.is_file() || file_type.is_symlink()) {
        return None;
    }
    let path = fs::canonicalize(entry.path()).ok()?;
    if !path.starts_with(dir) {
        return None;
    }
    // Follows symlinks, so a link to a folder is rejected here.
    let meta = fs::metadata(&path).ok().filter(Metadata::is_file)?;
    Some(Visible { name, path, meta })
}

/// Hidden and system files (desktop.ini, Thumbs.db) aren't meant to be shared.
#[cfg(windows)]
fn is_hidden(entry: &DirEntry, _name: &str) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
    const FILE_ATTRIBUTE_SYSTEM: u32 = 0x4;
    entry
        .metadata()
        .is_ok_and(|m| m.file_attributes() & (FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM) != 0)
}

/// Dotfiles are hidden on Linux and macOS (.DS_Store, .directory).
#[cfg(not(windows))]
fn is_hidden(_entry: &DirEntry, name: &str) -> bool {
    name.starts_with('.')
}

fn modified_secs(meta: &Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs())
}

/// `attachment` with both a plain ASCII `filename` for old clients and the
/// exact UTF-8 name in `filename*` (RFC 6266 / RFC 8187), which every
/// current browser prefers.
pub fn content_disposition(name: &str) -> String {
    let fallback: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_graphic() && !matches!(c, '"' | '\\' | '%') || c == ' ' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!(
        "attachment; filename=\"{fallback}\"; filename*=UTF-8''{}",
        percent_encode(name)
    )
}

/// RFC 8187 `attr-char` encoding: anything outside the safe set becomes %XX.
fn percent_encode(s: &str) -> String {
    const SAFE: &[u8] = b"!#$&+-.^_`|~";
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || SAFE.contains(&b) {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

// ---- Handlers ----

/// `GET /api/files`
pub async fn list_handler(State(state): State<AppState>) -> Response {
    let view = state.share.view();
    let shown = state.share.folder().display.clone();
    // "E:\" has no last component; show it whole.
    let folder = shown
        .file_name()
        .unwrap_or(shown.as_os_str())
        .to_string_lossy()
        .into_owned();
    match tokio::task::spawn_blocking(move || list(&view)).await {
        Ok(Ok(files)) => Json(FileList { folder, files }).into_response(),
        Ok(Err(e)) => {
            tracing::error!("Could not read the shared folder: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not read the shared folder",
            )
                .into_response()
        }
        Err(e) => {
            tracing::error!("listing task failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// `GET /api/files/{name}`. Range requests, `If-Modified-Since`, and HEAD
/// are handled by tower-http's `ServeFile`.
pub async fn download_handler(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    UrlPath(name): UrlPath<String>,
    req: Request,
) -> Response {
    let view = state.share.view();
    let lookup = name.clone();
    let path = tokio::task::spawn_blocking(move || resolve(&view, &lookup))
        .await
        .ok()
        .flatten();
    let Some(path) = path else {
        return (StatusCode::NOT_FOUND, "Not found").into_response();
    };

    let is_get = req.method() == Method::GET;
    let mut serve = ServeFile::new(&path)
        .with_buf_chunk_size(READ_CHUNK)
        // Browsers and players handle a full 200 fine. tower-http can't
        // build a multipart/byteranges reply, so serve the whole file
        // instead of refusing.
        .ignore_multi_range_requests(true);
    let res = match serve.try_call(req).await {
        Ok(res) => res,
        Err(e) => {
            tracing::warn!("Could not send {name:?} to {}: {e}", peer.ip());
            return (StatusCode::INTERNAL_SERVER_ERROR, "Could not read the file").into_response();
        }
    };

    let status = res.status();
    let (mut parts, body) = res.into_parts();
    if status.is_success() {
        match HeaderValue::from_str(&content_disposition(&name)) {
            Ok(v) => {
                parts.headers.insert(header::CONTENT_DISPOSITION, v);
            }
            Err(e) => tracing::debug!("could not build Content-Disposition: {e}"),
        }
    }
    let body = Body::new(body);
    if !is_get || !status.is_success() {
        return Response::from_parts(parts, body);
    }

    let expected = parts
        .headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let mut log = DownloadLog {
        name,
        ip: peer.ip(),
        expected,
        sent: 0,
        partial: status == StatusCode::PARTIAL_CONTENT,
        started: Instant::now(),
    };
    // `log` moves into the stream and drops with it, which is when the
    // transfer is over. The method call matters: touching only `log.sent`
    // would make the closure capture just that field (edition 2024), and
    // the log would fire as soon as this handler returned.
    let counted = body.into_data_stream().map(move |chunk| {
        if let Ok(bytes) = &chunk {
            log.record(bytes.len());
        }
        chunk
    });
    Response::from_parts(parts, Body::from_stream(counted))
}

/// Logs a download when its body is finished or abandoned.
struct DownloadLog {
    name: String,
    ip: IpAddr,
    expected: u64,
    sent: u64,
    /// A 206 reply. Players fetch many of these while seeking, so they are
    /// logged at debug level to keep the terminal readable.
    partial: bool,
    started: Instant,
}

impl DownloadLog {
    fn record(&mut self, bytes: usize) {
        self.sent += bytes as u64;
    }
}

impl Drop for DownloadLog {
    fn drop(&mut self) {
        let elapsed = self.started.elapsed();
        let complete = self.sent >= self.expected;
        match (self.partial, complete) {
            (false, true) => tracing::info!(
                "Sent {:?} to {}: {} in {}, {}",
                self.name,
                self.ip,
                human::bytes(self.sent),
                human::duration(elapsed),
                human::rate(self.sent, elapsed),
            ),
            (false, false) => tracing::info!(
                "Download of {:?} by {} stopped after {} of {}",
                self.name,
                self.ip,
                human::bytes(self.sent),
                human::bytes(self.expected),
            ),
            (true, _) => tracing::debug!(
                "range of {:?} to {}: {} of {} in {}",
                self.name,
                self.ip,
                human::bytes(self.sent),
                human::bytes(self.expected),
                human::duration(elapsed),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    fn canon(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().canonicalize().unwrap()
    }

    fn write_aged(dir: &Path, name: &str, body: &[u8], age_secs: u64) {
        let path = dir.join(name);
        fs::write(&path, body).unwrap();
        let when = SystemTime::now() - Duration::from_secs(age_secs);
        fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(when)
            .unwrap();
    }

    // Most tests share just a folder.
    fn list(dir: &Path) -> io::Result<Vec<FileEntry>> {
        super::list(&View::of(dir.to_path_buf()))
    }

    fn resolve(dir: &Path, name: &str) -> Option<PathBuf> {
        super::resolve(&View::of(dir.to_path_buf()), name)
    }

    // Only the Windows-only tests look at files still being written.
    #[cfg(windows)]
    fn scan(dir: &Path) -> io::Result<Scan> {
        super::scan(&View::of(dir.to_path_buf()))
    }

    fn names(dir: &Path) -> Vec<String> {
        list(dir).unwrap().into_iter().map(|f| f.name).collect()
    }

    fn extra(id: u32, path: &Path) -> Extra {
        let path = path.canonicalize().unwrap();
        Extra {
            id,
            name: path.file_name().unwrap().to_string_lossy().into_owned(),
            display: path.clone(),
            path,
        }
    }

    #[test]
    fn files_from_elsewhere_are_listed_and_served_in_place() {
        let shared = tempfile::tempdir().unwrap();
        let ssd = tempfile::tempdir().unwrap();
        let dir = canon(&shared);
        write_aged(&dir, "notes.txt", b"in the folder", 100);
        write_aged(ssd.path(), "movie.mkv", b"frames", 100);
        let view = View {
            dir: dir.clone(),
            extras: vec![extra(0, &ssd.path().join("movie.mkv"))],
        };

        let mut listed: Vec<_> = super::list(&view)
            .unwrap()
            .into_iter()
            .map(|f| (f.name, f.size))
            .collect();
        listed.sort();
        assert_eq!(listed, [("movie.mkv".into(), 6), ("notes.txt".into(), 13)]);
        assert_eq!(
            super::resolve(&view, "movie.mkv"),
            Some(ssd.path().join("movie.mkv").canonicalize().unwrap())
        );
        // Without the extra, the name means nothing.
        assert_eq!(resolve(&dir, "movie.mkv"), None);
    }

    #[test]
    fn a_file_from_elsewhere_moves_aside_for_a_folder_file_of_the_same_name() {
        let shared = tempfile::tempdir().unwrap();
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let dir = canon(&shared);
        write_aged(&dir, "photo.jpg", b"folder", 100);
        write_aged(a.path(), "photo.jpg", b"from a", 100);
        write_aged(b.path(), "photo.jpg", b"from b", 100);
        let view = View {
            dir: dir.clone(),
            extras: vec![
                extra(0, &a.path().join("photo.jpg")),
                extra(1, &b.path().join("photo.jpg")),
            ],
        };
        let mut listed: Vec<_> = super::list(&view)
            .unwrap()
            .into_iter()
            .map(|f| f.name)
            .collect();
        listed.sort();
        assert_eq!(listed, ["photo (1).jpg", "photo (2).jpg", "photo.jpg"]);

        let read = |name| fs::read(super::resolve(&view, name).unwrap()).unwrap();
        assert_eq!(read("photo.jpg"), b"folder");
        assert_eq!(read("photo (1).jpg"), b"from a");
        assert_eq!(read("photo (2).jpg"), b"from b");
    }

    #[test]
    fn files_from_elsewhere_that_moved_or_are_now_in_the_folder() {
        let shared = tempfile::tempdir().unwrap();
        let ssd = tempfile::tempdir().unwrap();
        let dir = canon(&shared);
        write_aged(ssd.path(), "gone.mkv", b"x", 100);
        write_aged(&dir, "here.txt", b"y", 100);
        let view = View {
            dir: dir.clone(),
            extras: vec![
                extra(0, &ssd.path().join("gone.mkv")),
                // Shared before the folder was switched to its own folder.
                extra(1, &dir.join("here.txt")),
            ],
        };
        fs::remove_file(ssd.path().join("gone.mkv")).unwrap();
        // Listed once, as a folder file, and the deleted one not at all.
        assert_eq!(
            super::list(&view)
                .unwrap()
                .into_iter()
                .map(|f| f.name)
                .collect::<Vec<_>>(),
            ["here.txt"]
        );
        assert_eq!(super::resolve(&view, "gone.mkv"), None);
    }

    #[test]
    fn files_from_elsewhere_survive_an_unreadable_folder_on_download() {
        let ssd = tempfile::tempdir().unwrap();
        write_aged(ssd.path(), "movie.mkv", b"frames", 100);
        let view = View {
            dir: ssd.path().join("unplugged"),
            extras: vec![extra(0, &ssd.path().join("movie.mkv"))],
        };
        assert!(super::resolve(&view, "movie.mkv").is_some());
    }

    #[test]
    fn lists_newest_first_with_sizes() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = canon(&tmp);
        write_aged(&dir, "old.txt", b"1", 300);
        write_aged(&dir, "new.txt", b"22", 10);
        write_aged(&dir, "mid.txt", b"333", 100);
        let files = list(&dir).unwrap();
        let got: Vec<(&str, u64)> = files.iter().map(|f| (f.name.as_str(), f.size)).collect();
        assert_eq!(got, [("new.txt", 2), ("mid.txt", 3), ("old.txt", 1)]);
        assert!(files[0].modified > files[2].modified);
    }

    #[test]
    fn skips_folders_and_partial_uploads() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = canon(&tmp);
        fs::write(dir.join("keep.txt"), b"x").unwrap();
        fs::write(dir.join("video.mp4.beam-1234abcd.part"), b"x").unwrap();
        fs::write(dir.join("other.zip.part"), b"x").unwrap();
        fs::create_dir(dir.join("folder")).unwrap();
        assert_eq!(names(&dir), ["keep.txt"]);
        assert_eq!(resolve(&dir, "video.mp4.beam-1234abcd.part"), None);
        assert_eq!(resolve(&dir, "folder"), None);
    }

    #[cfg(windows)]
    #[test]
    fn skips_hidden_and_system_files() {
        use std::os::windows::fs::OpenOptionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let dir = canon(&tmp);
        fs::write(dir.join("keep.txt"), b"x").unwrap();
        for (name, attr) in [("desktop.ini", 0x2 | 0x4), ("secret.txt", 0x2)] {
            fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .attributes(attr)
                .open(dir.join(name))
                .unwrap();
        }
        assert_eq!(names(&dir), ["keep.txt"]);
        assert_eq!(resolve(&dir, "secret.txt"), None);
    }

    #[cfg(not(windows))]
    #[test]
    fn skips_dotfiles() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = canon(&tmp);
        fs::write(dir.join("keep.txt"), b"x").unwrap();
        fs::write(dir.join(".DS_Store"), b"x").unwrap();
        assert_eq!(names(&dir), ["keep.txt"]);
    }

    /// Mirrors an Explorer copy: full size already set, writer still open.
    #[cfg(windows)]
    #[test]
    fn files_still_being_written_are_held_back() {
        use std::io::Write;
        let tmp = tempfile::tempdir().unwrap();
        let dir = canon(&tmp);
        write_aged(&dir, "done.txt", b"x", 600);
        let mut writer = fs::File::create(dir.join("copying.mp4")).unwrap();
        writer.set_len(10_000_000).unwrap();
        writer.write_all(b"first bytes").unwrap();

        let scan = scan(&dir).unwrap();
        assert_eq!(scan.busy, 1);
        assert_eq!(names(&dir), ["done.txt"]);
        assert_eq!(resolve(&dir, "copying.mp4"), None);

        drop(writer);
        assert_eq!(names(&dir), ["copying.mp4", "done.txt"]);
        assert!(resolve(&dir, "copying.mp4").is_some());
    }

    /// The case the end-to-end test caught: on a busy disk the modified time
    /// stopped moving for 15 s mid-copy. The file was created moments ago and
    /// is still held for writing, so it must stay hidden.
    #[cfg(windows)]
    #[test]
    fn stalled_copies_are_still_held_back() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = canon(&tmp);
        let writer = fs::File::create(dir.join("stalled.mkv")).unwrap();
        writer.set_len(1_000_000).unwrap();
        writer
            .set_modified(SystemTime::now() - Duration::from_secs(600))
            .unwrap();
        assert_eq!(scan(&dir).unwrap().busy, 1);
        assert!(names(&dir).is_empty());
        drop(writer);
        assert_eq!(names(&dir), ["stalled.mkv"]);
    }

    #[test]
    fn old_files_open_for_writing_are_not_held_back() {
        // Past the settle time the lock probe doesn't even run, so a log
        // file some program keeps open stays visible.
        let tmp = tempfile::tempdir().unwrap();
        let dir = canon(&tmp);
        write_aged(&dir, "app.log", b"x", 600);
        let writer = fs::OpenOptions::new()
            .append(true)
            .open(dir.join("app.log"))
            .unwrap();
        #[cfg(windows)]
        {
            use std::os::windows::fs::FileTimesExt;
            let two_hours_ago = SystemTime::now() - Duration::from_secs(2 * 60 * 60);
            writer
                .set_times(
                    fs::FileTimes::new()
                        .set_created(two_hours_ago)
                        .set_modified(two_hours_ago),
                )
                .unwrap();
        }
        assert_eq!(names(&dir), ["app.log"]);
        drop(writer);
    }

    #[test]
    fn resolves_only_listed_names() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = canon(&tmp);
        fs::write(dir.join("a.txt"), b"x").unwrap();
        fs::write(
            tmp.path().parent().unwrap().join("beam-outside-probe.txt"),
            b"x",
        )
        .ok();

        assert_eq!(resolve(&dir, "a.txt"), Some(dir.join("a.txt")));
        for bad in [
            "",
            ".",
            "..",
            "../a.txt",
            "./a.txt",
            "a.txt/",
            r"..\a.txt",
            "../beam-outside-probe.txt",
            "a.txt:stream",
            "a.txt::$DATA",
            "CON",
            "NUL.txt",
            "missing.txt",
            "a.txt\0",
        ] {
            assert_eq!(resolve(&dir, bad), None, "{bad:?} must not resolve");
        }
        let abs = dir.join("a.txt");
        assert_eq!(resolve(&dir, &abs.to_string_lossy()), None);
        let _ = fs::remove_file(tmp.path().parent().unwrap().join("beam-outside-probe.txt"));
    }

    /// Needs symlink rights; on Windows that means Developer Mode or admin,
    /// so the test skips itself when it can't create one.
    #[test]
    fn symlinks_pointing_outside_are_refused() {
        let outside = tempfile::tempdir().unwrap();
        let secret = outside.path().join("secret.txt");
        fs::write(&secret, b"top secret").unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let dir = canon(&tmp);
        fs::write(dir.join("inside.txt"), b"ok").unwrap();

        #[cfg(windows)]
        let made =
            std::os::windows::fs::symlink_file(&secret, dir.join("link.txt")).and_then(|()| {
                std::os::windows::fs::symlink_file(dir.join("inside.txt"), dir.join("ok-link.txt"))
            });
        #[cfg(not(windows))]
        let made = std::os::unix::fs::symlink(&secret, dir.join("link.txt")).and_then(|()| {
            std::os::unix::fs::symlink(dir.join("inside.txt"), dir.join("ok-link.txt"))
        });
        if made.is_err() {
            eprintln!("skipping: no permission to create symlinks");
            return;
        }
        assert_eq!(resolve(&dir, "link.txt"), None);
        assert!(!names(&dir).contains(&"link.txt".to_owned()));
        // A link that stays inside the folder is fine.
        assert_eq!(resolve(&dir, "ok-link.txt"), Some(dir.join("inside.txt")));
    }

    #[test]
    fn content_disposition_encodes_the_name() {
        assert_eq!(
            content_disposition("report.pdf"),
            "attachment; filename=\"report.pdf\"; filename*=UTF-8''report.pdf"
        );
        assert_eq!(
            content_disposition("my file (1).mp4"),
            "attachment; filename=\"my file (1).mp4\"; filename*=UTF-8''my%20file%20%281%29.mp4"
        );
        assert_eq!(
            content_disposition("фото.jpg"),
            "attachment; filename=\"____.jpg\"; filename*=UTF-8''%D1%84%D0%BE%D1%82%D0%BE.jpg"
        );
        // Quotes, backslashes and percent signs can't break out of the header.
        let tricky = content_disposition("a\"b\\c%41;d.txt");
        assert!(tricky.starts_with("attachment; filename=\"a_b_c_41;d.txt\";"));
        assert!(tricky.ends_with("filename*=UTF-8''a%22b%5Cc%2541%3Bd.txt"));
        assert!(HeaderValue::from_str(&content_disposition("日本 📷\u{7f}.txt")).is_ok());
    }
}
