use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use beam::server::{self, AppState};
use reqwest::StatusCode;
use reqwest::header::{CONTENT_TYPE, COOKIE, SET_COOKIE};
use tempfile::TempDir;

const BOUNDARY: &str = "beam-test-boundary-7MA4YWxkTrZu0gW";

struct TestServer {
    base: String,
    addr: SocketAddr,
    token: String,
    client: reqwest::Client,
    dir: TempDir,
    /// Stands in for the PC's clipboard, which tests must never touch.
    clipboard: beam::clipboard::Memory,
}

impl TestServer {
    async fn start() -> Self {
        Self::start_with_phone_urls(|_| Vec::new()).await
    }

    /// `urls` gets the token and returns the phone URLs to advertise.
    async fn start_with_phone_urls(urls: impl FnOnce(&str) -> Vec<String>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let token = beam::config::generate_token();
        let clipboard = beam::clipboard::Memory::default();
        let state = AppState::new(token.clone(), dir.path().canonicalize().unwrap())
            .with_phone_urls(urls(&token))
            .with_clipboard(beam::clipboard::Clipboard::with_backend(clipboard.clone()));
        // Ends with the test's runtime.
        beam::watcher::start(state.clone()).await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                listener,
                server::router(state).into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        Self {
            base: format!("http://{addr}"),
            addr,
            token,
            client,
            dir,
            clipboard,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    fn cookie(&self) -> String {
        format!("beam_session={}", self.token)
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    /// Sorted names of everything in the shared folder.
    fn files(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(self.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    fn post_upload(&self, body: impl Into<reqwest::Body>) -> reqwest::RequestBuilder {
        self.client
            .post(self.url("/api/upload"))
            .header(COOKIE, self.cookie())
            .header(
                CONTENT_TYPE,
                format!("multipart/form-data; boundary={BOUNDARY}"),
            )
            .body(body)
    }

    async fn upload(&self, files: &[(&str, &[u8])]) -> reqwest::Response {
        self.post_upload(multipart_body(files))
            .send()
            .await
            .unwrap()
    }

    /// Uploads one file and returns the name the server saved it under.
    async fn upload_one(&self, name: &str, data: &[u8]) -> String {
        let res = self.upload(&[(name, data)]).await;
        assert_eq!(res.status(), StatusCode::OK, "upload of {name:?} failed");
        let json: serde_json::Value = serde_json::from_str(&res.text().await.unwrap()).unwrap();
        assert_eq!(json["files"][0]["size"].as_u64(), Some(data.len() as u64));
        json["files"][0]["name"].as_str().unwrap().to_owned()
    }
}

/// Multipart is built by hand rather than with reqwest's builder, so tests
/// control exactly which filename bytes reach the server, the way a hostile
/// client would.
fn part_header(name: &str) -> String {
    format!(
        "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{name}\"\r\n\
         Content-Type: application/octet-stream\r\n\r\n"
    )
}

fn multipart_body(files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut body = Vec::new();
    for (name, data) in files {
        body.extend_from_slice(part_header(name).as_bytes());
        body.extend_from_slice(data);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
    body
}

/// Polls until `check` passes or `timeout` runs out.
async fn eventually(timeout: Duration, mut check: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if check() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    check()
}

// ---- Auth (M1) ----

#[tokio::test]
async fn page_without_cookie_asks_to_rescan() {
    let s = TestServer::start().await;
    let res = s.client.get(s.url("/")).send().await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    assert!(res.headers().get(SET_COOKIE).is_none());
    let body = res.text().await.unwrap();
    assert!(body.contains("Scan the QR code again"));
}

#[tokio::test]
async fn wrong_token_is_rejected_without_cookie() {
    let s = TestServer::start().await;
    let res = s.client.get(s.url("/?t=deadbeef")).send().await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    assert!(res.headers().get(SET_COOKIE).is_none());
}

#[tokio::test]
async fn token_exchange_sets_strict_httponly_cookie_and_moves_to_root() {
    let s = TestServer::start().await;
    let res = s
        .client
        .get(s.url(&format!("/?t={}", s.token)))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let cookie = res
        .headers()
        .get(SET_COOKIE)
        .expect("cookie set")
        .to_str()
        .unwrap()
        .to_owned();
    assert!(cookie.starts_with(&format!("beam_session={};", s.token)));
    assert!(cookie.contains("HttpOnly"));
    assert!(cookie.contains("SameSite=Strict"));
    let body = res.text().await.unwrap();
    assert!(body.contains(r#"http-equiv="refresh" content="0;url=/""#));

    // The cookie from the exchange now opens the app.
    let cookie_pair = cookie.split(';').next().unwrap();
    let res = s
        .client
        .get(s.url("/"))
        .header(COOKIE, cookie_pair)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(
        res.headers()
            .get("cache-control")
            .unwrap()
            .to_str()
            .unwrap(),
        "no-store"
    );
    let body = res.text().await.unwrap();
    assert!(body.contains("Send to PC"));
    assert!(body.contains("On PC"));
}

#[tokio::test]
async fn cookie_from_a_previous_run_is_rejected() {
    let s = TestServer::start().await;
    let old = beam::config::generate_token();
    let res = s
        .client
        .get(s.url("/"))
        .header(COOKIE, format!("beam_session={old}"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn other_routes_require_cookie() {
    let s = TestServer::start().await;
    let res = s.client.get(s.url("/api/whatever")).send().await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

    // With a valid cookie it gets past auth and hits the normal 404.
    let res = s
        .client
        .get(s.url("/api/whatever"))
        .header(COOKIE, s.cookie())
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn token_in_query_does_not_unlock_other_routes() {
    let s = TestServer::start().await;
    let res = s
        .client
        .get(s.url(&format!("/api/whatever?t={}", s.token)))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn busy_port_falls_back_to_next() {
    use std::net::Ipv4Addr;
    let taken = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let busy = taken.local_addr().unwrap().port();
    // Port numbers near the top of the range would leave no room to fall back.
    if busy > u16::MAX - server::PORT_ATTEMPTS {
        return;
    }
    let listener = server::bind_on(Ipv4Addr::LOCALHOST, busy).await.unwrap();
    let got = listener.local_addr().unwrap().port();
    assert!(got > busy && got < busy + server::PORT_ATTEMPTS);
}

// ---- Uploads (M2) ----

#[tokio::test]
async fn upload_without_cookie_is_rejected() {
    let s = TestServer::start().await;
    let res = s
        .client
        .post(s.url("/api/upload"))
        .header(
            CONTENT_TYPE,
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .body(multipart_body(&[("a.txt", b"x")]))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    assert!(s.files().is_empty());
}

#[tokio::test]
async fn upload_lands_with_the_right_content() {
    let s = TestServer::start().await;
    let data = b"hello from the phone\n\x00\xff binary too";
    assert_eq!(s.upload_one("hello.txt", data).await, "hello.txt");
    assert_eq!(std::fs::read(s.path().join("hello.txt")).unwrap(), data);
    assert_eq!(s.files(), ["hello.txt"], "no .part file may remain");
}

#[tokio::test]
async fn several_files_in_one_request() {
    let s = TestServer::start().await;
    let res = s.upload(&[("one.txt", b"1"), ("two.txt", b"22")]).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(s.files(), ["one.txt", "two.txt"]);
    assert_eq!(std::fs::read(s.path().join("two.txt")).unwrap(), b"22");
}

#[tokio::test]
async fn collisions_get_numbered_and_nothing_is_overwritten() {
    let s = TestServer::start().await;
    assert_eq!(s.upload_one("photo.jpg", b"first").await, "photo.jpg");
    assert_eq!(s.upload_one("photo.jpg", b"second").await, "photo (1).jpg");
    assert_eq!(s.upload_one("photo.jpg", b"third").await, "photo (2).jpg");
    assert_eq!(std::fs::read(s.path().join("photo.jpg")).unwrap(), b"first");
    assert_eq!(
        std::fs::read(s.path().join("photo (1).jpg")).unwrap(),
        b"second"
    );
    assert_eq!(
        std::fs::read(s.path().join("photo (2).jpg")).unwrap(),
        b"third"
    );
}

#[tokio::test]
async fn concurrent_uploads_of_the_same_name_all_survive() {
    let s = TestServer::start().await;
    let bodies: Vec<Vec<u8>> = (0..8u8).map(|i| vec![i; 64 * 1024]).collect();
    let uploads = bodies.iter().map(|b| s.upload_one("same.bin", b));
    let mut names = futures::future::join_all(uploads).await;
    names.sort();
    names.dedup();
    assert_eq!(names.len(), 8, "every upload needs its own name: {names:?}");
    // Each body is one repeated byte, so every original must be on disk intact.
    let mut found: Vec<u8> = names
        .iter()
        .map(|n| {
            let data = std::fs::read(s.path().join(n)).unwrap();
            assert_eq!(data.len(), 64 * 1024);
            assert!(data.iter().all(|&b| b == data[0]));
            data[0]
        })
        .collect();
    found.sort();
    assert_eq!(found, (0..8u8).collect::<Vec<_>>());
}

#[tokio::test]
async fn traversal_names_stay_inside_the_folder() {
    let s = TestServer::start().await;
    let outside: PathBuf = s.path().parent().unwrap().join("evil.txt");
    let existed_before = outside.exists();

    assert_eq!(s.upload_one("../../evil.txt", b"x").await, "evil.txt");
    let saved = s.upload_one(r"..\..\evil.txt", b"y").await;
    assert!(
        saved.ends_with(".txt") && !saved.contains(['/', '\\']),
        "{saved:?}"
    );
    assert_eq!(s.upload_one("/etc/passwd", b"z").await, "passwd");

    assert_eq!(
        outside.exists(),
        existed_before,
        "nothing may be written outside"
    );
    assert_eq!(s.files().len(), 3);
    for name in s.files() {
        assert!(s.path().join(&name).is_file());
    }
}

#[tokio::test]
async fn reserved_and_forbidden_names_are_made_safe() {
    let s = TestServer::start().await;
    assert_eq!(s.upload_one("CON.txt", b"x").await, "_CON.txt");
    assert_eq!(s.upload_one("nul", b"x").await, "_nul");
    assert_eq!(s.upload_one("a<b>c|d?e*.txt", b"x").await, "abcde.txt");
    assert_eq!(
        s.upload_one("notes.txt:stream", b"x").await,
        "notes.txtstream"
    );
    assert_eq!(s.upload_one("trailing. . .", b"x").await, "trailing");
    let fallback = s.upload_one("..", b"x").await;
    assert!(fallback.starts_with("file-"), "{fallback:?}");
}

#[tokio::test]
async fn unicode_names_survive_the_round_trip() {
    let s = TestServer::start().await;
    for name in ["фото 📷.jpg", "日本語.txt", "café.mp3"] {
        assert_eq!(s.upload_one(name, b"x").await, name);
        assert!(s.path().join(name).is_file());
    }
}

#[tokio::test]
async fn uploads_above_the_default_2mb_body_limit_work() {
    let s = TestServer::start().await;
    let data: Vec<u8> = (0..5 * 1024 * 1024u32).map(|i| (i % 251) as u8).collect();
    assert_eq!(s.upload_one("big.bin", &data).await, "big.bin");
    assert_eq!(std::fs::read(s.path().join("big.bin")).unwrap(), data);
}

#[tokio::test]
async fn request_without_files_is_a_bad_request() {
    let s = TestServer::start().await;
    let body = format!(
        "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"note\"\r\n\r\nhi\r\n--{BOUNDARY}--\r\n"
    );
    let res = s.post_upload(body).send().await.unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    assert!(res.text().await.unwrap().contains("no files"));
    assert!(s.files().is_empty());
}

/// The client vanishes halfway through. The temp file must exist during the
/// upload, which proves the test got that far, and be gone afterwards.
#[tokio::test]
async fn interrupted_upload_leaves_no_file_behind() {
    use tokio::io::AsyncWriteExt;

    let s = TestServer::start().await;
    let mut sock = tokio::net::TcpStream::connect(s.addr).await.unwrap();
    let head = format!(
        "POST /api/upload HTTP/1.1\r\nHost: {}\r\nCookie: {}\r\n\
         Content-Type: multipart/form-data; boundary={BOUNDARY}\r\n\
         Content-Length: 100000000\r\n\r\n{}",
        s.addr,
        s.cookie(),
        part_header("partial.bin"),
    );
    sock.write_all(head.as_bytes()).await.unwrap();
    sock.write_all(&vec![7u8; 1024 * 1024]).await.unwrap();

    let started = eventually(Duration::from_secs(5), || {
        s.files().iter().any(|n| n.ends_with(".part"))
    })
    .await;
    assert!(started, "upload never started: {:?}", s.files());

    drop(sock);
    let cleaned = eventually(Duration::from_secs(5), || s.files().is_empty()).await;
    assert!(cleaned, "left behind: {:?}", s.files());
}

/// Streams a multi-GB body to check that large files work and memory stays
/// flat (watch the test process in Task Manager while it runs).
/// Run with: cargo test --release --test integration -- --ignored --nocapture huge
#[tokio::test]
#[ignore = "writes 3 GB to disk; run manually"]
async fn huge_upload_streams_to_disk() {
    use axum::body::Bytes;

    const CHUNK: usize = 1024 * 1024;
    const CHUNKS: usize = 3 * 1024;

    let s = TestServer::start().await;
    let chunk = Bytes::from(vec![0x5a_u8; CHUNK]);
    let head = Bytes::from(part_header("huge.bin"));
    let tail = Bytes::from(format!("\r\n--{BOUNDARY}--\r\n"));
    let parts = std::iter::once(head)
        .chain(std::iter::repeat_n(chunk, CHUNKS))
        .chain(std::iter::once(tail))
        .map(Ok::<_, std::io::Error>);
    let body = reqwest::Body::wrap_stream(futures::stream::iter(parts));

    let started = Instant::now();
    let res = s.post_upload(body).send().await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let elapsed = started.elapsed();

    let size = std::fs::metadata(s.path().join("huge.bin")).unwrap().len();
    assert_eq!(size, (CHUNK * CHUNKS) as u64);
    assert_eq!(s.files(), ["huge.bin"]);
    println!(
        "3 GB in {:.1}s ({:.0} MB/s)",
        elapsed.as_secs_f64(),
        size as f64 / 1_048_576.0 / elapsed.as_secs_f64()
    );
}

// ---- Listing and downloads (M3) ----

impl TestServer {
    async fn get(&self, path: &str) -> reqwest::Response {
        self.client
            .get(self.url(path))
            .header(COOKIE, self.cookie())
            .send()
            .await
            .unwrap()
    }

    async fn get_range(&self, path: &str, range: &str) -> reqwest::Response {
        self.client
            .get(self.url(path))
            .header(COOKIE, self.cookie())
            .header(reqwest::header::RANGE, range)
            .send()
            .await
            .unwrap()
    }

    fn put(&self, name: &str, data: &[u8], age_secs: u64) {
        let path = self.path().join(name);
        std::fs::write(&path, data).unwrap();
        let when = std::time::SystemTime::now() - Duration::from_secs(age_secs);
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(when)
            .unwrap();
    }
}

fn header<'a>(res: &'a reqwest::Response, name: &str) -> &'a str {
    res.headers()
        .get(name)
        .unwrap_or_else(|| panic!("missing {name}"))
        .to_str()
        .unwrap()
}

fn sample(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

#[tokio::test]
async fn listing_and_downloads_require_cookie() {
    let s = TestServer::start().await;
    s.put("a.txt", b"x", 0);
    for path in ["/api/files", "/api/files/a.txt"] {
        let res = s.client.get(s.url(path)).send().await.unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED, "{path}");
    }
}

#[tokio::test]
async fn listing_is_newest_first_and_hides_partials_and_folders() {
    let s = TestServer::start().await;
    s.put("old.txt", b"1", 3600);
    s.put("new.mp4", b"22", 5);
    s.put("mid.pdf", b"333", 600);
    s.put("upload.mp4.beam-0badc0de.part", b"partial", 0);
    std::fs::create_dir(s.path().join("folder")).unwrap();

    let res = s.get("/api/files").await;
    assert_eq!(res.status(), StatusCode::OK);
    let json: serde_json::Value = serde_json::from_str(&res.text().await.unwrap()).unwrap();
    let files = json["files"].as_array().unwrap();
    let got: Vec<(&str, u64)> = files
        .iter()
        .map(|f| (f["name"].as_str().unwrap(), f["size"].as_u64().unwrap()))
        .collect();
    assert_eq!(got, [("new.mp4", 2), ("mid.pdf", 3), ("old.txt", 1)]);
    assert!(files[0]["modified"].as_u64().unwrap() > files[2]["modified"].as_u64().unwrap());
}

#[tokio::test]
async fn uploaded_files_show_up_in_the_listing() {
    let s = TestServer::start().await;
    s.upload_one("from-phone.jpg", b"jpeg").await;
    let body = s.get("/api/files").await.text().await.unwrap();
    assert!(body.contains("\"from-phone.jpg\""), "{body}");
}

#[tokio::test]
async fn full_download_is_an_attachment_with_the_right_bytes() {
    let s = TestServer::start().await;
    let data = sample(300_000);
    s.put("clip.mp4", &data, 0);

    let res = s.get("/api/files/clip.mp4").await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(
        header(&res, "content-disposition"),
        "attachment; filename=\"clip.mp4\"; filename*=UTF-8''clip.mp4"
    );
    assert_eq!(header(&res, "accept-ranges"), "bytes");
    assert_eq!(header(&res, "content-length"), "300000");
    assert_eq!(header(&res, "content-type"), "video/mp4");
    assert_eq!(res.bytes().await.unwrap().as_ref(), data.as_slice());
}

#[tokio::test]
async fn range_requests_return_206_with_the_right_bytes() {
    let s = TestServer::start().await;
    let data = sample(1000);
    s.put("song.mp3", &data, 0);

    let res = s.get_range("/api/files/song.mp3", "bytes=10-19").await;
    assert_eq!(res.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(header(&res, "content-range"), "bytes 10-19/1000");
    assert_eq!(header(&res, "content-length"), "10");
    assert!(header(&res, "content-disposition").starts_with("attachment;"));
    assert_eq!(res.bytes().await.unwrap().as_ref(), &data[10..20]);

    // Resuming from an offset, as a browser does after a dropped download.
    let res = s.get_range("/api/files/song.mp3", "bytes=990-").await;
    assert_eq!(res.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(res.bytes().await.unwrap().as_ref(), &data[990..]);

    // The last N bytes.
    let res = s.get_range("/api/files/song.mp3", "bytes=-5").await;
    assert_eq!(res.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(res.bytes().await.unwrap().as_ref(), &data[995..]);
}

#[tokio::test]
async fn bad_and_multi_ranges_are_handled() {
    let s = TestServer::start().await;
    s.put("a.bin", &sample(1000), 0);

    let res = s.get_range("/api/files/a.bin", "bytes=5000-").await;
    assert_eq!(res.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(header(&res, "content-range"), "bytes */1000");

    // Several ranges in one request get the whole file rather than an error.
    let res = s.get_range("/api/files/a.bin", "bytes=0-1,5-6").await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.bytes().await.unwrap().len(), 1000);
}

#[tokio::test]
async fn head_reports_size_without_a_body() {
    let s = TestServer::start().await;
    s.put("a.bin", &sample(4321), 0);
    let res = s
        .client
        .head(s.url("/api/files/a.bin"))
        .header(COOKIE, s.cookie())
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(header(&res, "content-length"), "4321");
    assert!(res.bytes().await.unwrap().is_empty());
}

#[tokio::test]
async fn unicode_names_download_with_an_encoded_filename() {
    let s = TestServer::start().await;
    s.put("фото 📷.jpg", b"img", 0);
    let res = s
        .get("/api/files/%D1%84%D0%BE%D1%82%D0%BE%20%F0%9F%93%B7.jpg")
        .await;
    assert_eq!(res.status(), StatusCode::OK);
    let disposition = header(&res, "content-disposition").to_owned();
    assert!(
        disposition.ends_with("filename*=UTF-8''%D1%84%D0%BE%D1%82%D0%BE%20%F0%9F%93%B7.jpg"),
        "{disposition}"
    );
    assert_eq!(res.bytes().await.unwrap().as_ref(), b"img");
}

#[tokio::test]
async fn names_outside_the_listing_are_404() {
    let s = TestServer::start().await;
    s.put("a.txt", b"inside", 0);
    s.put("video.mp4.beam-0badc0de.part", b"partial", 0);
    std::fs::create_dir(s.path().join("sub")).unwrap();
    std::fs::write(s.path().join("sub").join("deep.txt"), b"deep").unwrap();
    // A real file just outside the shared folder, so traversal has a target.
    let secret_name = format!("beam-secret-{}.txt", &s.token[..8]);
    let secret = s.path().parent().unwrap().join(&secret_name);
    std::fs::write(&secret, b"secret").unwrap();

    let attempts = [
        format!("/api/files/..%2F{secret_name}"),
        format!("/api/files/..%5C{secret_name}"),
        format!("/api/files/%2E%2E%2F{secret_name}"),
        "/api/files/%2E%2E".to_owned(),
        "/api/files/sub%2Fdeep.txt".to_owned(),
        "/api/files/sub".to_owned(),
        format!(
            "/api/files/{}",
            secret
                .to_string_lossy()
                .replace('\\', "%5C")
                .replace(':', "%3A")
                .replace('/', "%2F")
        ),
        "/api/files/C%3A%5CWindows%5Cwin.ini".to_owned(),
        "/api/files/%2Fetc%2Fpasswd".to_owned(),
        "/api/files/a.txt%3Astream".to_owned(),
        "/api/files/a.txt%3A%3A%24DATA".to_owned(),
        "/api/files/CON".to_owned(),
        "/api/files/video.mp4.beam-0badc0de.part".to_owned(),
        "/api/files/missing.txt".to_owned(),
    ];
    for path in &attempts {
        let res = s.get(path).await;
        let status = res.status();
        let body = res.bytes().await.unwrap();
        assert_eq!(status, StatusCode::NOT_FOUND, "{path} gave {body:?}");
        assert!(!body.starts_with(b"secret") && !body.starts_with(b"deep"));
    }
    let _ = std::fs::remove_file(&secret);
}

/// reqwest normalizes `..` out of URLs, so send a literal one by hand.
#[tokio::test]
async fn literal_dot_dot_in_the_path_is_404() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let s = TestServer::start().await;
    s.put("a.txt", b"x", 0);
    for target in [
        "/api/files/../a.txt",
        "/api/files/../../etc/passwd",
        "/api/files/./a.txt/..",
    ] {
        let mut sock = tokio::net::TcpStream::connect(s.addr).await.unwrap();
        let req = format!(
            "GET {target} HTTP/1.1\r\nHost: x\r\nCookie: {}\r\nConnection: close\r\n\r\n",
            s.cookie()
        );
        sock.write_all(req.as_bytes()).await.unwrap();
        let mut reply = String::new();
        sock.read_to_string(&mut reply).await.unwrap();
        assert!(reply.starts_with("HTTP/1.1 404"), "{target}: {reply}");
    }
}

#[cfg(windows)]
#[tokio::test]
async fn hidden_files_are_not_listed_or_served() {
    use std::os::windows::fs::OpenOptionsExt;
    let s = TestServer::start().await;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .attributes(0x2) // FILE_ATTRIBUTE_HIDDEN
        .open(s.path().join("private.txt"))
        .unwrap();
    let listing = s.get("/api/files").await.text().await.unwrap();
    assert!(!listing.contains("private.txt"));
    assert_eq!(
        s.get("/api/files/private.txt").await.status(),
        StatusCode::NOT_FOUND
    );
}

// ---- Live updates (M4) ----

/// Just enough of a WebSocket client for these tests: handshake, then read
/// the server's (unmasked) frames. Server pings come every 20 s, longer
/// than any test runs, so control frames only need to be skipped.
struct WsClient {
    sock: tokio::net::TcpStream,
    buf: Vec<u8>,
}

impl WsClient {
    /// Returns the response status line on failure.
    async fn connect(s: &TestServer, cookie: Option<String>) -> Result<Self, String> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut sock = tokio::net::TcpStream::connect(s.addr).await.unwrap();
        let cookie = cookie
            .map(|c| format!("Cookie: {c}\r\n"))
            .unwrap_or_default();
        let req = format!(
            "GET /ws HTTP/1.1\r\nHost: {}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
             Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n{cookie}\r\n",
            s.addr
        );
        sock.write_all(req.as_bytes()).await.unwrap();
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        let end = loop {
            let n = sock.read(&mut chunk).await.unwrap();
            if n == 0 {
                return Err(String::from_utf8_lossy(&buf).into_owned());
            }
            buf.extend_from_slice(&chunk[..n]);
            if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break i + 4;
            }
        };
        let head = String::from_utf8_lossy(&buf[..end]).into_owned();
        let status = head.lines().next().unwrap_or("").to_owned();
        if !status.starts_with("HTTP/1.1 101") {
            return Err(status);
        }
        Ok(Self {
            sock,
            buf: buf[end..].to_vec(),
        })
    }

    async fn fill(&mut self, want: usize) -> bool {
        use tokio::io::AsyncReadExt;
        let mut chunk = [0u8; 4096];
        while self.buf.len() < want {
            match self.sock.read(&mut chunk).await {
                Ok(0) | Err(_) => return false,
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
            }
        }
        true
    }

    /// Next text message as JSON, or `None` on close or timeout.
    async fn next(&mut self, timeout: Duration) -> Option<serde_json::Value> {
        tokio::time::timeout(timeout, async {
            loop {
                if !self.fill(2).await {
                    return None;
                }
                let opcode = self.buf[0] & 0x0f;
                let (len, header) = match self.buf[1] & 0x7f {
                    126 => {
                        self.fill(4).await;
                        (u16::from_be_bytes([self.buf[2], self.buf[3]]) as usize, 4)
                    }
                    127 => {
                        self.fill(10).await;
                        let mut b = [0u8; 8];
                        b.copy_from_slice(&self.buf[2..10]);
                        (u64::from_be_bytes(b) as usize, 10)
                    }
                    n => (n as usize, 2),
                };
                if !self.fill(header + len).await {
                    return None;
                }
                let payload: Vec<u8> = self.buf.drain(..header + len).skip(header).collect();
                match opcode {
                    0x1 => return Some(serde_json::from_slice(&payload).unwrap()),
                    0x8 => return None,
                    _ => {} // ping/pong
                }
            }
        })
        .await
        .ok()
        .flatten()
    }

    /// Waits for an event matching `want`, skipping others.
    async fn expect(
        &mut self,
        what: &str,
        want: impl Fn(&serde_json::Value) -> bool,
    ) -> serde_json::Value {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut seen = Vec::new();
        while Instant::now() < deadline {
            match self.next(deadline - Instant::now()).await {
                Some(v) if want(&v) => return v,
                Some(v) => seen.push(v),
                None => break,
            }
        }
        panic!("no {what} event within 5s; saw {seen:?}");
    }

    /// Collects everything that arrives in `window`.
    async fn drain(&mut self, window: Duration) -> Vec<serde_json::Value> {
        let deadline = Instant::now() + window;
        let mut seen = Vec::new();
        while let Some(v) = self
            .next(deadline.saturating_duration_since(Instant::now()))
            .await
        {
            seen.push(v);
        }
        seen
    }
}

fn is_event(v: &serde_json::Value, kind: &str, name: &str) -> bool {
    v["type"] == kind && v["name"] == name
}

#[tokio::test]
async fn websocket_requires_cookie() {
    let s = TestServer::start().await;
    let err = WsClient::connect(&s, None)
        .await
        .err()
        .expect("must be refused");
    assert!(err.starts_with("HTTP/1.1 401"), "{err}");
    let err = WsClient::connect(&s, Some("beam_session=wrong".into()))
        .await
        .err()
        .expect("must be refused");
    assert!(err.starts_with("HTTP/1.1 401"), "{err}");
}

#[tokio::test]
async fn file_dropped_into_the_folder_is_announced() {
    let s = TestServer::start().await;
    let mut ws = WsClient::connect(&s, Some(s.cookie())).await.unwrap();
    let started = Instant::now();
    std::fs::write(s.path().join("dropped.pdf"), b"from explorer").unwrap();
    let v = ws
        .expect("file_added", |v| is_event(v, "file_added", "dropped.pdf"))
        .await;
    assert_eq!(v["size"], 13);
    assert!(v["modified"].as_u64().unwrap() > 0);
    // PRD: "within about a second".
    assert!(
        started.elapsed() < Duration::from_millis(1500),
        "took {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn removed_and_renamed_files_are_announced() {
    let s = TestServer::start().await;
    s.put("a.txt", b"1", 60);
    s.put("b.txt", b"2", 60);
    // Wait for the watcher to settle on the files created above.
    let mut ws = WsClient::connect(&s, Some(s.cookie())).await.unwrap();
    ws.drain(Duration::from_millis(1500)).await;

    std::fs::remove_file(s.path().join("a.txt")).unwrap();
    ws.expect("file_removed", |v| is_event(v, "file_removed", "a.txt"))
        .await;

    std::fs::rename(s.path().join("b.txt"), s.path().join("c.txt")).unwrap();
    ws.expect("file_removed", |v| is_event(v, "file_removed", "b.txt"))
        .await;
    ws.expect("file_added", |v| is_event(v, "file_added", "c.txt"))
        .await;
}

#[tokio::test]
async fn uploads_announce_completion_and_never_their_part_file() {
    let s = TestServer::start().await;
    let mut ws = WsClient::connect(&s, Some(s.cookie())).await.unwrap();
    let data = sample(3 * 1024 * 1024);
    s.upload_one("phone.mp4", &data).await;

    let done = ws
        .expect("upload_completed", |v| {
            is_event(v, "upload_completed", "phone.mp4")
        })
        .await;
    assert_eq!(done["size"], data.len());
    let rest = ws.drain(Duration::from_millis(1500)).await;
    assert!(
        rest.iter().any(|v| is_event(v, "file_added", "phone.mp4")),
        "{rest:?}"
    );
    assert!(
        !rest
            .iter()
            .any(|v| v["name"].as_str().is_some_and(|n| n.ends_with(".part"))),
        "{rest:?}"
    );
}

#[tokio::test]
async fn every_connected_page_gets_the_events() {
    let s = TestServer::start().await;
    let mut a = WsClient::connect(&s, Some(s.cookie())).await.unwrap();
    let mut b = WsClient::connect(&s, Some(s.cookie())).await.unwrap();
    std::fs::write(s.path().join("both.txt"), b"x").unwrap();
    a.expect("file_added", |v| is_event(v, "file_added", "both.txt"))
        .await;
    b.expect("file_added", |v| is_event(v, "file_added", "both.txt"))
        .await;
}

/// An Explorer-style copy: the size is set up front and the writer keeps the
/// file open. The phone must not see it until the copy finishes.
#[cfg(windows)]
#[tokio::test]
async fn files_still_being_copied_are_announced_only_when_done() {
    use std::io::Write;
    let s = TestServer::start().await;
    let mut ws = WsClient::connect(&s, Some(s.cookie())).await.unwrap();

    let mut writer = std::fs::File::create(s.path().join("big.mkv")).unwrap();
    writer.set_len(50 * 1024 * 1024).unwrap();
    for _ in 0..10 {
        writer.write_all(&[1u8; 64 * 1024]).unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    let during = ws.drain(Duration::from_millis(300)).await;
    assert!(
        !during.iter().any(|v| v["name"] == "big.mkv"),
        "announced while copying: {during:?}"
    );
    let listing = s.get("/api/files").await.text().await.unwrap();
    assert!(
        !listing.contains("big.mkv"),
        "listed while copying: {listing}"
    );
    assert_eq!(
        s.get("/api/files/big.mkv").await.status(),
        StatusCode::NOT_FOUND
    );

    drop(writer);
    let done = ws
        .expect("file_added", |v| is_event(v, "file_added", "big.mkv"))
        .await;
    assert_eq!(done["size"], 50 * 1024 * 1024);
}

// ---- QR page for the PC's browser (M5) ----

#[tokio::test]
async fn qr_page_requires_cookie() {
    let s = TestServer::start().await;
    let res = s.client.get(s.url("/qr")).send().await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn qr_page_shows_a_code_for_every_address() {
    let s = TestServer::start_with_phone_urls(|t| {
        vec![
            format!("http://192.168.1.5:8080/?t={t}"),
            format!("http://10.0.0.7:8080/?t={t}"),
        ]
    })
    .await;
    let res = s.get("/qr").await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(header(&res, "cache-control"), "no-store");
    let body = res.text().await.unwrap();
    assert_eq!(body.matches("<svg").count(), 2, "{body}");
    assert!(body.contains(&format!("http://192.168.1.5:8080/?t={}", s.token)));
    assert!(body.contains("data-pick=\"1\""), "address picker missing");
    // The first code is visible, the second starts hidden.
    assert!(body.contains("data-i=\"0\"><svg"));
    assert!(body.contains("data-i=\"1\" hidden><svg"));
}

#[tokio::test]
async fn qr_page_without_addresses_says_so() {
    let s = TestServer::start().await;
    let body = s.get("/qr").await.text().await.unwrap();
    assert!(body.contains("No local network address found"));
    assert!(!body.contains("<svg"));
}

#[tokio::test]
async fn exchange_with_next_qr_goes_to_the_qr_page_only() {
    let s = TestServer::start().await;
    let body = |next: &'static str| {
        let url = s.url(&format!("/?t={}&next={next}", s.token));
        let client = s.client.clone();
        async move { client.get(url).send().await.unwrap().text().await.unwrap() }
    };
    assert!(body("qr").await.contains(r#"content="0;url=/qr""#));
    // Anything else falls back to the app, so `next` can't redirect elsewhere.
    for other in ["https://evil.example", "//evil.example", "/api/files", "QR"] {
        let page = body(other).await;
        assert!(page.contains(r#"content="0;url=/""#), "{other}: {page}");
        assert!(!page.contains("evil"), "{other}: {page}");
    }
}

// ---- Clipboard ----

use beam::clipboard::{Clip, Format, Image};

impl TestServer {
    async fn put_clipboard(
        &self,
        content_type: &str,
        body: impl Into<reqwest::Body>,
    ) -> reqwest::Response {
        self.client
            .put(self.url("/api/clipboard"))
            .header(COOKIE, self.cookie())
            .header(CONTENT_TYPE, content_type)
            .body(body)
            .send()
            .await
            .unwrap()
    }
}

/// A 24-bit BMP laid out like the one the phone page builds.
fn bmp(width: u32, height: u32) -> Vec<u8> {
    let stride = (width as usize * 3).div_ceil(4) * 4;
    let pixels = stride * height as usize;
    let mut b = Vec::new();
    b.extend_from_slice(b"BM");
    b.extend_from_slice(&((54 + pixels) as u32).to_le_bytes());
    b.extend_from_slice(&[0; 4]);
    b.extend_from_slice(&54u32.to_le_bytes());
    b.extend_from_slice(&40u32.to_le_bytes());
    b.extend_from_slice(&width.to_le_bytes());
    b.extend_from_slice(&height.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&24u16.to_le_bytes());
    b.extend_from_slice(&[0; 24]);
    b.extend((0..pixels).map(|i| (i % 253) as u8));
    b
}

#[tokio::test]
async fn clipboard_requires_cookie() {
    let s = TestServer::start().await;
    s.clipboard.set(Some(Clip::Text("secret".into())));
    let res = s.client.get(s.url("/api/clipboard")).send().await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    assert!(!res.text().await.unwrap().contains("secret"));

    let res = s
        .client
        .put(s.url("/api/clipboard"))
        .header(CONTENT_TYPE, "text/plain")
        .body("intruder")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(s.clipboard.get(), Some(Clip::Text("secret".into())));
}

#[tokio::test]
async fn empty_clipboard_is_204() {
    let s = TestServer::start().await;
    let res = s.get("/api/clipboard").await;
    assert_eq!(res.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn text_goes_to_the_pc_clipboard_and_back() {
    let s = TestServer::start().await;
    let text = "Wi-Fi password: áºžeam ðŸš€\nline two";
    let res = s.put_clipboard("text/plain;charset=UTF-8", text).await;
    assert_eq!(res.status(), StatusCode::NO_CONTENT);
    assert_eq!(s.clipboard.get(), Some(Clip::Text(text.into())));

    let res = s.get("/api/clipboard").await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(header(&res, "content-type"), "text/plain; charset=utf-8");
    assert_eq!(header(&res, "cache-control"), "no-store");
    assert_eq!(res.text().await.unwrap(), text);
}

#[tokio::test]
async fn pc_clipboard_text_is_fetched() {
    let s = TestServer::start().await;
    s.clipboard.set(Some(Clip::Text("copied on the PC".into())));
    let res = s.get("/api/clipboard").await;
    assert_eq!(res.text().await.unwrap(), "copied on the PC");
}

#[tokio::test]
async fn image_goes_to_the_pc_clipboard_and_back() {
    let s = TestServer::start().await;
    let image = bmp(5, 3);
    let res = s.put_clipboard("image/bmp", image.clone()).await;
    assert_eq!(res.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        s.clipboard.get(),
        Some(Clip::Image(Image {
            format: Format::Bmp,
            bytes: image.clone()
        }))
    );

    let res = s.get("/api/clipboard").await;
    assert_eq!(header(&res, "content-type"), "image/bmp");
    assert_eq!(res.bytes().await.unwrap().as_ref(), image.as_slice());
}

#[tokio::test]
async fn pc_clipboard_png_is_fetched_as_png() {
    let s = TestServer::start().await;
    let png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x02\0\0\0\x02".to_vec();
    s.clipboard.set(Some(Clip::Image(Image {
        format: Format::Png,
        bytes: png.clone(),
    })));
    let res = s.get("/api/clipboard").await;
    assert_eq!(header(&res, "content-type"), "image/png");
    assert_eq!(res.bytes().await.unwrap().as_ref(), png.as_slice());
}

#[tokio::test]
async fn bad_clipboard_bodies_are_refused_and_change_nothing() {
    let s = TestServer::start().await;
    s.clipboard.set(Some(Clip::Text("keep me".into())));

    let mut cut_short = bmp(10, 10);
    cut_short.truncate(100);
    let cases: [(&str, Vec<u8>, StatusCode); 5] = [
        (
            "image/bmp",
            b"not an image".to_vec(),
            StatusCode::BAD_REQUEST,
        ),
        ("image/bmp", cut_short, StatusCode::BAD_REQUEST),
        (
            "text/plain",
            vec![0xff, 0xfe, 0x00],
            StatusCode::BAD_REQUEST,
        ),
        (
            "image/png",
            b"\x89PNG".to_vec(),
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
        ),
        (
            "application/octet-stream",
            b"x".to_vec(),
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
        ),
    ];
    for (content_type, body, status) in cases {
        let res = s.put_clipboard(content_type, body).await;
        assert_eq!(res.status(), status, "{content_type}");
        let json: serde_json::Value = serde_json::from_slice(&res.bytes().await.unwrap()).unwrap();
        assert!(json["error"].as_str().is_some_and(|e| !e.is_empty()));
    }
    assert_eq!(s.clipboard.get(), Some(Clip::Text("keep me".into())));
}

/// The 2 MB default would refuse a phone screenshot.
#[tokio::test]
async fn clipboard_accepts_images_over_two_megabytes() {
    let s = TestServer::start().await;
    let image = bmp(1080, 2400); // about 7.4 MB, a typical phone screenshot
    let res = s.put_clipboard("image/bmp", image.clone()).await;
    assert_eq!(res.status(), StatusCode::NO_CONTENT);
    assert!(matches!(s.clipboard.get(), Some(Clip::Image(img)) if img.bytes == image));
}
