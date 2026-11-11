use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use subtle::ConstantTimeEq;

use crate::server::AppState;

pub const COOKIE_NAME: &str = "beam_session";

/// Constant-time token comparison. Only the length can leak, and that is public anyway.
pub fn tokens_match(expected: &str, provided: &str) -> bool {
    expected.as_bytes().ct_eq(provided.as_bytes()).into()
}

/// True if any `beam_session` cookie in the request carries the current token.
/// Checks all of them because a browser may still hold one from an older run.
pub fn has_valid_cookie(headers: &HeaderMap, token: &str) -> bool {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .filter(|(name, _)| *name == COOKIE_NAME)
        .any(|(_, value)| tokens_match(token, value))
}

/// How long the phone stays logged in, at most. Without a `Max-Age` the
/// cookie would only last for the browser session, and Android often closes
/// Chrome in the background, so the phone was asked to scan again after
/// just switching apps. The cookie still stops working the moment Beam
/// exits, because the token changes on every run.
pub const COOKIE_MAX_AGE_SECS: u32 = 24 * 60 * 60;

/// No `Secure`, because Beam is plain HTTP.
pub fn session_cookie(token: &str) -> String {
    format!(
        "{COOKIE_NAME}={token}; Path=/; Max-Age={COOKIE_MAX_AGE_SECS}; HttpOnly; SameSite=Strict"
    )
}

pub const UNAUTHORIZED_TEXT: &str = "Not connected. Scan the QR code shown on the PC again.";

/// Runs on every route. `/` is let through because its handler does the token
/// exchange itself and serves a friendly HTML page on failure.
pub async fn require_auth(State(state): State<AppState>, req: Request, next: Next) -> Response {
    if req.uri().path() == "/" || has_valid_cookie(req.headers(), &state.token) {
        return next.run(req).await;
    }
    (StatusCode::UNAUTHORIZED, UNAUTHORIZED_TEXT).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers(cookies: &[&str]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for c in cookies {
            h.append(header::COOKIE, HeaderValue::from_str(c).unwrap());
        }
        h
    }

    #[test]
    fn match_is_exact() {
        assert!(tokens_match("abc123", "abc123"));
        assert!(!tokens_match("abc123", "abc124"));
        assert!(!tokens_match("abc123", "abc12"));
        assert!(!tokens_match("abc123", ""));
    }

    #[test]
    fn finds_cookie_among_others() {
        let h = headers(&["theme=dark; beam_session=tok; other=1"]);
        assert!(has_valid_cookie(&h, "tok"));
    }

    #[test]
    fn accepts_when_one_of_several_matches() {
        let h = headers(&["beam_session=old", "beam_session=tok"]);
        assert!(has_valid_cookie(&h, "tok"));
    }

    #[test]
    fn rejects_missing_wrong_or_misnamed() {
        assert!(!has_valid_cookie(&headers(&[]), "tok"));
        assert!(!has_valid_cookie(&headers(&["beam_session=nope"]), "tok"));
        assert!(!has_valid_cookie(&headers(&["xbeam_session=tok"]), "tok"));
        assert!(!has_valid_cookie(&headers(&["beam_session="]), "tok"));
    }

    #[test]
    fn cookie_attributes() {
        let c = session_cookie("tok");
        assert!(c.starts_with("beam_session=tok;"));
        assert!(c.contains("HttpOnly"));
        assert!(c.contains("SameSite=Strict"));
        assert!(c.contains("Path=/"));
        // Survives the phone's browser restarting (but not Beam restarting).
        assert!(c.contains("Max-Age=86400"));
    }
}
