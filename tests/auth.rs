//! HTTP integration tests for Unit 1 auth flow.
//!
//! We spin up the real `tmons::server::router(...)` with a known token and
//! drive it via `tower::ServiceExt::oneshot` so we don't need a real socket.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

use tmons::{
    AppState, Token, COOKIE_NAME, router,
};

fn make_state(token: Token, no_auth: bool) -> AppState {
    let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
    AppState {
        token: Arc::new(token),
        no_auth,
        socket: None,
        bound_addr: addr,
        shutdown: CancellationToken::new(),
        build_info: tmons::BuildInfo {
            version: "test",
            commit: "test",
        },
    }
}

fn req(method: Method, uri: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header(header::HOST, "127.0.0.1:8080")
        .body(Body::empty())
        .unwrap()
}

fn req_with_host(method: Method, uri: &str, host: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header(header::HOST, host)
        .body(Body::empty())
        .unwrap()
}

#[tokio::test]
async fn token_in_query_sets_cookie_and_redirects() {
    let token = Token::new_random().unwrap();
    let encoded = token.encoded();
    let state = make_state(token, false);

    let app = router(state);
    let response = app
        .oneshot(req(Method::GET, &format!("/?t={encoded}")))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::FOUND);
    let cookie = response
        .headers()
        .get(header::SET_COOKIE)
        .expect("Set-Cookie header")
        .to_str()
        .unwrap();
    assert!(cookie.starts_with(&format!("{COOKIE_NAME}={encoded}; ")), "got {cookie:?}");
    assert!(cookie.contains("HttpOnly"));
    assert!(cookie.contains("SameSite=Strict"));
    assert!(cookie.contains("Path=/"));

    let location = response
        .headers()
        .get(header::LOCATION)
        .unwrap()
        .to_str()
        .unwrap();
    assert_eq!(location, "/");
}

#[tokio::test]
async fn index_with_valid_cookie_returns_html_and_csp() {
    let token = Token::new_random().unwrap();
    let encoded = token.encoded();
    let state = make_state(token, false);

    let app = router(state);
    let request = Request::builder()
        .method(Method::GET)
        .uri("/")
        .header(header::HOST, "127.0.0.1:8080")
        .header(header::COOKIE, format!("{COOKIE_NAME}={encoded}"))
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let csp = response
        .headers()
        .get(header::CONTENT_SECURITY_POLICY)
        .expect("CSP header")
        .to_str()
        .unwrap();
    assert!(csp.contains("default-src 'self'"));
    assert!(csp.contains("frame-ancestors 'none'"));
    assert!(csp.contains("connect-src 'self'"));

    assert_eq!(
        response
            .headers()
            .get(header::X_FRAME_OPTIONS)
            .unwrap()
            .to_str()
            .unwrap(),
        "DENY"
    );

    let body = response.into_body().collect().await.unwrap().to_bytes();
    let body = std::str::from_utf8(&body).unwrap();
    assert!(body.contains("<title>tmons</title>"));
    assert!(body.contains("/assets/main.js"));
    assert!(body.contains("/assets/vendor/xterm.css"));
}

#[tokio::test]
async fn index_without_cookie_or_token_returns_401() {
    let token = Token::new_random().unwrap();
    let state = make_state(token, false);

    let app = router(state);
    let response = app.oneshot(req(Method::GET, "/")).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn index_with_wrong_cookie_returns_401() {
    let token = Token::new_random().unwrap();
    let state = make_state(token, false);

    let app = router(state);
    let request = Request::builder()
        .method(Method::GET)
        .uri("/")
        .header(header::HOST, "127.0.0.1:8080")
        .header(header::COOKIE, format!("{COOKIE_NAME}=different"))
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn invalid_token_in_query_returns_401() {
    let token = Token::new_random().unwrap();
    let state = make_state(token, false);

    let app = router(state);
    let response = app
        .oneshot(req(Method::GET, "/?t=invalid"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn evil_host_rejected_with_403_even_with_valid_cookie() {
    let token = Token::new_random().unwrap();
    let encoded = token.encoded();
    let state = make_state(token, false);

    let app = router(state);
    let request = Request::builder()
        .method(Method::GET)
        .uri("/")
        .header(header::HOST, "evil.com")
        .header(header::COOKIE, format!("{COOKIE_NAME}={encoded}"))
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn rebinding_subdomain_rejected() {
    let token = Token::new_random().unwrap();
    let state = make_state(token, false);

    let app = router(state);
    let response = app
        .oneshot(req_with_host(Method::GET, "/", "127.0.0.1.nip.io"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn decimal_loopback_host_accepted() {
    // `2130706433` == 0x7f000001 == 127.0.0.1. Should accept (it IS loopback).
    let token = Token::new_random().unwrap();
    let encoded = token.encoded();
    let state = make_state(token, false);

    let app = router(state);
    let request = Request::builder()
        .method(Method::GET)
        .uri("/")
        .header(header::HOST, "2130706433")
        .header(header::COOKIE, format!("{COOKIE_NAME}={encoded}"))
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn trailing_dot_localhost_accepted() {
    let token = Token::new_random().unwrap();
    let encoded = token.encoded();
    let state = make_state(token, false);

    let app = router(state);
    let request = Request::builder()
        .method(Method::GET)
        .uri("/")
        .header(header::HOST, "LOCALHOST.")
        .header(header::COOKIE, format!("{COOKIE_NAME}={encoded}"))
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn no_auth_mode_skips_cookie_check_but_still_enforces_host() {
    let token = Token::new_random().unwrap();
    let state = make_state(token, true);

    let app = router(state.clone());
    let response = app.oneshot(req(Method::GET, "/")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let app = router(state);
    let response = app
        .oneshot(req_with_host(Method::GET, "/", "evil.com"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn embedded_asset_served_with_correct_mime() {
    let token = Token::new_random().unwrap();
    let encoded = token.encoded();
    let state = make_state(token, false);

    let app = router(state);
    let request = Request::builder()
        .method(Method::GET)
        .uri("/assets/main.css")
        .header(header::HOST, "127.0.0.1:8080")
        .header(header::COOKIE, format!("{COOKIE_NAME}={encoded}"))
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get(header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap(),
        "text/css"
    );
    assert!(response.headers().contains_key(header::CONTENT_SECURITY_POLICY));
}

#[tokio::test]
async fn debug_state_returns_json() {
    let token = Token::new_random().unwrap();
    let encoded = token.encoded();
    let state = make_state(token, false);

    let app = router(state);
    let request = Request::builder()
        .method(Method::GET)
        .uri("/debug/state")
        .header(header::HOST, "127.0.0.1:8080")
        .header(header::COOKIE, format!("{COOKIE_NAME}={encoded}"))
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body = response.into_body().collect().await.unwrap().to_bytes();
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["build"]["version"], "test");
    assert_eq!(body["build"]["commit"], "test");
    assert_eq!(body["bound_addr"], "127.0.0.1:8080");
    assert_eq!(body["no_auth"], false);
}

#[tokio::test]
async fn debug_state_requires_cookie() {
    let token = Token::new_random().unwrap();
    let state = make_state(token, false);

    let app = router(state);
    let response = app
        .oneshot(req(Method::GET, "/debug/state"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}
