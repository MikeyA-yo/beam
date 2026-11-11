//! Beam: send files between a PC and a phone's browser over the local network.
//!
//! The binary in `main.rs` is a thin wrapper; everything lives here so the
//! integration tests can start a real server.

// The exceptions are Windows API calls: the socket option in `socket.rs`
// and clipboard images in `clipboard/win.rs`.
#![deny(unsafe_code)]

pub mod auth;
pub mod browser;
pub mod clipboard;
pub mod config;
pub mod events;
pub mod files;
mod human;
pub mod net;
mod qr;
pub mod sanitize;
pub mod server;
mod socket;
pub mod upload;
pub mod watcher;
