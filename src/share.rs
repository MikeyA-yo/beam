//! What Beam shares: one folder, which the person at the PC can switch
//! while Beam runs, plus single files given on the command line, which are
//! served from where they are instead of being copied.
//!
//! Only the PC may change any of this. The phone holds the same cookie, so
//! the cookie can't tell them apart, but where the request comes from can:
//! the PC's page is opened at `127.0.0.1`, and nothing on the network can
//! connect from the PC's loopback address.

use std::fs;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock};

use axum::Json;
use axum::extract::{ConnectInfo, Path as UrlPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::watch;

use crate::config::SharedDir;
use crate::events::Event;
use crate::server::AppState;
use crate::upload;

/// Folders kept for switching back to, most recent first.
const MAX_RECENT: usize = 8;

/// A file shared from outside the shared folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Extra {
    /// Stable for the run, so the PC page can stop sharing the right one.
    pub id: u32,
    /// The file's own name. The phone sees it numbered if the shared
    /// folder has a file of the same name.
    pub name: String,
    /// Absolute, for showing on the PC.
    pub display: PathBuf,
    /// Canonical, for serving. Checked again before every use.
    pub path: PathBuf,
}

/// A consistent copy of what's shared, for one listing or lookup.
#[derive(Debug, Clone, Default)]
pub struct View {
    /// Canonical shared folder.
    pub dir: PathBuf,
    pub extras: Vec<Extra>,
}

impl View {
    /// Just a folder, as in most tests.
    pub fn of(dir: PathBuf) -> Self {
        Self {
            dir,
            extras: Vec::new(),
        }
    }
}

pub struct Share {
    folder: watch::Sender<Arc<SharedDir>>,
    extras: RwLock<Extras>,
    recent: Mutex<Vec<PathBuf>>,
}

#[derive(Default)]
struct Extras {
    next_id: u32,
    list: Vec<Extra>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum FolderError {
    #[error("enter the path of a folder")]
    Empty,
    #[error("enter a full path, starting with the drive, like D:\\Movies")]
    Relative,
    #[error("that folder doesn't exist")]
    NotFound,
    #[error("that's a file, not a folder. Enter the folder it's in")]
    IsFile,
    #[error("Beam isn't allowed to open that folder")]
    Denied,
    #[error("could not open that folder ({0})")]
    Other(String),
}

impl From<io::Error> for FolderError {
    fn from(e: io::Error) -> Self {
        match e.kind() {
            io::ErrorKind::NotFound => Self::NotFound,
            io::ErrorKind::PermissionDenied => Self::Denied,
            _ => Self::Other(e.to_string()),
        }
    }
}

#[derive(Debug, Error)]
pub enum AddError {
    #[error("{0} was not found")]
    NotFound(PathBuf),
    #[error("{0} is a folder; share a folder with --dir")]
    IsFolder(PathBuf),
    #[error("could not share {path}: {source}")]
    Io { path: PathBuf, source: io::Error },
}

#[derive(Debug, PartialEq, Eq)]
pub enum Added {
    Shared(Extra),
    /// The file is in the shared folder, so it's listed anyway.
    InFolder(String),
    /// The same file was given twice.
    AlreadyShared(String),
}

impl Share {
    pub fn new(folder: SharedDir) -> Self {
        Self {
            folder: watch::channel(Arc::new(folder)).0,
            extras: RwLock::new(Extras::default()),
            recent: Mutex::new(Vec::new()),
        }
    }

    pub fn folder(&self) -> Arc<SharedDir> {
        self.folder.borrow().clone()
    }

    /// The canonical shared folder.
    pub fn dir(&self) -> PathBuf {
        self.folder.borrow().canonical.clone()
    }

    /// Fires whenever the folder is switched. Used by the watcher.
    pub fn subscribe(&self) -> watch::Receiver<Arc<SharedDir>> {
        self.folder.subscribe()
    }

    pub fn view(&self) -> View {
        View {
            dir: self.dir(),
            extras: self.read_extras().list.clone(),
        }
    }

    pub fn extras(&self) -> Vec<Extra> {
        self.read_extras().list.clone()
    }

    /// Earlier folders of this run, most recent first.
    pub fn recent(&self) -> Vec<PathBuf> {
        lock(&self.recent).clone()
    }

    /// Switches the shared folder to `input`, a path as typed or pasted on
    /// the PC. The folder must exist: a typo shouldn't create a folder.
    /// Blocking.
    pub fn set_folder(&self, input: &str) -> Result<Arc<SharedDir>, FolderError> {
        let path = parse_path(input)?;
        if !fs::metadata(&path)?.is_dir() {
            return Err(FolderError::IsFile);
        }
        let new = Arc::new(SharedDir {
            display: std::path::absolute(&path)?,
            canonical: fs::canonicalize(&path)?,
        });
        // An upload killed mid-way in that folder in an earlier run.
        upload::remove_stale_parts(&new.canonical);

        let old = self.folder.send_replace(new.clone());
        let mut recent = lock(&self.recent);
        recent.retain(|p| !same_folder(p, &new.display));
        if old.canonical != new.canonical {
            recent.retain(|p| !same_folder(p, &old.display));
            recent.insert(0, old.display.clone());
            recent.truncate(MAX_RECENT);
        }
        Ok(new)
    }

    /// Shares `path` from where it is: nothing is copied. Blocking.
    pub fn add_file(&self, path: &Path) -> Result<Added, AddError> {
        let io_err = |source| AddError::Io {
            path: path.to_path_buf(),
            source,
        };
        let meta = match fs::metadata(path) {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Err(AddError::NotFound(path.to_path_buf()));
            }
            Err(e) => return Err(io_err(e)),
        };
        if meta.is_dir() {
            return Err(AddError::IsFolder(path.to_path_buf()));
        }
        let canonical = fs::canonicalize(path).map_err(io_err)?;
        let name = canonical
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if canonical.parent() == Some(self.dir().as_path()) {
            return Ok(Added::InFolder(name));
        }
        let display = std::path::absolute(path).map_err(io_err)?;

        let mut extras = self.write_extras();
        if extras.list.iter().any(|e| e.path == canonical) {
            return Ok(Added::AlreadyShared(name));
        }
        let extra = Extra {
            id: extras.next_id,
            name,
            display,
            path: canonical,
        };
        extras.next_id += 1;
        extras.list.push(extra.clone());
        Ok(Added::Shared(extra))
    }

    /// Stops sharing a file added with [`Self::add_file`].
    pub fn remove_file(&self, id: u32) -> Option<Extra> {
        let mut extras = self.write_extras();
        let at = extras.list.iter().position(|e| e.id == id)?;
        Some(extras.list.remove(at))
    }

    // A panic while holding these locks can't leave the data half-updated
    // (each update is a single push, remove or replace), so a poisoned lock
    // is still safe to use.
    fn read_extras(&self) -> std::sync::RwLockReadGuard<'_, Extras> {
        self.extras.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn write_extras(&self) -> std::sync::RwLockWriteGuard<'_, Extras> {
        self.extras.write().unwrap_or_else(PoisonError::into_inner)
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A path as typed or pasted. Explorer's "Copy as path" wraps it in quotes.
fn parse_path(input: &str) -> Result<PathBuf, FolderError> {
    let trimmed = input.trim();
    let unquoted = trimmed
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(trimmed)
        .trim();
    if unquoted.is_empty() {
        return Err(FolderError::Empty);
    }
    let path = PathBuf::from(unquoted);
    // Relative to wherever Beam was started from, which the person at the
    // PC can't see, so it would be a guess.
    if !path.is_absolute() {
        return Err(FolderError::Relative);
    }
    Ok(path)
}

/// Windows paths don't care about case.
fn same_folder(a: &Path, b: &Path) -> bool {
    if cfg!(windows) {
        a.as_os_str().eq_ignore_ascii_case(b.as_os_str())
    } else {
        a == b
    }
}

/// See the module docs. Every route that changes what's shared checks this.
pub fn from_this_pc(peer: &SocketAddr) -> bool {
    peer.ip().is_loopback()
}

// ---- Handlers, for the PC's page only ----

#[derive(Serialize)]
pub struct ShareInfo {
    pub folder: String,
    pub recent: Vec<String>,
    pub files: Vec<SharedFile>,
}

#[derive(Serialize)]
pub struct SharedFile {
    pub id: u32,
    pub name: String,
    pub path: String,
}

impl ShareInfo {
    fn of(share: &Share) -> Self {
        Self {
            folder: share.folder().display.to_string_lossy().into_owned(),
            recent: share
                .recent()
                .iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect(),
            files: share
                .extras()
                .into_iter()
                .map(|e| SharedFile {
                    id: e.id,
                    name: e.name,
                    path: e.display.to_string_lossy().into_owned(),
                })
                .collect(),
        }
    }
}

#[derive(Deserialize)]
pub struct FolderRequest {
    pub path: String,
}

fn error(status: StatusCode, message: impl ToString) -> Response {
    #[derive(Serialize)]
    struct ErrorBody {
        error: String,
    }
    let body = ErrorBody {
        error: message.to_string(),
    };
    (status, Json(body)).into_response()
}

fn not_from_this_pc(peer: &SocketAddr) -> Response {
    tracing::warn!(
        "Refused {}: only this PC can change what's shared",
        peer.ip()
    );
    error(
        StatusCode::FORBIDDEN,
        "only the PC running Beam can change what's shared",
    )
}

/// `GET /api/share`: the shared folder, earlier folders, and extra files.
pub async fn get_handler(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
) -> Response {
    if !from_this_pc(&peer) {
        return not_from_this_pc(&peer);
    }
    Json(ShareInfo::of(&state.share)).into_response()
}

/// `PUT /api/share/folder` with `{"path": "..."}`: share another folder.
/// JSON only, so a page on another site can't send it without the
/// browser asking first (and the cookie is `SameSite=Strict` anyway).
pub async fn folder_handler(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Json(req): Json<FolderRequest>,
) -> Response {
    if !from_this_pc(&peer) {
        return not_from_this_pc(&peer);
    }
    let share = state.share.clone();
    let result = tokio::task::spawn_blocking(move || share.set_folder(&req.path)).await;
    match result {
        Ok(Ok(folder)) => {
            tracing::info!("Shared folder is now {}", folder.display.display());
            Json(ShareInfo::of(&state.share)).into_response()
        }
        Ok(Err(e)) => {
            tracing::info!("Shared folder not changed: {e}");
            error(StatusCode::BAD_REQUEST, e)
        }
        Err(e) => {
            tracing::error!("folder change task failed: {e}");
            error(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
        }
    }
}

/// `DELETE /api/share/files/{id}`: stop sharing a file from elsewhere.
pub async fn remove_handler(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    UrlPath(id): UrlPath<u32>,
) -> Response {
    if !from_this_pc(&peer) {
        return not_from_this_pc(&peer);
    }
    match state.share.remove_file(id) {
        Some(extra) => {
            tracing::info!("Stopped sharing {:?}", extra.name);
            // Its name on the phone may have been numbered; a full refetch
            // is simpler than working that out.
            let _ = state.events.send(Event::Resync);
            Json(ShareInfo::of(&state.share)).into_response()
        }
        None => error(StatusCode::NOT_FOUND, "that file isn't shared"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn share_of(dir: &Path) -> Share {
        Share::new(SharedDir {
            display: dir.to_path_buf(),
            canonical: dir.canonicalize().unwrap(),
        })
    }

    #[test]
    fn pasted_paths_are_cleaned_up() {
        let abs = if cfg!(windows) {
            r"D:\Movies"
        } else {
            "/media/movies"
        };
        for input in [
            abs.to_owned(),
            format!("  \"{abs}\"  "),
            format!("\"{abs}\"\n"),
        ] {
            assert_eq!(parse_path(&input), Ok(PathBuf::from(abs)), "{input:?}");
        }
        assert_eq!(parse_path("   "), Err(FolderError::Empty));
        assert_eq!(parse_path("\"\""), Err(FolderError::Empty));
        assert_eq!(parse_path("Movies"), Err(FolderError::Relative));
        assert_eq!(parse_path(r"..\..\Windows"), Err(FolderError::Relative));
    }

    #[test]
    fn switching_folders_keeps_the_earlier_ones_for_later() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let share = share_of(a.path());
        let path_b = b.path().to_string_lossy().into_owned();

        let now = share.set_folder(&path_b).unwrap();
        assert_eq!(now.canonical, b.path().canonicalize().unwrap());
        assert_eq!(share.dir(), now.canonical);
        assert_eq!(share.recent(), [a.path().to_path_buf()]);

        // Back to the first: it leaves the list, and the second joins it.
        share.set_folder(&a.path().to_string_lossy()).unwrap();
        assert_eq!(share.recent(), std::slice::from_ref(&now.display));

        // The same folder again changes nothing.
        share.set_folder(&a.path().to_string_lossy()).unwrap();
        assert_eq!(share.recent().len(), 1);
    }

    #[test]
    fn refuses_missing_folders_and_files() {
        let tmp = tempfile::tempdir().unwrap();
        let share = share_of(tmp.path());
        let missing = tmp.path().join("typo");
        assert_eq!(
            share.set_folder(&missing.to_string_lossy()).unwrap_err(),
            FolderError::NotFound
        );
        assert!(!missing.exists(), "a typo must not create a folder");

        let file = tmp.path().join("notes.txt");
        fs::write(&file, "x").unwrap();
        assert_eq!(
            share.set_folder(&file.to_string_lossy()).unwrap_err(),
            FolderError::IsFile
        );
        // Still sharing the original folder.
        assert_eq!(share.dir(), tmp.path().canonicalize().unwrap());
        assert!(share.recent().is_empty());
    }

    #[tokio::test]
    async fn the_watcher_hears_about_a_switch() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let share = share_of(a.path());
        let mut rx = share.subscribe();
        share.set_folder(&b.path().to_string_lossy()).unwrap();
        rx.changed().await.unwrap();
        assert_eq!(rx.borrow().canonical, b.path().canonicalize().unwrap());
    }

    #[test]
    fn files_from_elsewhere_are_shared_in_place() {
        let shared = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let movie = elsewhere.path().join("movie.mkv");
        fs::write(&movie, "frames").unwrap();
        let share = share_of(shared.path());

        let Added::Shared(extra) = share.add_file(&movie).unwrap() else {
            panic!("expected the file to be shared");
        };
        assert_eq!(extra.name, "movie.mkv");
        assert_eq!(extra.path, movie.canonicalize().unwrap());
        // Nothing was copied or linked into the shared folder.
        assert_eq!(fs::read_dir(shared.path()).unwrap().count(), 0);

        assert_eq!(
            share.add_file(&movie).unwrap(),
            Added::AlreadyShared("movie.mkv".into())
        );
        assert_eq!(share.extras().len(), 1);

        assert_eq!(share.remove_file(extra.id), Some(extra.clone()));
        assert_eq!(share.remove_file(extra.id), None);
        assert!(share.view().extras.is_empty());
    }

    #[test]
    fn add_file_refuses_missing_files_and_folders() {
        let tmp = tempfile::tempdir().unwrap();
        let share = share_of(tmp.path());
        assert!(matches!(
            share.add_file(&tmp.path().join("nope.txt")),
            Err(AddError::NotFound(_))
        ));
        let sub = tmp.path().join("sub");
        fs::create_dir(&sub).unwrap();
        assert!(matches!(share.add_file(&sub), Err(AddError::IsFolder(_))));
    }

    #[test]
    fn a_file_already_in_the_folder_is_left_alone() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("here.txt"), "x").unwrap();
        let share = share_of(tmp.path());
        assert_eq!(
            share.add_file(&tmp.path().join("here.txt")).unwrap(),
            Added::InFolder("here.txt".into())
        );
        assert!(share.extras().is_empty());
    }

    #[test]
    fn only_the_pc_itself_may_change_the_share() {
        let at = |s: &str| from_this_pc(&s.parse().unwrap());
        assert!(at("127.0.0.1:50000"));
        assert!(at("[::1]:50000"));
        assert!(!at("192.168.1.23:50000"));
        assert!(!at("10.0.0.5:50000"));
    }
}
