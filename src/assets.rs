//! Static asset serving. Production builds embed `assets/` via `rust-embed`;
//! debug builds (with `debug-embed` cargo feature DISABLED) read from disk at
//! runtime, relative to `CARGO_MANIFEST_DIR`. This gives instant frontend
//! iteration without rebuilds.
//!
//! All asset responses carry the same strict CSP as the index page.
//!
//! Note: `cargo run` resolves disk paths relative to the manifest directory.
//! Use `cargo run --manifest-path …` if invoking from elsewhere.

use axum::body::Body;
use axum::extract::Path;
use axum::http::{Response, StatusCode, header};
use rust_embed::Embed;

#[derive(Embed)]
#[folder = "assets/"]
pub struct Assets;

pub const CSP: &str = "default-src 'self'; \
    connect-src 'self'; \
    script-src 'self'; \
    style-src 'self' 'unsafe-inline'; \
    img-src 'self' data:; \
    frame-ancestors 'none'";

pub fn apply_security_headers(builder: http::response::Builder) -> http::response::Builder {
    builder
        .header(header::CONTENT_SECURITY_POLICY, CSP)
        .header(header::X_FRAME_OPTIONS, "DENY")
        .header("X-Content-Type-Options", "nosniff")
        .header("Referrer-Policy", "no-referrer")
}

pub async fn serve(Path(path): Path<String>) -> Response<Body> {
    let path = path.trim_start_matches('/');
    serve_path(path).await
}

pub async fn serve_path(path: &str) -> Response<Body> {
    let Some(file) = Assets::get(path) else {
        return apply_security_headers(Response::builder().status(StatusCode::NOT_FOUND))
            .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
            .body(Body::from(format!("404: {path}\n")))
            .unwrap();
    };

    let mime = mime_guess::from_path(path).first_or_octet_stream();
    let body = Body::from(file.data.into_owned());

    apply_security_headers(Response::builder().status(StatusCode::OK))
        .header(header::CONTENT_TYPE, mime.essence_str())
        .body(body)
        .unwrap()
}
