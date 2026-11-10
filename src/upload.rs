//! Phone-to-PC uploads, streamed to disk.
//!
//! Each file goes to `<name>.beam-<id>.part` while it arrives, then is renamed
//! to a free name. A guard deletes the temp file if anything goes wrong,
//! including the client vanishing or the request future being dropped.

use std::fs::OpenOptions;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use axum::Json;
use axum::extract::multipart::{Field, MultipartError};
use axum::extract::{ConnectInfo, Multipart, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use thiserror::Error;
use tokio::io::{AsyncWriteExt, BufWriter};

use crate::human;
use crate::sanitize::sanitize_filename;
use crate::server::AppState;

/// Write buffer per upload. Large enough that disk writes are not the
/// bottleneck on fast Wi-Fi, small enough that memory stays flat with
/// several uploads at once.
const WRITE_BUFFER: usize = 512 * 1024;

/// An upload that sends nothing for this long is treated as dead. Phones
/// that drop off Wi-Fi often never close the connection.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// How many `name (n).ext` variants to try before giving up.
const MAX_NAME_ATTEMPTS: u32 = 10_000;

/// Leftover temp files older than this are removed at startup. The age
/// check keeps a second Beam instance on the same folder from deleting
/// this one's in-progress uploads.
const STALE_PART_AGE: Duration = Duration::from_secs(5 * 60);

const PART_MARKER: &str = ".beam-";
const PART_SUFFIX: &str = ".part";

#[derive(Debug, Error)]
pub enum UploadError {
    #[error("the PC's disk is full")]
    DiskFull,
    #[error("the upload was interrupted")]
    Interrupted(#[source] MultipartError),
    #[error("the upload stalled for {} seconds", IDLE_TIMEOUT.as_secs())]
    Stalled,
    #[error("the request contained no files")]
    NoFiles,
    #[error("could not find a free filename")]
    NoFreeName,
    #[error("could not save the file: {0}")]
    Io(#[source] io::Error),
}

impl From<io::Error> for UploadError {
    fn from(e: io::Error) -> Self {
        if e.kind() == io::ErrorKind::StorageFull {
            Self::DiskFull
        } else {
            Self::Io(e)
        }
    }
}

impl UploadError {
    fn status(&self) -> StatusCode {
        match self {
            Self::DiskFull => StatusCode::INSUFFICIENT_STORAGE,
            Self::Interrupted(_) | Self::NoFiles => StatusCode::BAD_REQUEST,
            Self::Stalled => StatusCode::REQUEST_TIMEOUT,
            Self::NoFreeName | Self::Io(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl IntoResponse for UploadError {
    fn into_response(self) -> Response {
        let body = ErrorBody {
            error: self.to_string(),
        };
        (self.status(), Json(body)).into_response()
    }
}

#[derive(Serialize)]
struct ErrorBody {
    error: String,
}

#[derive(Debug, Serialize)]
pub struct Saved {
    /// Name on disk, which differs from what was sent if it was sanitized
    /// or renamed to avoid a collision.
    pub name: String,
    pub size: u64,
}

#[derive(Serialize)]
pub struct UploadResponse {
    pub files: Vec<Saved>,
}

/// `POST /api/upload`. Each file part of the multipart body is saved in turn.
/// The page sends one file per request so each gets its own progress bar,
/// but several in one request work too.
pub async fn handle(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    mut multipart: Multipart,
) -> Result<Json<UploadResponse>, UploadError> {
    let mut files = Vec::new();
    loop {
        let field = match multipart.next_field().await {
            Ok(Some(field)) => field,
            Ok(None) => break,
            Err(e) => {
                tracing::warn!("Upload from {} failed: the connection dropped", peer.ip());
                tracing::debug!("multipart error: {e}");
                return Err(UploadError::Interrupted(e));
            }
        };
        // Fields without a filename are form values, not files.
        let Some(sent_name) = field.file_name().map(str::to_owned) else {
            continue;
        };
        let started = Instant::now();
        match save_field(field, &state.dir, &sent_name).await {
            Ok(saved) => {
                log_received(&saved, &sent_name, peer, started.elapsed());
                files.push(saved);
            }
            Err(e) => {
                tracing::warn!(
                    "Upload of {sent_name:?} from {} failed: {e}. The partial file was removed.",
                    peer.ip()
                );
                if let Some(source) = std::error::Error::source(&e) {
                    tracing::debug!("cause: {source}");
                }
                return Err(e);
            }
        }
    }
    if files.is_empty() {
        return Err(UploadError::NoFiles);
    }
    Ok(Json(UploadResponse { files }))
}

fn log_received(saved: &Saved, sent_name: &str, peer: SocketAddr, elapsed: Duration) {
    let renamed = if saved.name == sent_name {
        String::new()
    } else {
        format!(" (saved as {:?})", saved.name)
    };
    tracing::info!(
        "Received {sent_name:?}{renamed} from {}: {} in {}, {}",
        peer.ip(),
        human::bytes(saved.size),
        human::duration(elapsed),
        human::rate(saved.size, elapsed),
    );
}

/// Streams one multipart field to a temp file, then moves it into place.
async fn save_field(
    mut field: Field<'_>,
    dir: &Path,
    sent_name: &str,
) -> Result<Saved, UploadError> {
    let name = sanitize_filename(sent_name);
    let (part_path, file) = create_part_file(dir, &name).await?;
    let mut guard = PartGuard::new(part_path.clone(), sent_name);

    let result = async {
        let size = stream_to_file(&mut field, file).await?;
        let dir = dir.to_path_buf();
        let final_path =
            tokio::task::spawn_blocking(move || move_into_place(&part_path, &dir, &name))
                .await
                .map_err(|e| UploadError::Io(io::Error::other(e)))??;
        Ok::<_, UploadError>((final_path, size))
    }
    .await;

    match result {
        Ok((final_path, size)) => {
            guard.disarm();
            let name = final_path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            Ok(Saved { name, size })
        }
        Err(e) => {
            // The caller logs the reason. The guard still deletes the file.
            guard.quiet();
            Err(e)
        }
    }
}

/// Copies the field's chunks into `file` and closes it. Returns the byte count.
async fn stream_to_file(field: &mut Field<'_>, file: std::fs::File) -> Result<u64, UploadError> {
    let mut out = BufWriter::with_capacity(WRITE_BUFFER, tokio::fs::File::from_std(file));
    let mut size: u64 = 0;
    loop {
        let chunk = match tokio::time::timeout(IDLE_TIMEOUT, field.chunk()).await {
            Err(_) => return Err(UploadError::Stalled),
            Ok(Err(e)) => return Err(UploadError::Interrupted(e)),
            Ok(Ok(None)) => break,
            Ok(Ok(Some(chunk))) => chunk,
        };
        size += chunk.len() as u64;
        out.write_all(&chunk).await?;
    }
    out.flush().await?;
    // Wait for tokio's background write to finish and close the handle.
    // Windows can't rename a file that is still open.
    drop(out.into_inner().into_std().await);
    Ok(size)
}

/// Creates `<name>.beam-<id>.part` with `create_new`, so it never opens an
/// existing file, including through a symlink someone planted.
async fn create_part_file(dir: &Path, name: &str) -> Result<(PathBuf, std::fs::File), UploadError> {
    let dir = dir.to_path_buf();
    let name = name.to_owned();
    tokio::task::spawn_blocking(move || {
        // The random id makes collisions practically impossible. Retry a few
        // times anyway rather than fail on one.
        let mut last_err = None;
        for _ in 0..8 {
            let path = dir.join(part_name(&name, rand::random()));
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => return Ok((path, file)),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => last_err = Some(e),
                Err(e) => return Err(e.into()),
            }
        }
        Err(last_err.map_or(UploadError::NoFreeName, UploadError::from))
    })
    .await
    .map_err(|e| UploadError::Io(io::Error::other(e)))?
}

pub fn part_name(name: &str, id: u32) -> String {
    format!("{name}{PART_MARKER}{id:08x}{PART_SUFFIX}")
}

/// True only for temp files this program creates, so the startup sweep
/// never touches someone else's `.part` file (Firefox uses that suffix too).
pub fn is_beam_part(file_name: &str) -> bool {
    file_name
        .strip_suffix(PART_SUFFIX)
        .and_then(|rest| rest.rsplit_once(PART_MARKER))
        .is_some_and(|(_, id)| id.len() == 8 && id.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// Moves a finished temp file to the first free name among `name`,
/// `name (1).ext`, `name (2).ext`, ...
///
/// `rename` replaces whatever is at the target, so each name is claimed
/// first with `create_new`, which fails atomically if anything exists there.
/// The rename then replaces only the empty file we just made. An existing
/// file is never overwritten, even by another upload finishing at the
/// same moment.
pub fn move_into_place(part: &Path, dir: &Path, name: &str) -> Result<PathBuf, UploadError> {
    for n in 0..MAX_NAME_ATTEMPTS {
        let target = dir.join(numbered_name(name, n));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target)
        {
            Ok(placeholder) => {
                drop(placeholder);
                if let Err(e) = std::fs::rename(part, &target) {
                    let _ = std::fs::remove_file(&target);
                    return Err(e.into());
                }
                return Ok(target);
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            // Windows reports a folder with that name as access denied.
            Err(e)
                if e.kind() == io::ErrorKind::PermissionDenied
                    && std::fs::symlink_metadata(&target).is_ok() => {}
            Err(e) => return Err(e.into()),
        }
    }
    Err(UploadError::NoFreeName)
}

/// `photo.jpg` → `photo (n).jpg`. Names without an extension, or dotfiles
/// like `.env`, get the number at the end.
pub fn numbered_name(name: &str, n: u32) -> String {
    if n == 0 {
        return name.to_owned();
    }
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => format!("{stem} ({n}).{ext}"),
        _ => format!("{name} ({n})"),
    }
}

/// Removes temp files left by a previous run that was killed mid-upload.
/// Returns how many were removed.
pub fn remove_stale_parts(dir: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let now = SystemTime::now();
    entries
        .filter_map(Result::ok)
        .filter(|entry| is_beam_part(&entry.file_name().to_string_lossy()))
        .filter(|entry| {
            entry
                .metadata()
                .ok()
                .filter(std::fs::Metadata::is_file)
                .and_then(|m| m.modified().ok())
                .and_then(|modified| now.duration_since(modified).ok())
                .is_some_and(|age| age >= STALE_PART_AGE)
        })
        .filter(|entry| match std::fs::remove_file(entry.path()) {
            Ok(()) => true,
            Err(e) => {
                tracing::debug!("could not remove {}: {e}", entry.path().display());
                false
            }
        })
        .count()
}

/// Deletes the temp file when dropped, unless the upload succeeded.
///
/// Dropping can also happen without any error reaching our code, when the
/// server cancels the request (for example on Ctrl+C). The guard logs
/// that case itself, unless `quiet` says the caller already did.
struct PartGuard {
    path: Option<PathBuf>,
    sent_name: String,
    report: bool,
}

impl PartGuard {
    fn new(path: PathBuf, sent_name: &str) -> Self {
        Self {
            path: Some(path),
            sent_name: sent_name.to_owned(),
            report: true,
        }
    }

    fn disarm(&mut self) {
        self.path = None;
    }

    fn quiet(&mut self) {
        self.report = false;
    }
}

impl Drop for PartGuard {
    fn drop(&mut self) {
        let Some(path) = self.path.take() else {
            return;
        };
        match std::fs::remove_file(&path) {
            Ok(()) => {
                if self.report {
                    tracing::warn!(
                        "Upload of {:?} stopped before finishing. The partial file was removed.",
                        self.sent_name
                    );
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => {
                tracing::warn!("Could not remove unfinished upload {}: {e}", path.display());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names_in(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    fn write_part(dir: &Path, name: &str, body: &[u8]) -> PathBuf {
        let path = dir.join(part_name(name, rand::random()));
        std::fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn numbered_names() {
        assert_eq!(numbered_name("photo.jpg", 0), "photo.jpg");
        assert_eq!(numbered_name("photo.jpg", 1), "photo (1).jpg");
        assert_eq!(numbered_name("archive.tar.gz", 2), "archive.tar (2).gz");
        assert_eq!(numbered_name("README", 3), "README (3)");
        assert_eq!(numbered_name(".env", 1), ".env (1)");
    }

    #[test]
    fn part_names_are_recognized() {
        let name = part_name("video.mp4", 0xdead_beef);
        assert_eq!(name, "video.mp4.beam-deadbeef.part");
        assert!(is_beam_part(&name));
        assert!(!is_beam_part("video.mp4.part"));
        assert!(!is_beam_part("video.mp4.beam-xyz.part"));
        assert!(!is_beam_part("video.mp4"));
    }

    #[test]
    fn moves_to_the_requested_name_when_free() {
        let dir = tempfile::tempdir().unwrap();
        let part = write_part(dir.path(), "a.txt", b"hello");
        let target = move_into_place(&part, dir.path(), "a.txt").unwrap();
        assert_eq!(target, dir.path().join("a.txt"));
        assert_eq!(std::fs::read(&target).unwrap(), b"hello");
        assert_eq!(names_in(dir.path()), ["a.txt"]);
    }

    #[test]
    fn never_overwrites_and_numbers_collisions() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"original").unwrap();
        std::fs::write(dir.path().join("a (1).txt"), b"first copy").unwrap();

        let part = write_part(dir.path(), "a.txt", b"new");
        let target = move_into_place(&part, dir.path(), "a.txt").unwrap();

        assert_eq!(target, dir.path().join("a (2).txt"));
        assert_eq!(
            std::fs::read(dir.path().join("a.txt")).unwrap(),
            b"original"
        );
        assert_eq!(
            std::fs::read(dir.path().join("a (1).txt")).unwrap(),
            b"first copy"
        );
        assert_eq!(std::fs::read(&target).unwrap(), b"new");
    }

    #[test]
    fn skips_a_folder_with_the_same_name() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("photos")).unwrap();
        let part = write_part(dir.path(), "photos", b"x");
        let target = move_into_place(&part, dir.path(), "photos").unwrap();
        assert_eq!(target, dir.path().join("photos (1)"));
        assert!(dir.path().join("photos").is_dir());
    }

    #[test]
    fn guard_removes_the_file_unless_disarmed() {
        let dir = tempfile::tempdir().unwrap();
        let doomed = write_part(dir.path(), "a", b"x");
        let kept = write_part(dir.path(), "b", b"x");
        drop(PartGuard::new(doomed.clone(), "a"));
        let mut guard = PartGuard::new(kept.clone(), "b");
        guard.disarm();
        drop(guard);
        assert!(!doomed.exists());
        assert!(kept.exists());
    }

    #[test]
    fn stale_sweep_only_removes_old_beam_parts() {
        let dir = tempfile::tempdir().unwrap();
        let old = write_part(dir.path(), "old.mp4", b"x");
        let fresh = write_part(dir.path(), "fresh.mp4", b"x");
        let foreign = dir.path().join("firefox-download.zip.part");
        std::fs::write(&foreign, b"x").unwrap();
        std::fs::write(dir.path().join("keep.txt"), b"x").unwrap();

        let past = SystemTime::now() - STALE_PART_AGE - Duration::from_secs(60);
        for path in [&old, &foreign] {
            std::fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(past)
                .unwrap();
        }

        assert_eq!(remove_stale_parts(dir.path()), 1);
        assert!(!old.exists());
        assert!(fresh.exists(), "an in-progress upload must survive");
        assert!(foreign.exists(), "someone else's .part file must survive");
        assert!(dir.path().join("keep.txt").exists());
    }
}
