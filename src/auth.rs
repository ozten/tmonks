//! Auth and same-origin enforcement for tmons.
//!
//! tmons binds to 127.0.0.1 and trusts:
//! 1. The user's host file system (anyone with the URL token can drive shells).
//! 2. The browser's same-origin policy.
//!
//! It does NOT trust:
//! - Other local services on different loopback ports (CSWSH defense).
//! - Malicious websites the user might also have open (DNS-rebinding defense).
//! - Future XSS / hostile extensions writing into the page (CSP).
//!
//! The token is generated once at startup, presented once via `?t=<token>`,
//! and persisted in an `HttpOnly` `SameSite=Strict` cookie for the rest of the
//! session. All HTTP routes and WebSocket upgrades validate the cookie via
//! `subtle::ConstantTimeEq`.
//!
//! The query string is the **only** way to obtain the cookie; existing cookies
//! without a matching server-side token are rejected.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use anyhow::Result;
use axum::{
    body::Body,
    extract::{Query, Request, State},
    http::{HeaderMap, Method, Response, StatusCode, header},
    middleware::Next,
};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::TryRngCore;
use serde::Deserialize;
use subtle::ConstantTimeEq;

use crate::server::AppState;

pub const COOKIE_NAME: &str = "tmons_session";

/// The server-side token. Stored in `AppState` and compared in constant time
/// against incoming cookies.
#[derive(Clone, Debug)]
pub struct Token(Vec<u8>);

impl Token {
    pub fn new_random() -> Result<Self> {
        let mut buf = [0u8; 32];
        rand::rngs::OsRng
            .try_fill_bytes(&mut buf)
            .map_err(|e| anyhow::anyhow!("OsRng failed: {e}"))?;
        Ok(Self(buf.to_vec()))
    }

    /// Returns the URL-safe, padding-less base64 of the raw bytes.
    /// This is the value that appears in the printed URL and in the cookie.
    pub fn encoded(&self) -> String {
        URL_SAFE_NO_PAD.encode(&self.0)
    }

    pub fn matches(&self, encoded: &str) -> bool {
        // Decode the incoming value first; reject if invalid.
        let Ok(provided) = URL_SAFE_NO_PAD.decode(encoded.as_bytes()) else {
            return false;
        };
        provided.as_slice().ct_eq(&self.0).into()
    }
}

#[derive(Debug, Deserialize)]
pub struct TokenQuery {
    pub t: Option<String>,
}

/// Handler for `GET /?t=<token>` — sets the cookie and 302s to `/`.
///
/// Browsers visiting `/?t=<token>` with a valid token receive:
///   * `Set-Cookie: tmons_session=<token>; HttpOnly; SameSite=Strict; Path=/`
///   * `302 Location: /`
///
/// Visiting `/?t=<token>` with an invalid token returns 401.
///
/// This is the only path that mints the cookie. All other routes must arrive
/// already carrying it.
pub async fn token_redirect(
    State(state): State<AppState>,
    Query(q): Query<TokenQuery>,
) -> Response<Body> {
    if state.no_auth {
        return Response::builder()
            .status(StatusCode::FOUND)
            .header(header::LOCATION, "/")
            .body(Body::empty())
            .unwrap();
    }

    let Some(provided) = q.t.as_deref() else {
        return unauthorized("missing ?t=<token>");
    };

    if !state.token.matches(provided) {
        return unauthorized("invalid token");
    }

    let cookie_value = format!(
        "{}={}; HttpOnly; SameSite=Strict; Path=/",
        COOKIE_NAME,
        state.token.encoded()
    );

    Response::builder()
        .status(StatusCode::FOUND)
        .header(header::LOCATION, "/")
        .header(header::SET_COOKIE, cookie_value)
        .body(Body::empty())
        .unwrap()
}

fn unauthorized(msg: &str) -> Response<Body> {
    Response::builder()
        .status(StatusCode::UNAUTHORIZED)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(Body::from(format!("401 Unauthorized: {msg}\n")))
        .unwrap()
}

fn forbidden(msg: &str) -> Response<Body> {
    Response::builder()
        .status(StatusCode::FORBIDDEN)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(Body::from(format!("403 Forbidden: {msg}\n")))
        .unwrap()
}

/// Middleware enforcing:
///   * Host header parses to a loopback address (rejects DNS rebinding).
///   * Cookie matches the server-side token (rejects unauthenticated requests).
///
/// `/?t=<token>` is exempted from the cookie check because it's the path that
/// mints the cookie. The Host check applies to it too.
pub async fn auth_middleware(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response<Body> {
    // Always enforce Host loopback (defense against DNS rebinding).
    if let Err(msg) = check_host(request.headers()) {
        tracing::warn!(reason = %msg, "rejecting request: bad Host");
        return forbidden(&msg);
    }

    if state.no_auth {
        return next.run(request).await;
    }

    let path = request.uri().path();
    let is_token_handshake = path == "/" && request.method() == Method::GET && {
        request
            .uri()
            .query()
            .map(|q| q.split('&').any(|p| p.starts_with("t=")))
            .unwrap_or(false)
    };

    if is_token_handshake {
        return next.run(request).await;
    }

    if !check_cookie(request.headers(), &state.token) {
        return unauthorized("missing or invalid cookie; visit the URL printed at startup");
    }

    next.run(request).await
}

/// Origin check used on WebSocket upgrades only. Must exact-match the request's
/// Host (host + port). Defends against CSWSH from another local service running
/// on a different loopback port — SameSite=Strict treats same-host-different-port
/// as same-site and would otherwise send the cookie.
///
/// Wired by Units 4 and 5 (the WS upgrade handlers).
#[allow(dead_code)]
pub fn check_origin_for_ws(headers: &HeaderMap) -> Result<(), String> {
    let host = header_str(headers, header::HOST).ok_or("missing Host header")?;
    let origin = header_str(headers, header::ORIGIN).ok_or("missing Origin header")?;

    let origin_url = url::Url::parse(origin).map_err(|_| "Origin is not a valid URL".to_string())?;
    let origin_host = origin_url
        .host_str()
        .ok_or("Origin has no host")?
        .to_ascii_lowercase();
    let origin_port = origin_url
        .port_or_known_default()
        .ok_or("Origin has no port")?;

    let (host_host, host_port) = split_host_port(host)?;
    let host_host = host_host.trim_end_matches('.').to_ascii_lowercase();

    if origin_host != host_host {
        return Err(format!(
            "Origin host {origin_host:?} != Host host {host_host:?}",
        ));
    }
    if origin_port != host_port {
        return Err(format!(
            "Origin port {origin_port} != Host port {host_port}",
        ));
    }
    Ok(())
}

fn check_host(headers: &HeaderMap) -> Result<(), String> {
    let host = header_str(headers, header::HOST).ok_or_else(|| "missing Host header".to_string())?;
    let (host_part, _port) = split_host_port(host)?;
    let host_part = host_part.trim_end_matches('.').to_ascii_lowercase();

    // Accept the literal `localhost` (resolves to loopback by convention).
    if host_part == "localhost" {
        return Ok(());
    }

    // Parse as an IP, including the legacy `inet_aton` shorthands a browser
    // or malicious tool might use to dodge a naive string check (`127.1`,
    // `2130706433`, `0x7f000001`, IPv4-mapped IPv6, etc.).
    let addr = parse_ip_with_legacy_forms(&host_part).ok_or_else(|| {
        format!("Host {host_part:?} is not a loopback IP and is not `localhost`")
    })?;

    if !addr.is_loopback() {
        return Err(format!("Host {host_part:?} -> {addr} is not loopback"));
    }
    Ok(())
}

/// Parse an IP address including the `inet_aton`-style legacy IPv4 forms.
///
/// Examples this must accept:
///   * `127.0.0.1`        (canonical dotted-quad)
///   * `127.1`            (one-byte `a` + 24-bit `b`)
///   * `127.0.1`          (two-byte `a.b` + 16-bit `c`)
///   * `2130706433`       (single decimal)
///   * `0x7f000001`       (single hex)
///   * `0177.0.0.1`       (octal with leading 0; per inet_aton)
///   * `[::1]`            (IPv6 literal — handled by standard parser)
///   * `::ffff:127.0.0.1` (IPv4-mapped IPv6)
///
/// This is intentionally permissive — the *security check* is `is_loopback()`
/// on the parsed `IpAddr`, not on the string form.
fn parse_ip_with_legacy_forms(s: &str) -> Option<IpAddr> {
    // Standard parser handles dotted-quad and IPv6.
    if let Ok(addr) = s.parse::<IpAddr>() {
        return Some(addr);
    }

    // Otherwise try the legacy IPv4 dotted-shorthand and single-number forms.
    let parts: Vec<&str> = s.split('.').collect();
    if parts.is_empty() || parts.iter().any(|p| p.is_empty()) {
        return None;
    }

    let nums: Vec<u32> = parts
        .iter()
        .map(|p| parse_legacy_u32(p))
        .collect::<Option<Vec<_>>>()?;

    let combined: u32 = match nums.as_slice() {
        // Single number: full 32 bits.
        [a] => *a,
        // a.b: a is high 8 bits, b is low 24 bits.
        [a, b] => {
            if *a > 0xFF || *b > 0x00FF_FFFF {
                return None;
            }
            (a << 24) | b
        }
        // a.b.c: a, b are high 8 bits each, c is low 16 bits.
        [a, b, c] => {
            if *a > 0xFF || *b > 0xFF || *c > 0xFFFF {
                return None;
            }
            (a << 24) | (b << 16) | c
        }
        // a.b.c.d covered by the standard parser; reject if it got here
        // (means at least one part was non-numeric).
        _ => return None,
    };

    Some(IpAddr::V4(Ipv4Addr::from(combined)))
}

/// Parse a single component of a legacy IPv4 string: accepts decimal, hex
/// (with `0x` prefix), and octal (with leading `0`).
fn parse_legacy_u32(s: &str) -> Option<u32> {
    if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        return u32::from_str_radix(hex, 16).ok();
    }
    if let Some(rest) = s.strip_prefix('0')
        && !rest.is_empty()
        && rest.chars().all(|c| c.is_ascii_digit())
    {
        // inet_aton treats `0NNN` as octal. We replicate that.
        return u32::from_str_radix(rest, 8).ok();
    }
    s.parse::<u32>().ok()
}

fn check_cookie(headers: &HeaderMap, token: &Token) -> bool {
    let Some(cookie_header) = header_str(headers, header::COOKIE) else {
        return false;
    };
    for piece in cookie_header.split(';') {
        let piece = piece.trim();
        if let Some(rest) = piece.strip_prefix(COOKIE_NAME) {
            if let Some(value) = rest.strip_prefix('=') {
                return token.matches(value);
            }
        }
    }
    false
}

fn header_str(headers: &HeaderMap, name: header::HeaderName) -> Option<&str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

fn split_host_port(host: &str) -> Result<(&str, u16), String> {
    // IPv6 literal: `[::1]:8080` or `[::1]`.
    if let Some(stripped) = host.strip_prefix('[') {
        let close = stripped
            .find(']')
            .ok_or("malformed IPv6 host: missing ']'")?;
        let host_part = &stripped[..close];
        let after = &stripped[close + 1..];
        let port = if let Some(p) = after.strip_prefix(':') {
            p.parse::<u16>().map_err(|_| "invalid port".to_string())?
        } else {
            80
        };
        return Ok((host_part, port));
    }

    if let Some((h, p)) = host.rsplit_once(':') {
        // Only treat as host:port if `p` parses; otherwise it might be an IPv6
        // (already handled above) or a bare hostname containing a colon.
        if let Ok(port) = p.parse::<u16>() {
            return Ok((h, port));
        }
    }
    Ok((host, 80))
}

/// Resolve the bound `SocketAddr` to the user-facing URL printed at startup.
///
/// The bound address may be `0.0.0.0` (rejected at CLI parse) or `127.0.0.1`.
/// Use the bind IP literally when constructing the URL.
pub fn print_startup_url(addr: SocketAddr, token: &Token) -> String {
    let ip = addr.ip();
    let port = addr.port();
    let host = match ip {
        IpAddr::V6(_) => format!("[{ip}]:{port}"),
        IpAddr::V4(_) => format!("{ip}:{port}"),
    };
    format!("http://{host}/?t={}", token.encoded())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn fake_token() -> Token {
        Token(vec![0xab; 32])
    }

    #[test]
    fn token_matches_only_its_own_encoding() {
        let t = fake_token();
        let encoded = t.encoded();
        assert!(t.matches(&encoded));
        assert!(!t.matches(""));
        assert!(!t.matches("bogus"));
        assert!(!t.matches(&format!("{encoded}x")));
    }

    #[test]
    fn token_matches_is_constant_time_for_correct_length() {
        let t = fake_token();
        // Different value of correct length should not match.
        let other = URL_SAFE_NO_PAD.encode([0x00u8; 32]);
        assert!(!t.matches(&other));
    }

    #[test]
    fn host_accepts_loopback_forms() {
        for value in [
            "127.0.0.1",
            "127.0.0.1:8080",
            "localhost",
            "localhost:8080",
            "LOCALHOST.",
            "LOCALHOST.:8080",
            "[::1]",
            "[::1]:8080",
            "127.1",     // shorthand for 127.0.0.1
            "2130706433",// decimal 127.0.0.1
        ] {
            let mut h = HeaderMap::new();
            h.insert(header::HOST, HeaderValue::from_str(value).unwrap());
            assert!(
                check_host(&h).is_ok(),
                "expected accept for Host {value:?}: {:?}",
                check_host(&h)
            );
        }
    }

    #[test]
    fn host_rejects_non_loopback() {
        for value in [
            "evil.com",
            "evil.com:8080",
            "127.0.0.1.nip.io",
            "8.8.8.8",
            "10.0.0.1",
            "",
        ] {
            let mut h = HeaderMap::new();
            if !value.is_empty() {
                h.insert(header::HOST, HeaderValue::from_str(value).unwrap());
            }
            assert!(check_host(&h).is_err(), "expected reject for Host {value:?}");
        }
    }

    #[test]
    fn origin_check_accepts_matching_host_and_port() {
        let mut h = HeaderMap::new();
        h.insert(header::HOST, HeaderValue::from_static("127.0.0.1:8080"));
        h.insert(
            header::ORIGIN,
            HeaderValue::from_static("http://127.0.0.1:8080"),
        );
        check_origin_for_ws(&h).unwrap();
    }

    #[test]
    fn origin_check_rejects_port_mismatch() {
        let mut h = HeaderMap::new();
        h.insert(header::HOST, HeaderValue::from_static("127.0.0.1:8080"));
        h.insert(
            header::ORIGIN,
            HeaderValue::from_static("http://127.0.0.1:3000"),
        );
        let err = check_origin_for_ws(&h).unwrap_err();
        assert!(err.contains("port"), "{err}");
    }

    #[test]
    fn origin_check_rejects_missing_origin() {
        let mut h = HeaderMap::new();
        h.insert(header::HOST, HeaderValue::from_static("127.0.0.1:8080"));
        check_origin_for_ws(&h).unwrap_err();
    }

    #[test]
    fn origin_check_handles_case_and_trailing_dot() {
        let mut h = HeaderMap::new();
        h.insert(header::HOST, HeaderValue::from_static("LOCALHOST.:8080"));
        h.insert(
            header::ORIGIN,
            HeaderValue::from_static("http://localhost:8080"),
        );
        check_origin_for_ws(&h).unwrap();
    }

    #[test]
    fn print_url_encodes_token_and_port() {
        let token = fake_token();
        let url = print_startup_url("127.0.0.1:8765".parse().unwrap(), &token);
        assert!(url.starts_with("http://127.0.0.1:8765/?t="));
        assert!(url.contains(&token.encoded()));
    }

    #[test]
    fn cookie_parser_finds_value_among_others() {
        let mut h = HeaderMap::new();
        h.insert(
            header::COOKIE,
            HeaderValue::from_str(&format!("foo=bar; {COOKIE_NAME}={}; bar=baz", fake_token().encoded()))
                .unwrap(),
        );
        assert!(check_cookie(&h, &fake_token()));
    }

    #[test]
    fn cookie_parser_rejects_mismatch() {
        let mut h = HeaderMap::new();
        h.insert(
            header::COOKIE,
            HeaderValue::from_str(&format!("{COOKIE_NAME}=different")).unwrap(),
        );
        assert!(!check_cookie(&h, &fake_token()));
    }
}
