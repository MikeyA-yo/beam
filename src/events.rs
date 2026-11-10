//! Live updates for the page over a WebSocket at `/ws`.
//!
//! The watcher and the upload handler publish [`Event`]s on a broadcast
//! channel, and each connected page gets its own receiver. A page that falls
//! too far behind gets `{"type":"resync"}` and refetches `/api/files`
//! instead of being dropped.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, State};
use axum::response::Response;
use serde::Serialize;
use tokio::sync::broadcast::{self, error::RecvError};
use tokio::sync::watch;

use crate::server::AppState;

/// Events buffered per receiver before it counts as lagging. Generous,
/// because events are tiny and a burst of uploads can produce dozens.
pub const CHANNEL_CAPACITY: usize = 256;

/// Keeps idle connections alive and detects phones that left without
/// closing (Wi-Fi dropped, screen locked).
const PING_INTERVAL: Duration = Duration::from_secs(20);

/// A client that answers nothing for this long is considered gone.
const CLIENT_TIMEOUT: Duration = Duration::from_secs(50);

pub const RESYNC: &str = r#"{"type":"resync"}"#;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// A file appeared, or changed size or modified time. Clients upsert by name.
    FileAdded {
        name: String,
        size: u64,
        modified: u64,
    },
    FileRemoved {
        name: String,
    },
    /// Sent by the upload handler as soon as a file is in place.
    UploadCompleted {
        name: String,
        size: u64,
        modified: u64,
    },
}

pub fn channel() -> broadcast::Sender<Event> {
    broadcast::channel(CHANNEL_CAPACITY).0
}

/// `GET /ws`. Auth is the same cookie middleware as every other route.
pub async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
) -> Response {
    // Subscribe before upgrading so nothing published in between is missed.
    let rx = state.events.subscribe();
    let shutdown = state.shutdown_signal();
    ws.on_upgrade(move |socket| run(socket, rx, shutdown, peer))
}

async fn run(
    mut socket: WebSocket,
    mut rx: broadcast::Receiver<Event>,
    mut shutdown: watch::Receiver<bool>,
    peer: SocketAddr,
) {
    tracing::debug!("live updates connected: {}", peer.ip());
    let mut ping = tokio::time::interval(PING_INTERVAL);
    ping.tick().await; // the first tick fires immediately
    let mut last_heard = Instant::now();

    loop {
        tokio::select! {
            msg = next_message(&mut rx) => {
                let Some(text) = msg else { break };
                if socket.send(Message::Text(text.into())).await.is_err() {
                    break;
                }
            }
            incoming = socket.recv() => match incoming {
                None | Some(Err(_) | Ok(Message::Close(_))) => break,
                // Pongs (and anything else the page sends) prove it is alive.
                Some(Ok(_)) => last_heard = Instant::now(),
            },
            _ = ping.tick() => {
                if last_heard.elapsed() > CLIENT_TIMEOUT
                    || socket.send(Message::Ping(Default::default())).await.is_err()
                {
                    break;
                }
            }
            // Close cleanly on Ctrl+C so the shutdown isn't held up.
            _ = shutdown.changed() => {
                let _ = socket.send(Message::Close(None)).await;
                break;
            }
        }
    }
    tracing::debug!("live updates disconnected: {}", peer.ip());
}

/// The next message to send. `None` once the channel is closed.
pub async fn next_message(rx: &mut broadcast::Receiver<Event>) -> Option<String> {
    match rx.recv().await {
        Ok(event) => Some(serde_json::to_string(&event).unwrap_or_else(|_| RESYNC.to_owned())),
        Err(RecvError::Lagged(missed)) => {
            tracing::debug!("a page fell {missed} events behind; asking it to resync");
            Some(RESYNC.to_owned())
        }
        Err(RecvError::Closed) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn added(name: &str) -> Event {
        Event::FileAdded {
            name: name.into(),
            size: 1,
            modified: 2,
        }
    }

    #[test]
    fn events_serialize_like_the_prd() {
        let json = serde_json::to_string(&Event::FileAdded {
            name: "video.mp4".into(),
            size: 2_147_483_648,
            modified: 1_762_780_000,
        })
        .unwrap();
        assert_eq!(
            json,
            r#"{"type":"file_added","name":"video.mp4","size":2147483648,"modified":1762780000}"#
        );
        let json = serde_json::to_string(&Event::FileRemoved { name: "a".into() }).unwrap();
        assert_eq!(json, r#"{"type":"file_removed","name":"a"}"#);
    }

    #[tokio::test]
    async fn lagging_receiver_gets_resync_then_continues() {
        let (tx, mut rx) = broadcast::channel(2);
        for i in 0..5 {
            tx.send(added(&i.to_string())).unwrap();
        }
        assert_eq!(next_message(&mut rx).await.as_deref(), Some(RESYNC));
        // After the resync it carries on with what is still buffered.
        let next = next_message(&mut rx).await.unwrap();
        assert!(next.contains(r#""name":"3""#), "{next}");
    }

    #[tokio::test]
    async fn closed_channel_ends_the_stream() {
        let (tx, mut rx) = broadcast::channel::<Event>(2);
        drop(tx);
        assert_eq!(next_message(&mut rx).await, None);
    }
}
