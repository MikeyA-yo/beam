//! PC-to-phone side: listing the shared folder, serving downloads, and
//! adding files passed on the command line.
//!
//! A name only resolves if it shows up in a directory listing. That one
//! rule blocks traversal (`../x`, `a/b`), absolute paths, Windows device
//! names (`CON`), and alternate data streams (`file.txt:stream`), because
//! none of those can appear as an entry name. The resolved path is then
//! canonicalized and checked against the canonical shared folder, so a
//! symlink pointing outside is refused too.

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
use thiserror::Error;
use tower_http::services::ServeFile;

use crate::sanitize::{is_partial, sanitize_filename};
use crate::server::AppState;
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

/// Files in `dir` (top level only), newest first.
pub fn list(dir: &Path) -> io::Result<Vec<FileEntry>> {
    scan(dir).map(|s| s.files)
}

pub fn scan(dir: &Path) -> io::Result<Scan> {
    let mut scan = Scan::default();
    for entry in fs::read_dir(dir)?.filter_map(Result::ok) {
        let Some(v) = inspect(&entry, dir) else {
            continue;
        };
        if is_being_written(&v) {
            scan.busy += 1;
            continue;
        }
        scan.files.push(FileEntry {
            modified: modified_secs(&v.meta),
            size: v.meta.len(),
            name: v.name,
        });
    }
    scan.files.sort_by(|a, b| {
        b.modified
            .cmp(&a.modified)
            .then_with(|| a.name.cmp(&b.name))
    });
    Ok(scan)
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
pub fn resolve(dir: &Path, name: &str) -> Option<PathBuf> {
    // Cheap rejections before scanning the folder. The scan below is the
    // real guarantee; these only skip pointless work.
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\', '\0']) {
        return None;
    }
    fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .find(|entry| entry.file_name() == name)
        .and_then(|entry| inspect(&entry, dir))
        .filter(|v| !is_being_written(v))
        .map(|v| v.path)
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
    let dir = state.dir.clone();
    match tokio::task::spawn_blocking(move || list(&dir)).await {
        Ok(Ok(files)) => Json(FileList { files }).into_response(),
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
    let dir = state.dir.clone();
    let lookup = name.clone();
    let path = tokio::task::spawn_blocking(move || resolve(&dir, &lookup))
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

// ---- Files passed on the command line (FR-21) ----

#[derive(Debug, Error)]
pub enum ShareError {
    #[error("{0} was not found")]
    NotFound(PathBuf),
    #[error("{0} is a folder; only files can be shared")]
    IsFolder(PathBuf),
    #[error("could not add {path}: {source}")]
    Io { path: PathBuf, source: io::Error },
    #[error("could not add {path}: {source}")]
    Store {
        path: PathBuf,
        source: upload::UploadError,
    },
}

#[derive(Debug, PartialEq, Eq)]
pub enum Shared {
    /// Hard link: instant, uses no extra disk space.
    Linked(String),
    /// Copied, because the file is on another drive.
    Copied(String),
    /// The file is already in the shared folder.
    AlreadyThere(String),
}

/// Puts `src` into the shared folder under a free name, without ever
/// overwriting. Tries a hard link first and falls back to copying. Blocking.
pub fn add_to_share(src: &Path, dir: &Path) -> Result<Shared, ShareError> {
    let io_err = |source| ShareError::Io {
        path: src.to_path_buf(),
        source,
    };
    let meta = match fs::metadata(src) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err(ShareError::NotFound(src.to_path_buf()));
        }
        Err(e) => return Err(io_err(e)),
    };
    if meta.is_dir() {
        return Err(ShareError::IsFolder(src.to_path_buf()));
    }
    let canonical = fs::canonicalize(src).map_err(io_err)?;
    let original_name = canonical
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    if canonical.parent() == Some(dir) {
        return Ok(Shared::AlreadyThere(original_name));
    }
    let name = sanitize_filename(&original_name);

    // `hard_link` fails if the target exists, so it can't overwrite.
    for n in 0..10_000 {
        let target = dir.join(upload::numbered_name(&name, n));
        match fs::hard_link(&canonical, &target) {
            Ok(()) => return Ok(Shared::Linked(file_name_of(&target))),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            // Different drive, or a filesystem without hard links (FAT, exFAT).
            Err(_) => break,
        }
    }

    copy_into_share(&canonical, dir, &name).map(Shared::Copied)
}

/// Copies into a temp file, so a half-finished copy is never listed, then
/// claims a free name the same way uploads do. Returns the name used.
fn copy_into_share(src: &Path, dir: &Path, name: &str) -> Result<String, ShareError> {
    let part = dir.join(upload::part_name(name, rand::random()));
    let placed = fs::copy(src, &part)
        .map_err(|source| ShareError::Io {
            path: src.to_path_buf(),
            source,
        })
        .and_then(|_| {
            upload::move_into_place(&part, dir, name).map_err(|source| ShareError::Store {
                path: src.to_path_buf(),
                source,
            })
        });
    match placed {
        Ok(target) => Ok(file_name_of(&target)),
        Err(e) => {
            let _ = fs::remove_file(&part);
            Err(e)
        }
    }
}

fn file_name_of(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
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

    fn names(dir: &Path) -> Vec<String> {
        list(dir).unwrap().into_iter().map(|f| f.name).collect()
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

    #[test]
    fn adds_a_file_without_overwriting() {
        let src_dir = tempfile::tempdir().unwrap();
        let src = src_dir.path().join("movie.mp4");
        fs::write(&src, b"movie").unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let dir = canon(&tmp);
        fs::write(dir.join("movie.mp4"), b"existing").unwrap();

        let result = add_to_share(&src, &dir).unwrap();
        let name = match &result {
            Shared::Linked(n) | Shared::Copied(n) => n.clone(),
            other => panic!("unexpected {other:?}"),
        };
        assert_eq!(name, "movie (1).mp4");
        assert_eq!(fs::read(dir.join("movie.mp4")).unwrap(), b"existing");
        assert_eq!(fs::read(dir.join(&name)).unwrap(), b"movie");
        assert_eq!(names(&dir).len(), 2, "no temp file may remain");
    }

    /// The other-drive path, which a same-drive test can't reach through
    /// `add_to_share` because the hard link always works there.
    #[test]
    fn copy_fallback_never_overwrites_and_leaves_no_temp_file() {
        let src_dir = tempfile::tempdir().unwrap();
        let src = src_dir.path().join("doc.pdf");
        fs::write(&src, b"new copy").unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let dir = canon(&tmp);
        fs::write(dir.join("doc.pdf"), b"existing").unwrap();

        assert_eq!(
            copy_into_share(&src, &dir, "doc.pdf").unwrap(),
            "doc (1).pdf"
        );
        assert_eq!(fs::read(dir.join("doc.pdf")).unwrap(), b"existing");
        assert_eq!(fs::read(dir.join("doc (1).pdf")).unwrap(), b"new copy");
        let all: Vec<_> = fs::read_dir(&dir).unwrap().collect();
        assert_eq!(all.len(), 2, "no temp file may remain");

        // A failed copy cleans up after itself too.
        assert!(copy_into_share(&src_dir.path().join("gone.pdf"), &dir, "gone.pdf").is_err());
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 2);
    }

    #[test]
    fn rejects_missing_files_and_folders() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = canon(&tmp);
        let missing = tmp.path().join("nope.txt");
        assert!(matches!(
            add_to_share(&missing, &dir),
            Err(ShareError::NotFound(_))
        ));
        let folder = tempfile::tempdir().unwrap();
        assert!(matches!(
            add_to_share(folder.path(), &dir),
            Err(ShareError::IsFolder(_))
        ));
    }

    #[test]
    fn a_file_already_in_the_folder_is_left_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = canon(&tmp);
        fs::write(dir.join("here.txt"), b"x").unwrap();
        assert_eq!(
            add_to_share(&dir.join("here.txt"), &dir).unwrap(),
            Shared::AlreadyThere("here.txt".into())
        );
        assert_eq!(names(&dir), ["here.txt"]);
    }
}
