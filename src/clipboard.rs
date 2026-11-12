//! Clipboard sharing: the phone sends text or an image to the PC's
//! clipboard, and fetches what's on it when the user taps a button.
//!
//! Nothing is sent to the phone unless it asks, so a password copied on
//! the PC never reaches the phone on its own. Contents are never logged.
//!
//! All clipboard access runs on one long-lived thread. Windows ties the
//! clipboard to the thread that opens it, and on Linux (X11) the text we
//! set only stays pasteable while the arboard handle that set it is alive.

#[cfg(any(windows, test))]
mod dropfiles;
mod image;
#[cfg(windows)]
mod win;

#[cfg(any(windows, test))]
use std::fs::{self, File};
#[cfg(any(windows, test))]
use std::io::{self, Read};
use std::net::SocketAddr;
#[cfg(any(windows, test))]
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};

use axum::Json;
use axum::body::Bytes;
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use thiserror::Error;
use tokio::sync::oneshot;

pub use image::{Format, Image, ImageError, MAX_IMAGE_BYTES, SNIFF_LEN};

use crate::human;
use crate::server::AppState;

/// Request body limit for `PUT /api/clipboard`: the largest image plus its
/// BMP headers. Clipboard data lives in memory on both ends anyway, so
/// unlike file uploads this is read into memory whole.
pub const MAX_BODY_BYTES: usize = MAX_IMAGE_BYTES + 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Clip {
    Text(String),
    Image(Image),
}

impl Clip {
    /// For the terminal log. Describes the content without revealing it.
    pub fn describe(&self) -> String {
        match self {
            Clip::Text(text) => {
                let n = text.chars().count();
                format!("text, {n} character{}", if n == 1 { "" } else { "s" })
            }
            Clip::Image(img) => match img.dimensions() {
                Some((w, h)) => format!("image, {w}×{h}"),
                None => format!("image, {}", human::bytes(img.bytes.len() as u64)),
            },
        }
    }
}

#[derive(Debug, Error)]
pub enum ClipboardError {
    #[error("the PC's clipboard isn't available ({0})")]
    Unavailable(String),
    #[error("another app is using the PC's clipboard, try again")]
    Busy,
    #[error("sending images to the PC's clipboard only works on Windows for now")]
    ImagesUnsupported,
    #[error("only text and images can go on the clipboard")]
    UnsupportedType,
    #[error("the text isn't valid UTF-8")]
    BadText,
    #[error("{0}")]
    BadImage(#[from] ImageError),
    #[error("{0} files are copied on the PC. Copy just one image file")]
    SeveralFiles(usize),
    #[error(
        "the file copied on the PC isn't an image the phone can show (PNG, JPEG, GIF, WebP or BMP)"
    )]
    NotAnImage,
    #[error("could not read the file copied on the PC ({0})")]
    CopiedFileUnreadable(&'static str),
    #[error("could not use the PC's clipboard ({0})")]
    Failed(String),
    #[error("the clipboard helper stopped")]
    Stopped,
}

impl ClipboardError {
    fn status(&self) -> StatusCode {
        match self {
            Self::Unavailable(_) | Self::Busy => StatusCode::SERVICE_UNAVAILABLE,
            Self::ImagesUnsupported | Self::UnsupportedType => StatusCode::UNSUPPORTED_MEDIA_TYPE,
            Self::BadImage(ImageError::TooLarge) => StatusCode::PAYLOAD_TOO_LARGE,
            Self::BadText | Self::BadImage(_) => StatusCode::BAD_REQUEST,
            // The request is fine; what's on the PC's clipboard isn't usable.
            Self::SeveralFiles(_) | Self::NotAnImage | Self::CopiedFileUnreadable(_) => {
                StatusCode::CONFLICT
            }
            Self::Failed(_) | Self::Stopped => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl IntoResponse for ClipboardError {
    fn into_response(self) -> Response {
        #[derive(Serialize)]
        struct ErrorBody {
            error: String,
        }
        let body = ErrorBody {
            error: self.to_string(),
        };
        (self.status(), Json(body)).into_response()
    }
}

/// Where clipboard reads and writes go. The real one is [`System`];
/// tests use [`Memory`] so they never touch the PC's clipboard.
pub trait Backend: Send + 'static {
    fn read(&mut self) -> Result<Option<Clip>, ClipboardError>;
    fn write(&mut self, clip: &Clip) -> Result<(), ClipboardError>;
}

type Reply<T> = oneshot::Sender<Result<T, ClipboardError>>;

enum Job {
    Read(Reply<Option<Clip>>),
    Write(Clip, Reply<()>),
}

/// Handle to the clipboard thread. Cheap to clone. The thread ends when
/// the last handle is dropped.
#[derive(Clone)]
pub struct Clipboard {
    jobs: mpsc::Sender<Job>,
}

impl Clipboard {
    /// The PC's real clipboard. Nothing touches it until the first request.
    pub fn system() -> Self {
        Self::with_backend(System::default())
    }

    pub fn with_backend(backend: impl Backend) -> Self {
        let (jobs, queue) = mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("beam-clipboard".into())
            .spawn(move || run(backend, queue));
        if let Err(e) = spawned {
            // The queue was dropped with the closure, so every request
            // now fails with `Stopped` instead of hanging.
            tracing::error!("could not start the clipboard thread: {e}");
        }
        Self { jobs }
    }

    pub async fn read(&self) -> Result<Option<Clip>, ClipboardError> {
        let (reply, answer) = oneshot::channel();
        self.jobs
            .send(Job::Read(reply))
            .map_err(|_| ClipboardError::Stopped)?;
        answer.await.map_err(|_| ClipboardError::Stopped)?
    }

    pub async fn write(&self, clip: Clip) -> Result<(), ClipboardError> {
        let (reply, answer) = oneshot::channel();
        self.jobs
            .send(Job::Write(clip, reply))
            .map_err(|_| ClipboardError::Stopped)?;
        answer.await.map_err(|_| ClipboardError::Stopped)?
    }
}

fn run(mut backend: impl Backend, queue: mpsc::Receiver<Job>) {
    // A reply can only fail to send if the request was dropped, for
    // example because the phone disconnected. Nothing to do then.
    for job in queue {
        match job {
            Job::Read(reply) => {
                let _ = reply.send(contain(|| backend.read()));
            }
            Job::Write(clip, reply) => {
                let _ = reply.send(contain(|| backend.write(&clip)));
            }
        }
    }
}

/// Turns a panic in clipboard code into an error for that one request, so
/// the thread, and clipboard sharing with it, keeps going.
fn contain<T>(f: impl FnOnce() -> Result<T, ClipboardError>) -> Result<T, ClipboardError> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
        .unwrap_or_else(|_| Err(ClipboardError::Failed("internal error".into())))
}

/// The PC's clipboard: text through arboard on every platform, images
/// through the Win32 API on Windows only.
#[derive(Default)]
pub struct System {
    /// Opened on first use. If that fails (say, Linux with no display), the
    /// next request tries again.
    text: Option<arboard::Clipboard>,
}

impl System {
    fn text(&mut self) -> Result<&mut arboard::Clipboard, ClipboardError> {
        if self.text.is_none() {
            let clipboard = arboard::Clipboard::new()
                .map_err(|e| ClipboardError::Unavailable(e.to_string()))?;
            self.text = Some(clipboard);
        }
        self.text.as_mut().ok_or(ClipboardError::Stopped)
    }
}

impl Backend for System {
    fn read(&mut self) -> Result<Option<Clip>, ClipboardError> {
        // Text wins when there's both, as when copying cells from Excel.
        match self.text()?.get_text() {
            Ok(text) if !text.is_empty() => return Ok(Some(Clip::Text(text))),
            Ok(_) | Err(arboard::Error::ContentNotAvailable) => {}
            Err(e) => return Err(from_arboard(e)),
        }
        #[cfg(windows)]
        {
            if let Some(img) = win::read_image()? {
                return Ok(Some(Clip::Image(img)));
            }
            // Last, a file copied in Explorer, which puts only its path on
            // the clipboard. The clipboard is closed again by now, so a
            // slow disk doesn't hold it up for other apps.
            image_from_copied_files(&win::copied_files()?).map(|img| img.map(Clip::Image))
        }
        #[cfg(not(windows))]
        return Ok(None);
    }

    fn write(&mut self, clip: &Clip) -> Result<(), ClipboardError> {
        match clip {
            Clip::Text(text) => self.text()?.set_text(text.as_str()).map_err(from_arboard),
            #[cfg(windows)]
            Clip::Image(img) => win::write_image(img),
            #[cfg(not(windows))]
            Clip::Image(_) => Err(ClipboardError::ImagesUnsupported),
        }
    }
}

/// The image in a file copied in Explorer. Only one file is taken, so
/// what the phone gets is never ambiguous; a folder of photos belongs in
/// the shared folder instead.
#[cfg(any(windows, test))]
fn image_from_copied_files(paths: &[PathBuf]) -> Result<Option<Image>, ClipboardError> {
    match paths {
        [] => Ok(None),
        [path] => read_image_file(path, MAX_IMAGE_BYTES).map(Some),
        several => Err(ClipboardError::SeveralFiles(several.len())),
    }
}

/// Reads an image file, up to `limit` bytes. It's held in memory like any
/// clipboard image. The file's start is checked first, so a large video
/// copied by mistake is refused without reading it all.
#[cfg(any(windows, test))]
fn read_image_file(path: &Path, limit: usize) -> Result<Image, ClipboardError> {
    let meta = fs::metadata(path).map_err(unreadable)?;
    if !meta.is_file() {
        return Err(ClipboardError::NotAnImage);
    }
    let mut file = File::open(path).map_err(unreadable)?;
    let mut bytes = Vec::new();
    (&mut file)
        .take(SNIFF_LEN as u64)
        .read_to_end(&mut bytes)
        .map_err(unreadable)?;
    let format = Format::sniff(&bytes).ok_or(ClipboardError::NotAnImage)?;
    if meta.len() > limit as u64 {
        return Err(ImageError::TooLarge.into());
    }
    // One byte past the limit, in case the file grew since `metadata`.
    bytes.reserve(meta.len() as usize);
    (&mut file)
        .take((limit + 1 - bytes.len().min(limit)) as u64)
        .read_to_end(&mut bytes)
        .map_err(unreadable)?;
    if bytes.len() > limit {
        return Err(ImageError::TooLarge.into());
    }
    Ok(Image { format, bytes })
}

/// For the phone. The OS's message is left to the debug log because it
/// can include the path.
#[cfg(any(windows, test))]
fn unreadable(e: io::Error) -> ClipboardError {
    tracing::debug!("reading the copied file: {e}");
    ClipboardError::CopiedFileUnreadable(match e.kind() {
        io::ErrorKind::NotFound => "it isn't there any more",
        io::ErrorKind::PermissionDenied => "access denied",
        _ => "read error",
    })
}

fn from_arboard(e: arboard::Error) -> ClipboardError {
    match e {
        arboard::Error::ClipboardOccupied => ClipboardError::Busy,
        arboard::Error::ClipboardNotSupported => ClipboardError::Unavailable(e.to_string()),
        other => ClipboardError::Failed(other.to_string()),
    }
}

/// A clipboard that lives in memory, for tests. Clones share contents.
#[derive(Clone, Default)]
pub struct Memory(Arc<Mutex<Option<Clip>>>);

impl Memory {
    pub fn get(&self) -> Option<Clip> {
        self.0.lock().map(|c| c.clone()).unwrap_or_default()
    }

    pub fn set(&self, clip: Option<Clip>) {
        if let Ok(mut c) = self.0.lock() {
            *c = clip;
        }
    }
}

impl Backend for Memory {
    fn read(&mut self) -> Result<Option<Clip>, ClipboardError> {
        Ok(self.get())
    }

    fn write(&mut self, clip: &Clip) -> Result<(), ClipboardError> {
        self.set(Some(clip.clone()));
        Ok(())
    }
}

/// `GET /api/clipboard`: what's on the PC's clipboard right now. Text,
/// a PNG or a BMP, told apart by `Content-Type`. 204 if there's nothing
/// the phone can use, such as copied files.
pub async fn get_handler(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
) -> Response {
    match state.clipboard.read().await {
        Ok(Some(clip)) => {
            tracing::info!("Sent clipboard to {}: {}", peer.ip(), clip.describe());
            let (content_type, body) = match clip {
                Clip::Text(text) => ("text/plain; charset=utf-8", text.into_bytes()),
                Clip::Image(img) => (img.format.mime(), img.bytes),
            };
            (
                [
                    (header::CONTENT_TYPE, content_type),
                    (header::CACHE_CONTROL, "no-store"),
                ],
                body,
            )
                .into_response()
        }
        Ok(None) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => {
            tracing::warn!("Could not read the clipboard for {}: {e}", peer.ip());
            e.into_response()
        }
    }
}

/// `PUT /api/clipboard`: replaces the PC's clipboard with the body, either
/// `text/plain` or an `image/bmp` built by the page.
pub async fn put_handler(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let clip = match parse_body(&headers, body) {
        Ok(clip) => clip,
        Err(e) => {
            tracing::warn!("Refused clipboard from {}: {e}", peer.ip());
            return e.into_response();
        }
    };
    let what = clip.describe();
    match state.clipboard.write(clip).await {
        Ok(()) => {
            tracing::info!("Received clipboard from {}: {what}", peer.ip());
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) => {
            tracing::warn!("Could not set the clipboard from {}: {e}", peer.ip());
            e.into_response()
        }
    }
}

fn parse_body(headers: &HeaderMap, body: Bytes) -> Result<Clip, ClipboardError> {
    let mime = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .map(|v| v.trim().to_ascii_lowercase())
        .unwrap_or_default();
    match mime.as_str() {
        "text/plain" => String::from_utf8(body.into())
            .map(Clip::Text)
            .map_err(|_| ClipboardError::BadText),
        "image/bmp" => {
            image::dib_from_bmp(&body)?;
            Ok(Clip::Image(Image {
                format: Format::Bmp,
                bytes: body.into(),
            }))
        }
        _ => Err(ClipboardError::UnsupportedType),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describe_never_includes_the_content() {
        let secret = Clip::Text("hunter2 and more".into());
        assert_eq!(secret.describe(), "text, 16 characters");
        assert_eq!(Clip::Text("é".into()).describe(), "text, 1 character");
        let img = Clip::Image(Image {
            format: Format::Png,
            bytes: vec![1, 2, 3],
        });
        assert_eq!(img.describe(), "image, 3 B");
    }

    #[test]
    fn body_type_decides_text_or_image() {
        let with = |ct: &str| {
            let mut h = HeaderMap::new();
            h.insert(header::CONTENT_TYPE, ct.parse().unwrap());
            h
        };
        assert_eq!(
            parse_body(&with("text/plain;charset=UTF-8"), Bytes::from("hi")).unwrap(),
            Clip::Text("hi".into())
        );
        assert!(matches!(
            parse_body(&with("text/plain"), Bytes::from_static(&[0xff, 0xfe])),
            Err(ClipboardError::BadText)
        ));
        assert!(matches!(
            parse_body(&with("image/bmp"), Bytes::from("not a bmp")),
            Err(ClipboardError::BadImage(ImageError::NotBmp))
        ));
        assert!(matches!(
            parse_body(&with("image/png"), Bytes::from("x")),
            Err(ClipboardError::UnsupportedType)
        ));
        assert!(matches!(
            parse_body(&HeaderMap::new(), Bytes::from("x")),
            Err(ClipboardError::UnsupportedType)
        ));
    }

    #[tokio::test]
    async fn requests_reach_the_backend_in_order() {
        let mem = Memory::default();
        let clipboard = Clipboard::with_backend(mem.clone());
        assert_eq!(clipboard.read().await.unwrap(), None);
        clipboard.write(Clip::Text("one".into())).await.unwrap();
        clipboard.write(Clip::Text("two".into())).await.unwrap();
        assert_eq!(
            clipboard.read().await.unwrap(),
            Some(Clip::Text("two".into()))
        );
        assert_eq!(mem.get(), Some(Clip::Text("two".into())));
    }

    #[tokio::test]
    async fn a_failing_backend_reports_instead_of_hanging() {
        struct Broken;
        impl Backend for Broken {
            fn read(&mut self) -> Result<Option<Clip>, ClipboardError> {
                Err(ClipboardError::Busy)
            }
            fn write(&mut self, _: &Clip) -> Result<(), ClipboardError> {
                panic!("backend bug");
            }
        }
        let clipboard = Clipboard::with_backend(Broken);
        assert!(matches!(clipboard.read().await, Err(ClipboardError::Busy)));
        // A panic fails that request only; the thread keeps serving.
        assert!(matches!(
            clipboard.write(Clip::Text("x".into())).await,
            Err(ClipboardError::Failed(_))
        ));
        assert!(matches!(clipboard.read().await, Err(ClipboardError::Busy)));
    }

    const JPEG: &[u8] = b"\xff\xd8\xff\xe0\x00\x10JFIF\x00 and the rest of the photo";

    #[test]
    fn a_copied_image_file_is_sent_as_it_is() {
        let dir = tempfile::tempdir().unwrap();
        // Named .txt on purpose: the content decides, not the name.
        let path = dir.path().join("photo.txt");
        fs::write(&path, JPEG).unwrap();
        let img = image_from_copied_files(&[path]).unwrap().unwrap();
        assert_eq!(img.format, Format::Jpeg);
        assert_eq!(img.bytes, JPEG);
    }

    #[test]
    fn nothing_copied_means_nothing_to_send() {
        assert_eq!(image_from_copied_files(&[]).unwrap(), None);
    }

    #[test]
    fn several_copied_files_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.jpg");
        let b = dir.path().join("b.jpg");
        fs::write(&a, JPEG).unwrap();
        fs::write(&b, JPEG).unwrap();
        let err = image_from_copied_files(&[a, b]).unwrap_err();
        assert!(matches!(err, ClipboardError::SeveralFiles(2)));
        assert_eq!(err.status(), StatusCode::CONFLICT);
    }

    #[test]
    fn copied_files_that_arent_images_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let notes = dir.path().join("notes.png");
        fs::write(&notes, "a text file with an image's name").unwrap();
        let empty = dir.path().join("empty.png");
        fs::write(&empty, "").unwrap();
        for path in [notes, empty, dir.path().to_path_buf()] {
            assert!(
                matches!(
                    image_from_copied_files(std::slice::from_ref(&path)),
                    Err(ClipboardError::NotAnImage)
                ),
                "{}",
                path.display()
            );
        }
    }

    #[test]
    fn a_copied_file_that_is_gone_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let err = image_from_copied_files(&[dir.path().join("deleted.jpg")]).unwrap_err();
        assert!(matches!(
            err,
            ClipboardError::CopiedFileUnreadable("it isn't there any more")
        ));
        // The phone's message never includes the path.
        assert!(!err.to_string().contains("deleted"));
    }

    #[test]
    fn a_copied_image_over_the_limit_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.jpg");
        fs::write(&path, JPEG).unwrap();
        assert!(read_image_file(&path, JPEG.len()).is_ok());
        let err = read_image_file(&path, JPEG.len() - 1).unwrap_err();
        assert!(matches!(
            err,
            ClipboardError::BadImage(ImageError::TooLarge)
        ));
        assert_eq!(err.status(), StatusCode::PAYLOAD_TOO_LARGE);
        // A file smaller than the sniffing window works too.
        assert!(read_image_file(&path, 10_000).is_ok());
    }

    /// Replaces this PC's clipboard text, so it only runs on request.
    #[tokio::test]
    #[ignore = "replaces the contents of this PC's clipboard"]
    async fn system_clipboard_text_round_trip() {
        let clipboard = Clipboard::system();
        let text = "Beam clipboard test ✓".to_owned();
        clipboard.write(Clip::Text(text.clone())).await.unwrap();
        assert_eq!(clipboard.read().await.unwrap(), Some(Clip::Text(text)));
    }
}
