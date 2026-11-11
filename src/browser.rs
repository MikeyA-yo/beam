//! Opens a URL in the PC's default browser.

use std::io;
use std::process::{Command, Stdio};

/// Starts the default browser on `url` and returns without waiting for it.
pub fn open(url: &str) -> io::Result<()> {
    let mut cmd = command(url);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(drop)
}

/// `cmd /c start` would treat the `&` in our URL as a command separator,
/// so ask the URL handler directly instead.
#[cfg(windows)]
fn command(url: &str) -> Command {
    let mut cmd = Command::new("rundll32.exe");
    cmd.args(["url.dll,FileProtocolHandler", url]);
    cmd
}

#[cfg(target_os = "macos")]
fn command(url: &str) -> Command {
    let mut cmd = Command::new("open");
    cmd.arg(url);
    cmd
}

#[cfg(not(any(windows, target_os = "macos")))]
fn command(url: &str) -> Command {
    let mut cmd = Command::new("xdg-open");
    cmd.arg(url);
    cmd
}
