use std::net::SocketAddr;

use beam::server::{self, AppState};
use reqwest::StatusCode;
use reqwest::header::{COOKIE, SET_COOKIE};
use tempfile::TempDir;

struct TestServer {
    base: String,
    token: String,
    client: reqwest::Client,
    _dir: TempDir,
}

impl TestServer {
    async fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let token = beam::config::generate_token();
        let state = AppState::new(token.clone(), dir.path().canonicalize().unwrap());
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
            token,
            client,
            _dir: dir,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    fn cookie(&self) -> String {
        format!("beam_session={}", self.token)
    }
}

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
