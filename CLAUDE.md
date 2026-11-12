# AGENTS.md

Instructions for any AI coding agent working in this repo. Read `PRD.md` first; it is the source of truth for scope and behavior. If this file and the PRD disagree, the PRD wins and you should flag the conflict.

(If your tool expects `CLAUDE.md` or another filename, copy or symlink this file.)

## Project in one paragraph

Beam (working name) is a single Rust binary that runs on a PC and lets a phone send and receive files through its browser over the local network. The PC prints a QR code containing a URL with a session token; the phone scans it, opens a small embedded web page, and uploads or downloads files. v1 is files; v1.1 adds clipboard sharing on request (text on every platform, images on Windows). No HTTPS, no mobile app.

## Working style

- Work milestone by milestone (M1 to M6 in the PRD). Finish and verify one before starting the next.
- Before writing code for a milestone, state briefly what you plan to change and which files you will touch.
- Keep changes small and reviewable. Do not refactor unrelated code.
- Do not add features outside v1 scope, even if they seem easy. If something seems missing from the PRD, ask instead of inventing it.
- Ask before adding any dependency that is not in the list below.
- When you finish a milestone, summarize what changed, how you verified it, and anything left open.

## Stack

Rust stable, 2021 edition or newer. Expected crates (check crates.io for current versions and pin them in `Cargo.toml`):

- `tokio` (full) for the runtime
- `axum` with the `ws` and `multipart` features for HTTP and WebSocket (note: axum 0.8+ uses `{name}` path syntax, not `:name`)
- `tower-http` with the `fs` and `trace` features for `ServeDir` and request tracing
- `tokio-util` and `futures` for streaming
- `rust-embed` to embed the `web/` folder into the binary
- `local-ip-address` for LAN IP detection
- `qr2term` to print the QR code
- `notify` to watch the shared folder
- `clap` (derive) for CLI args
- `serde` and `serde_json` for API payloads
- `rand` or `getrandom` for the token, `subtle` for constant-time comparison
- `tracing` and `tracing-subscriber` for logs
- `anyhow` in `main`, `thiserror` for typed errors in modules
- `arboard` (default features off, so text only) for clipboard text
- `windows-sys`, Windows only, for `SO_EXCLUSIVEADDRUSE` and clipboard images. Its `unsafe` stays in `socket.rs` and `clipboard/win.rs`, with a `SAFETY` comment on every block.
- Dev only: `reqwest` and `tempfile` for integration tests

## Suggested layout

```
src/
  main.rs        CLI parsing, startup, printing URL and QR
  config.rs      resolved settings (dir, port, token)
  server.rs      router assembly and shared state
  auth.rs        token exchange, cookie check middleware
  upload.rs      streaming multipart upload
  files.rs       listing and downloads
  events.rs      WebSocket handler and broadcast channel
  watcher.rs     folder watching, emits events
  sanitize.rs    filename sanitization and path containment
  clipboard.rs   clipboard thread, backends, /api/clipboard handlers
  clipboard/     image.rs (image format helpers), dropfiles.rs (files copied in Explorer),
                 win.rs (Windows clipboard images)
web/
  index.html     single page, inline or local CSS and JS only
tests/
  integration.rs
```

## Hard rules

These come from the PRD and are not optional.

1. **Never buffer a whole file in memory.** Stream multipart fields to disk chunk by chunk, and stream downloads from disk.
2. **Raise the body limit.** axum's default body limit is 2 MB. Apply `DefaultBodyLimit::disable()` (or a very high limit) to the upload route only.
3. **Write uploads to `name.part`, then rename** on success. Delete the `.part` file on any error or client disconnect.
4. **Never overwrite.** On name collision, use `name (1).ext`, `name (2).ext`, and so on.
5. **Sanitize every filename** that comes from the client, in `sanitize.rs`, with unit tests. Strip directory components, reject `..`, remove `<>:"/\|?*` and control characters, handle Windows reserved names (`CON`, `PRN`, `AUX`, `NUL`, `COM1` to `COM9`, `LPT1` to `LPT9`, with or without an extension), trim trailing dots and spaces, cap the length, and fall back to a generated name if nothing is left.
6. **Path containment on downloads.** Canonicalize the requested path and confirm it is inside the canonicalized shared folder, including after resolving symlinks. Anything else returns 404.
7. **Auth on every route** except the initial `/?t=<token>` exchange. Use an `HttpOnly`, `SameSite=Strict` cookie. Compare tokens in constant time. Never log the token.
8. **No external resources in the web page.** No CDNs, Google Fonts, or remote scripts. The phone may have no internet. Plain HTML, CSS, and vanilla JS are preferred; no frontend build step.
9. **No outbound network calls and no telemetry** from the binary.
10. **No `unwrap()` or `expect()` on anything driven by client input or the filesystem.** Return proper errors and keep the server running.

## Implementation notes

- Downloads: `ServeDir` handles range requests, but it does not set `Content-Disposition: attachment`. Either wrap it to add that header or implement the download handler yourself with range support. Make sure the filename in the header is properly encoded (`filename*=UTF-8''...`).
- Upload progress is shown client-side with `XMLHttpRequest` upload progress events (`fetch` does not expose upload progress). Upload files one request at a time or with small concurrency so each has its own progress bar.
- The WebSocket uses a `tokio::sync::broadcast` channel. Lagging receivers should resync by refetching `/api/files` rather than crashing.
- `notify` can emit several events for one file write. Debounce, and ignore `.part` files so in-progress uploads never show as available downloads.
- Clipboard: send the PC's clipboard only when the phone asks, and never log its contents (type and size only). It is held in memory by nature, so rule 1 doesn't apply, but the route has its own size cap. Tests must use `clipboard::Memory`; anything that touches the real clipboard is `#[ignore]`.
- LAN IP detection: prefer private IPv4 ranges, skip loopback and link-local (169.254.x.x), and print every candidate if there are several.
- On startup, print a one-line hint: if the phone cannot connect, allow the app through Windows Firewall on private networks, set the Wi-Fi profile to Private, or connect the PC to the phone's hotspot.
- Use `PathBuf` and `std::path` everywhere. Do not build paths with string concatenation, because Windows uses backslashes.

## Commands

```
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo test
cargo run -- --dir ./tmp-share --port 8080
```

All three of `fmt`, `clippy`, and `test` must pass before you call a milestone done.

## Testing expectations

- Unit tests for `sanitize.rs` covering traversal attempts, reserved names, forbidden characters, empty results, very long names, and Unicode names.
- Integration tests that start the server on a random port with a temp directory and check: requests without the cookie are rejected, the token exchange works, an upload lands with the right content, collisions get renamed, a traversal filename stays inside the folder, a range request returns 206 with the right bytes, and a partial upload leaves no file behind.
- For the large-file requirement, add an ignored test (`#[ignore]`) that streams a multi-GB generated body, so it can be run manually.
- Manual check for each milestone on a real phone, using the acceptance criteria in the PRD.

## Style

- Idiomatic async Rust. Keep handlers thin and put logic in plain functions that are easy to test.
- Clear error messages for the user in the terminal; technical detail goes to `tracing` at debug level.
- Comments explain why, not what.
- Commit messages: short imperative summary, for example `upload: stream to .part and rename on completion`.

## Out of scope for v1 (do not build)

Automatic clipboard sync, files on the clipboard (other than a single copied image file), HTTPS, PWA or service workers, mDNS, persistent pairing, internet relay, folder uploads, zip downloads, a tray app, and any mobile app. These are listed under Future in the PRD.
