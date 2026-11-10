use std::future::{Future, IntoFuture};
use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::{ConnectInfo, DefaultBodyLimit, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use rust_embed::Embed;
use serde::Deserialize;
use thiserror::Error;
use tokio::net::TcpListener;

use crate::{auth, socket, upload};

/// How many ports to try, starting from the requested one.
pub const PORT_ATTEMPTS: u16 = 10;

/// After Ctrl+C, how long transfers in progress get to finish. Anything
/// still running is then dropped, and its `.part` file is deleted.
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

#[derive(Embed)]
#[folder = "web/"]
struct Assets;

#[derive(Clone)]
pub struct AppState {
    pub token: Arc<str>,
    /// Canonical shared folder.
    pub dir: Arc<PathBuf>,
}

impl AppState {
    pub fn new(token: String, dir: PathBuf) -> Self {
        Self {
            token: token.into(),
            dir: Arc::new(dir),
        }
    }
}

#[derive(Debug, Error)]
pub enum BindError {
    #[error("ports {first} to {last} are all in use; pick another with --port")]
    AllInUse { first: u16, last: u16 },
    #[error("could not listen on port {port}: {source}")]
    Io { port: u16, source: io::Error },
}

/// Binds `0.0.0.0:port`, moving to the next port if it's taken.
pub async fn bind(port: u16) -> Result<TcpListener, BindError> {
    bind_on(Ipv4Addr::UNSPECIFIED, port).await
}

/// Like [`bind`] on a chosen address. Tests use loopback so Windows doesn't
/// show a firewall prompt.
///
/// Async only to make sure this runs inside the tokio runtime, which
/// registering the listener requires.
pub async fn bind_on(ip: Ipv4Addr, port: u16) -> Result<TcpListener, BindError> {
    let last = port.saturating_add(PORT_ATTEMPTS - 1);
    for p in port..=last {
        match socket::listen_exclusive(SocketAddrV4::new(ip, p)) {
            Ok(listener) => return Ok(listener),
            // Windows reports ports reserved by Hyper-V/WSL as PermissionDenied.
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::AddrInUse | io::ErrorKind::PermissionDenied
                ) =>
            {
                tracing::debug!("port {p} unavailable: {e}");
            }
            Err(source) => return Err(BindError::Io { port: p, source }),
        }
    }
    Err(BindError::AllInUse { first: port, last })
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        // axum's default 2 MB body limit would cut off any real upload.
        // The upload handler streams to disk, so lifting it costs no memory.
        .route(
            "/api/upload",
            post(upload::handle).layer(DefaultBodyLimit::disable()),
        )
        .fallback(not_found)
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth::require_auth,
        ))
        .with_state(state)
}

/// Serves until `shutdown` resolves, then gives transfers in progress
/// [`SHUTDOWN_GRACE`] to finish.
pub async fn serve(
    listener: TcpListener,
    state: AppState,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> io::Result<()> {
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let server = axum::serve(
        listener,
        router(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async {
        let _ = stop_rx.await;
    })
    .into_future();
    let mut server = std::pin::pin!(server);

    tokio::select! {
        result = &mut server => return result,
        () = shutdown => {}
    }
    let _ = stop_tx.send(());
    match tokio::time::timeout(SHUTDOWN_GRACE, server).await {
        Ok(result) => result,
        Err(_) => {
            tracing::info!("Stopping without waiting for unfinished transfers.");
            Ok(())
        }
    }
}

#[derive(Deserialize)]
struct IndexQuery {
    t: Option<String>,
}

/// `/?t=<token>` exchanges the token for a cookie; plain `/` serves the app.
async fn index(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Query(query): Query<IndexQuery>,
    headers: HeaderMap,
) -> Response {
    match query.t {
        Some(t) if auth::tokens_match(&state.token, &t) => {
            let agent = headers
                .get(header::USER_AGENT)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("unknown browser");
            tracing::info!("Device connected: {} ({agent})", peer.ip());
            connected_response(&state.token)
        }
        Some(_) => {
            tracing::warn!(
                "Rejected {}: wrong or expired token (old QR code?)",
                peer.ip()
            );
            unauthorized_page()
        }
        None if auth::has_valid_cookie(&headers, &state.token) => {
            html_page(StatusCode::OK, "index.html")
        }
        None => unauthorized_page(),
    }
}

/// Sets the cookie and sends the browser on to `/`.
///
/// This is a 200 page with a meta refresh instead of a 30x redirect. The
/// cookie is `SameSite=Strict` and the QR scan arrives as a cross-site
/// navigation. A server redirect keeps that cross-site context, so the
/// browser could drop the new cookie on the follow-up request. A refresh
/// started by our own page counts as same-site.
fn connected_response(token: &str) -> Response {
    let mut res = html_page(StatusCode::OK, "connected.html");
    match HeaderValue::from_str(&auth::session_cookie(token)) {
        Ok(cookie) => {
            res.headers_mut().insert(header::SET_COOKIE, cookie);
        }
        Err(e) => {
            // Can't happen with a hex token, but don't panic on a request path.
            tracing::error!("could not build session cookie: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "Internal error").into_response();
        }
    }
    res
}

fn unauthorized_page() -> Response {
    html_page(StatusCode::UNAUTHORIZED, "unauthorized.html")
}

fn html_page(status: StatusCode, name: &str) -> Response {
    let Some(file) = Assets::get(name) else {
        tracing::error!("embedded page {name} is missing from the build");
        return (StatusCode::INTERNAL_SERVER_ERROR, "Internal error").into_response();
    };
    (
        status,
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            // Always ask the server, so a stale copy never bypasses the auth check.
            (header::CACHE_CONTROL, "no-store"),
            // Keep the token URL out of any Referer header.
            (header::REFERRER_POLICY, "no-referrer"),
        ],
        file.data,
    )
        .into_response()
}

async fn not_found() -> impl IntoResponse {
    (StatusCode::NOT_FOUND, "Not found")
}
