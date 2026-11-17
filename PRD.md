# PRD: Beam v1 (working name)

Browser-based local file sharing between a PC and a phone. The PC runs a single Rust binary; the phone needs nothing installed, just a browser.

| | |
|---|---|
| Owner | Ay |
| Status | Draft |
| Version | v1 (files), v1.1 (clipboard on request), v1.2 (share from anywhere on the PC), v1.3 (direct connection through the PC's hotspot) |
| Primary setup | HP laptop on Windows + Android phone (Chrome) |

## 1. Problem

Moving files between a PC and a phone is more painful than it should be. Xender-style apps need an app on both ends, a hotspot dance, and a fiddly connection flow every time. USB needs a cable and the right drivers. Cloud uploads (WhatsApp, Drive, Telegram) are slow, eat data, and compress media.

Both devices are usually already on the same Wi-Fi. The fastest path is a direct local transfer, and every phone already has a browser that can upload and download files.

## 2. Goal

Run one command on the PC, scan a QR code with the phone, and start sending files in both directions within 10 seconds. No app install on the phone, no account, no internet.

## 3. Non-goals for v1

- Automatic clipboard sync. The PC's clipboard goes to the phone only when the user taps for it (see 6.7)
- Files on the clipboard (copied in Explorer), except a single image file (FR-29); use the shared folder for files
- A native Android app, Tauri mobile build, or Termux component
- HTTPS, PWA install, or Android share-sheet integration
- Internet relay or transfers across different networks
- Automatic device discovery (mDNS); the QR code is the discovery mechanism
- Multiple PCs, multi-user accounts, or persistent pairing
- Folder upload or zipped multi-file download
- A desktop GUI or tray app on the PC side (the terminal plus a local web page showing the QR code is enough; see FR-4a)

## 4. Users and scenarios

The primary user is Ay: one PC, one phone, same Wi-Fi or the phone's hotspot.

1. **Phone to PC:** take photos or a screen recording on the phone, open the page, select files, and they land in a folder on the PC.
2. **PC to phone:** drop a file into the shared folder on the PC (or pass it at startup), open the page on the phone, tap to download.
3. **Big files:** move a 2 GB video without the PC or phone running out of memory, and without the transfer silently dying.
4. **Hostile network:** on campus or office Wi-Fi with client isolation, fall back to the phone's hotspot with the PC connected to it.

## 5. User flow

1. User runs `beam` (optionally `beam --dir <path> --port <port>`).
2. Terminal prints the URL and a QR code, for example `http://192.168.1.5:8080/?t=<token>`, and the PC's browser opens a page showing the same QR code at a size that is easier to scan.
3. User scans the QR with the phone camera. Chrome opens the page and the token is stored as a cookie.
4. The page shows two sections: **Send to PC** (file picker plus upload progress) and **On PC** (list of files available to download).
5. Uploads appear in the PC's shared folder. Files added to the shared folder on the PC appear on the phone's list without a manual refresh.
6. User presses Ctrl+C on the PC to stop. The token dies with the process.

## 6. Functional requirements

### 6.1 Startup

- **FR-1:** Detect the machine's LAN IPv4 address. If several interfaces qualify, prefer private ranges (192.168.x.x, 10.x.x.x, 172.16 to 31.x.x) and print all candidates.
- **FR-2:** Bind to `0.0.0.0` on a configurable port (default 8080). If the port is taken, try the next few and print the one used.
- **FR-3:** Generate a random session token (at least 128 bits, URL-safe) on every start.
- **FR-4:** Print the full URL and render it as a QR code in the terminal.
- **FR-4a:** Also open the PC's default browser at a local page (`/qr`) that shows the QR code large, one per LAN address with a switcher. The page is a web UI served by Beam, not a desktop app, and sits behind the same cookie auth (the browser link exchanges the token first). The terminal output stays as in FR-4. `--no-open` disables opening the browser.
- **FR-5:** Use a shared folder, by default `~/Downloads/Beam` (create it if missing), overridable with `--dir`, and switchable while Beam runs (FR-33).

### 6.2 Auth

- **FR-6:** A request to `/?t=<token>` with the correct token sets an `HttpOnly`, `SameSite=Strict` session cookie and redirects to `/` to strip the token from the URL. The cookie has a 24-hour `Max-Age`, so the phone stays logged in when Android closes Chrome in the background. It still stops working when Beam exits, because the token changes on every run.
- **FR-7:** Every other route rejects requests without a valid cookie with 401. The page shows a "scan the QR code again" message.
- **FR-8:** Compare tokens in constant time.

### 6.3 Phone to PC (upload)

- **FR-9:** The page accepts multiple files in one selection.
- **FR-10:** Uploads stream to disk in chunks. The server never buffers a whole file in memory.
- **FR-11:** No artificial size limit beyond free disk space. The framework's default body limit must be disabled or raised for the upload route.
- **FR-12:** Write to a temporary file (for example `name.part`) and rename on completion, so partial uploads never look like finished files.
- **FR-13:** If a file with the same name exists, save as `name (1).ext`, `name (2).ext`, and so on. Never overwrite.
- **FR-14:** Sanitize filenames: strip path components and separators, reject `..`, remove characters Windows forbids (`<>:"/\|?*` and control characters), avoid reserved names (`CON`, `NUL`, `COM1` and similar), and cap length.
- **FR-15:** The page shows per-file progress (percent and speed) and a clear success or failure state.
- **FR-16:** If an upload is cancelled or the connection drops, delete the `.part` file.

### 6.4 PC to phone (download)

- **FR-17:** List files in the shared folder (top level only for v1), plus files shared from elsewhere (FR-32), with name, size, and modified time, newest first.
- **FR-18:** Downloads support HTTP range requests so the browser can resume or seek.
- **FR-19:** Serve downloads with `Content-Disposition: attachment` and the original filename.
- **FR-20:** Requests for paths outside the shared folder (traversal, absolute paths, symlinks pointing out) return 404.
- **FR-21:** Optionally accept files at startup, `beam file1.mp4 file2.pdf`. They are served from where they are, never copied (FR-32).

### 6.5 Live updates

- **FR-22:** A WebSocket at `/ws` (same cookie auth) pushes events: file added, file removed, upload completed.
- **FR-23:** The server watches the shared folder for changes so files dropped in from Explorer show up on the phone within about a second.
- **FR-24:** If the WebSocket drops, the page reconnects with backoff and refetches the file list.

### 6.6 Terminal output

- **FR-25:** Log each connected device (IP and user agent), each completed transfer (direction, name, size, duration, average speed), and errors. Keep it readable, not noisy.

### 6.7 Clipboard (v1.1)

- **FR-26:** The page has a Clipboard section with a text box. **Send to PC** puts its text on the PC's clipboard.
- **FR-27:** **Get from PC** reads the PC's clipboard once, when tapped. Beam never sends the clipboard on its own, so something copied on the PC (a password, say) doesn't reach the phone unless the user asks. Text fills the box and is copied to the phone's clipboard in the same tap. A **Copy** button copies the box again. Over plain HTTP the browser's modern clipboard API is unavailable, so the page uses the older copy command, which works on a tap.
- **FR-28:** Images go both ways. From the phone, the user picks an image (or pastes one into the box where the keyboard allows). The phone's browser decodes it and sends a 24-bit BMP, scaled down to at most 32 megapixels, and the PC puts it on the clipboard as a bitmap that any Windows app can paste. From the PC, the phone gets the clipboard's PNG if there is one, otherwise the bitmap, and shows it with a **Save** button. Long-pressing the image copies it. Images on the clipboard work on Windows only for now; elsewhere the page shows a clear message. Text works on Windows, macOS and Linux.
- **FR-29:** When there is both, text wins (copying Excel cells puts both on the clipboard). An image file copied in Explorer (Ctrl+C or right-click, Copy) puts only its path on the clipboard, not the picture, so on Windows Beam reads that file and sends it as it is, if it's a single PNG, JPEG, GIF, WebP or BMP of at most 128 MB, recognised by its content rather than its name. Several copied files, or a file that isn't such an image, get a clear message instead; those belong in the shared folder. If there's nothing usable at all, the page says there's nothing to get.
- **FR-30:** Same cookie auth as everything else. The server checks images strictly before they reach the clipboard, and caps a clipboard request at 128 MB. Clipboard data is held in memory (it lives in memory on both ends anyway), unlike file uploads.
- **FR-31:** The terminal logs each clipboard transfer with direction, IP, and type and size ("text, 42 characters" or "image, 1080×2400"), never the contents.

### 6.8 Sharing from anywhere on the PC (v1.2)

The PC's own drive may be short on space, for example when sending a film from an external SSD. Nothing should need to be copied onto it.

- **FR-32:** Files given at startup are served in place: no copy, no hard link, nothing written to the PC. They stay shared until Beam stops or the PC stops sharing them. If a file in the shared folder has the same name, the outside file is listed as `name (1).ext`; folder files keep their names. Each download checks the file is still at the registered path and is still a file.
- **FR-33:** The PC's QR page has a **Shared folder** box. Pasting a folder path (quotes from Explorer's "Copy as path" are stripped) switches the shared folder without restarting: the watcher moves to it, phones refetch the list at once, and uploads go there. The token and cookies keep working. Transfers already running finish in the old folder. A folder that doesn't exist is refused, never created, and relative paths are refused.
- **FR-34:** The page lists earlier folders of this run to switch back with one click, and the files shared from elsewhere with a **Stop sharing** button.
- **FR-35:** Only the PC itself can see or change what's shared: those routes answer requests from the loopback address only, so a phone with a valid cookie gets 403. They take JSON only, so another website can't send them without a CORS preflight. The phone page shows the shared folder's name, never its full path.

### 6.9 Direct connection through the PC's hotspot (v1.3)

Through a router, every byte crosses the air twice on one shared channel, often 2.4 GHz. Phone straight to PC crosses once, and can use 5 GHz. Flash-transfer apps and AirDrop work this way; a browser can't create a direct Wi-Fi link, but the PC can be the hotspot.

- **FR-36:** On Windows, the PC's QR page offers **Same Wi-Fi** (FR-4a) or **Direct to this PC**. Direct turns on Windows Mobile Hotspot and shows two numbered steps: a Wi-Fi QR code (`WIFI:T:WPA;S:...;P:...;;`, which phone cameras offer to join) with the network name and password, then the Beam QR code for `192.168.137.1`, the address Windows always gives the PC on its hotspot.
- **FR-37:** `beam --hotspot` does this at startup and prints both QR codes in the terminal, for people who start Beam from a shortcut. If the hotspot can't start, Beam says why and carries on with the normal connection.
- **FR-38:** Beam keeps the hotspot's existing name and password. It asks for 5 GHz when the hardware supports it, and keeps Windows from turning the hotspot off while no device has joined yet. Both are put back when the hotspot is turned off. A PC whose own Wi-Fi is on 2.4 GHz may still get 2.4 GHz; the page says so instead of promising 5 GHz.
- **FR-39:** When Beam stops (Ctrl+C or closing its window), it turns the hotspot off if Beam turned it on, and leaves it alone otherwise.
- **FR-40:** Same PC-only rule as FR-35: the hotspot routes answer the loopback address only and take JSON only. The password is shown on the PC's screen and in the QR code, never logged.
- **FR-41:** Windows treats the hotspot as a public network, so the page tells people to allow Beam through the firewall on public networks too.

## 7. Non-functional requirements

- **Single binary:** HTML, CSS, and JS are embedded in the binary. The page loads no external CDN, font, or script, because the phone may have no internet.
- **Performance:** a 1 GB transfer on decent 5 GHz Wi-Fi should be limited by the network, not the server. Server memory stays roughly flat regardless of file size.
- **Platforms:** Windows 10/11 is the primary target. The code should also build and run on Linux and macOS without changes.
- **Mobile UI:** works well on a narrow phone screen with large tap targets. Works in Chrome on Android; Safari on iOS should not break.
- **Resilience:** one failed upload never crashes the server or affects other transfers.
- **No telemetry, no outbound network calls.**

## 8. API sketch

| Method | Path | Purpose |
|---|---|---|
| GET | `/?t=<token>` | Exchange token for cookie, redirect to `/` |
| GET | `/` | Serve the embedded page |
| GET | `/qr` | QR code page for the PC's browser (FR-4a) |
| GET | `/api/files` | JSON list of shared files |
| POST | `/api/upload` | Multipart upload, streamed to disk |
| GET | `/api/files/{name}` | Download one file, with range support |
| GET | `/ws` | WebSocket for live events |
| GET | `/api/share` | The shared folder, earlier folders and files shared from elsewhere; PC only (FR-35) |
| PUT | `/api/share/folder` | Switch the shared folder, JSON `{"path": ...}`; PC only (FR-33) |
| DELETE | `/api/share/files/{id}` | Stop sharing a file from elsewhere; PC only (FR-34) |
| GET | `/api/hotspot` | Whether the PC's hotspot is on, with its network, password and both QR codes; PC only (FR-36) |
| POST | `/api/hotspot/start` | Turn the PC's hotspot on (JSON body `{}`); PC only (FR-36, FR-38) |
| POST | `/api/hotspot/stop` | Turn it off and restore its settings (JSON body `{}`); PC only (FR-38) |
| GET | `/api/clipboard` | The PC's clipboard: text or an image (PNG, JPEG, GIF, WebP or BMP) by `Content-Type`; 204 if empty, 409 with a message if a copied file can't be sent (FR-27, FR-29) |
| PUT | `/api/clipboard` | Set the PC's clipboard from `text/plain` or `image/bmp` (FR-26, FR-28) |

Example event over the WebSocket:

```json
{ "type": "file_added", "name": "video.mp4", "size": 2147483648, "modified": 1762780000 }
```

## 9. Error handling

| Case | Behavior |
|---|---|
| Wrong or missing token | 401, page says to rescan the QR |
| Disk full during upload | Abort, delete `.part`, show error on the page, log on the PC |
| Client disconnects mid-upload | Delete `.part`, log it |
| Phone cannot reach the PC | Out of the server's control; the startup output includes a short hint about Windows Firewall, Private network profile, and the hotspot fallback |
| Port in use | Try the next ports, then exit with a clear message |

## 10. Security

The threat model is other people on the same Wi-Fi. The token in the QR code is the only thing that grants access, it rotates on every run, and it never appears in server logs. Transport is plain HTTP in v1, so traffic on the local network is not encrypted; this is an accepted v1 tradeoff and HTTPS is planned for v2. Filename sanitization and path containment are mandatory because the server writes attacker-influenced names to disk. The clipboard can hold secrets, so it is only read when the phone asks and its contents are never logged. Changing what is shared would let anyone holding the QR code reach any folder on the PC, so it is restricted to the PC itself (FR-35), not merely to holders of the cookie.

## 11. Acceptance criteria

v1 is done when all of these pass on the real setup (Windows laptop + Android phone):

1. Fresh run to first file sent from the phone takes under 10 seconds.
2. A 2 GB file uploads from the phone and downloads to the phone successfully, with server memory staying flat.
3. Ten files selected at once all upload, with individual progress.
4. A file dropped into the shared folder in Explorer appears on the phone within about a second, without refreshing.
5. Opening the page without the token, or with an old token from a previous run, is rejected.
6. Uploading a file named `../../evil.txt` or `CON.txt` results in a safely renamed file inside the shared folder.
7. Killing the browser mid-upload leaves no partial file behind.
8. The binary runs on a machine with no internet connection.

## 12. Milestones

1. **M1:** server skeleton, LAN IP detection, token and QR output, embedded page, auth.
2. **M2:** streaming upload with sanitization, collisions, and `.part` handling.
3. **M3:** file listing and range-capable downloads.
4. **M4:** WebSocket events and folder watching.
5. **M5:** UI polish (progress, speed, errors), logging, and the acceptance pass above.
6. **M6 (v1.1):** clipboard text both ways, and images both ways on Windows (section 6.7).
7. **M7 (v1.2):** files served in place, and switching the shared folder from the PC while running (section 6.8).
8. **M8 (v1.3):** direct connection through the PC's hotspot, on the page and with `--hotspot` (section 6.9).

## 13. Future (v2 and beyond)

- Clipboard extras: optional live sync, clipboard images on macOS and Linux
- HTTPS (local cert or the DNS-to-LAN-IP trick), which unlocks the real clipboard API (reading the phone's clipboard with one tap, copying images directly), PWA install, and an Android share target
- mDNS discovery and persistent pairing so the QR is only needed once
- A PC-side web page with drag and drop
- Folder uploads and zipped multi-file downloads
- System tray app on Windows and a right-click "Send to phone" entry
- A native phone companion (Termux + Go, or a thin Android app around the same protocol), using Wi-Fi Direct for a direct 5 GHz link
- Quick Share compatibility, so Android's own share sheet can send to Beam (unofficial protocol; see `rquickshare`, `NearDrop`)

## 14. Open questions

- Final name for the project
- Should the shared folder double as the upload destination, or should uploads and outgoing files live in separate folders?
- Should a session expire after a period of inactivity, or only on process exit? (Today: on process exit, or 24 hours after scanning, whichever comes first.)
