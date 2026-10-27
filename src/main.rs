// Copyright (C) 2026 by MIT, All rights reserved
// Released under the Apache License, Version 2.0
// http://www.apache.org/licenses/LICENSE-2.0

use axum::{
    body::{Body, Bytes},
    extract::{Request, State},
    http::{header, HeaderMap, HeaderValue, Response, StatusCode, Uri},
    middleware::{self, Next},
    response::IntoResponse,
    routing::get,
    Router,
};
use debug_print::debug_eprintln;
use rust_embed::RustEmbed;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, PoisonError, RwLock};
use std::time::Instant;
use tokio::signal::unix::{signal, SignalKind};
use url::Url;

// 1. Tell Rust to bundle everything inside the "ui" folder directly into the binary
#[derive(RustEmbed)]
#[folder = "ui/"]
struct Asset;

#[tokio::main]
async fn main() {
    // 2. Map all fallback routes (like static assets) to our embed handler.
    //    The logging layer is added last so it wraps the routes *and* the fallback.
    let app = Router::new()
        .route("/healthz", get(health))
        .route("/", get(serve_index))
        // Without this, /index.html would hit the fallback and skip substitution.
        .route("/index.html", get(serve_index))
        .fallback(serve_asset)
        .layer(middleware::from_fn(log_requests))
        .with_state(AppState::default());

    let listener = tokio::net::TcpListener::bind("0.0.0.0:8080").await.unwrap();
    println!("Server running on http://0.0.0.0:8080");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .unwrap();

    println!("Shutdown complete");
}

// ---------------------------------------------------------------------------
// Signals
// ---------------------------------------------------------------------------

/// Resolves when SIGTERM (15) or SIGHUP (1) arrives, which ends `axum::serve`
/// and lets `main` return.
async fn shutdown_signal() {
    let mut sigterm = signal(SignalKind::terminate()).expect("failed to install SIGTERM handler");
    let mut sighup = signal(SignalKind::hangup()).expect("failed to install SIGHUP handler");

    tokio::select! {
        _ = sigterm.recv() => println!("Received SIGTERM, shutting down"),
        _ = sighup.recv()  => println!("Received SIGHUP, shutting down"),
    }
}

// ---------------------------------------------------------------------------
// Access logging
// ---------------------------------------------------------------------------

/// Pull the originating client address out of X-Forwarded-For.
///
/// X-Forwarded-For is a comma-separated chain, oldest first, so the leftmost
/// entry is the original client. Falls back to X-Real-IP, then to "-".
///
/// Only trust this if the reverse proxy in front of you overwrites (rather
/// than appends to) the header; otherwise a client can put anything it likes
/// in the leftmost position.
fn client_ip(headers: &HeaderMap) -> &str {
    // println!("headers = {:#?}", headers);
    if let Some(xff) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok())
        && let Some(first) = xff.split(',').next() {
            let first = first.trim();
            if !first.is_empty() {
                return first;
            }
        }

    headers
        .get("x-real-ip")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("-")
}

/// Log one line per request to stdout after the response is produced.
async fn log_requests(req: Request, next: Next) -> Response<Body> {
    let method = req.method().clone();
    let path = req.uri().path().to_owned();
    let ip = client_ip(req.headers()).to_owned();
    let user_agent = req
        .headers()
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("-")
        .to_owned();

    let started = Instant::now();
    let response = next.run(req).await;
    let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;

    // println! goes through a LineWriter, so each line is flushed as it is
    // written -- no buffering surprises when running under a container runtime.
    println!(
        "{} {} {} {} {:.1}ms \"{}\"",
        ip,
        method,
        path,
        response.status().as_u16(),
        elapsed_ms,
        user_agent
    );

    response
}

// ---------------------------------------------------------------------------
// Per-referrer index.html
// ---------------------------------------------------------------------------

/// Name of the JS constant in index.html whose string value gets replaced,
/// e.g. `const PARENT_ORIGIN = 'https://example.com';`
const PARENT_ORIGIN_NAME: &str = "PARENT_ORIGIN";

/// Upper bound on cached index variants. The Referer header is entirely
/// client-controlled, so without a cap anyone could grow this map without
/// limit by sending random hosts. Once full, new origins are still rendered
/// correctly, just not cached.
const MAX_CACHED_INDEXES: usize = 1024;

/// index.html split around the quoted value of `PARENT_ORIGIN = '...'`.
/// `before` ends just after the opening quote, `after` starts at the closing
/// quote, so rendering is `before + origin + after`.
struct IndexTemplate {
    before: String,
    after: String,
}

/// The embedded index.html, decoded and split once. None if the file is
/// missing, not UTF-8, or has no `PARENT_ORIGIN = '...'` assignment, in which
/// case the static file is served as-is.
static INDEX_TEMPLATE: LazyLock<Option<IndexTemplate>> = LazyLock::new(|| {
    let html = Asset::get("index.html")
        .and_then(|f| String::from_utf8(f.data.into_owned()).ok())?;
    let template = split_template(&html);
    if template.is_none() {
        eprintln!("index.html: no `{PARENT_ORIGIN_NAME} = '...'` assignment found; serving it unmodified");
    }
    template
});

/// Find the first `PARENT_ORIGIN = <quote>value<quote>` (quote may be ', " or `)
/// and split the text around `value`.
fn split_template(html: &str) -> Option<IndexTemplate> {
    for (pos, _) in html.match_indices(PARENT_ORIGIN_NAME) {
        let rest = &html[pos + PARENT_ORIGIN_NAME.len()..];
        let trimmed = rest.trim_start();
        // Must be an assignment, not `==` / `===` or part of a longer name.
        let Some(after_eq) = trimmed.strip_prefix('=') else { continue };
        if after_eq.starts_with('=') {
            continue;
        }
        let value_part = after_eq.trim_start();
        let Some(quote) = value_part.chars().next().filter(|c| matches!(c, '\'' | '"' | '`')) else {
            continue;
        };
        let value_start = html.len() - value_part.len() + 1; // just past opening quote
        let Some(value_len) = html[value_start..].find(quote) else { continue };
        let value_end = value_start + value_len;
        return Some(IndexTemplate {
            before: html[..value_start].to_owned(),
            after: html[value_end..].to_owned(),
        });
    }
    None
}

#[derive(Clone, Default)]
struct AppState {
    /// Rendered index.html keyed by referrer origin.
    index_cache: Arc<RwLock<HashMap<String, Bytes>>>,
}

impl AppState {
    /// Return index.html with PARENT_ORIGIN's value set to `origin`, rendering
    /// and caching it on first use. None if there is no usable template.
    fn index_for(&self, origin: &str) -> Option<Bytes> {
        if let Some(page) = self
            .index_cache
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(origin)
        {
            return Some(page.clone());
        }

        let t = INDEX_TEMPLATE.as_ref()?;
        let rendered = Bytes::from(format!("{}{}{}", t.before, origin, t.after));

        let mut cache = self
            .index_cache
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        // Another request may have rendered it while we weren't holding the lock.
        if let Some(page) = cache.get(origin) {
            return Some(page.clone());
        }
        if cache.len() < MAX_CACHED_INDEXES {
            debug_eprintln!("Caching index.html for origin {}", origin);
            cache.insert(origin.to_owned(), rendered.clone());
        }
        Some(rendered)
    }
}

/// Extract the origin (`scheme://host[:port]`) from a Referer value.
/// The port is included only when it isn't the scheme's default.
///
/// Only http/https referrers are accepted, and the result is restricted to
/// characters that can legitimately appear in an origin, so a hostile Referer
/// can't break out of the JS string literal or inject markup.
fn referer_origin(referer: &str) -> Option<String> {
    let url = Url::parse(referer).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    url.host_str()?;
    let origin = url.origin().ascii_serialization();

    let safe = origin.len() <= 300
        && origin.bytes().all(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_' | b':' | b'/' | b'[' | b']')
        });
    safe.then_some(origin)
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

// Handler for /healthz entry
async fn health() -> &'static str {
    "OK"
}

// Handler for the root path (and /index.html)
async fn serve_index(State(state): State<AppState>, headers: HeaderMap) -> Response<Body> {
    let origin = headers
        .get(header::REFERER)
        .and_then(|v| v.to_str().ok())
        .and_then(referer_origin);
    debug_eprintln!("Referer origin: {:?}", origin);

    let mut response = match origin.and_then(|o| state.index_for(&o)) {
        Some(page) => asset_response("index.html", Body::from(page)),
        // No (usable) referrer: serve the static file untouched.
        None => serve_asset_by_path("index.html"),
    };

    // The body now depends on Referer, so any shared cache in front of us
    // must not hand one host's page to another.
    response
        .headers_mut()
        .insert(header::VARY, HeaderValue::from_static("Referer"));
    response
}

// Fallback handler for assets like /style.css
async fn serve_asset(uri: Uri) -> impl IntoResponse {
    let path = uri.path().trim_start_matches('/');
    serve_asset_by_path(path)
}

// ---------------------------------------------------------------------------
// Caching
// ---------------------------------------------------------------------------

/// How long a given asset may be cached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CachePolicy {
    /// Content-addressed file: the name changes when the bytes change.
    Immutable,
    /// Bootstrap/selection script: must always be fetched fresh.
    NoStore,
    /// Everything else.
    Short,
}

const ONE_YEAR_SECS: u32 = 365 * 24 * 60 * 60; // 31_536_000
const FIVE_MINUTES_SECS: u32 = 5 * 60; // 300

/// Classify a request path.
///
/// Order matters: `*.nocache.js` also ends in `cache.js`, so it has to be
/// tested first or nocache files would be pinned for a year.
fn cache_policy(path: &str) -> CachePolicy {
    // Only look at the file name, not the directory components.
    let name = path.rsplit('/').next().unwrap_or(path);

    if name.ends_with("nocache.js") {
        CachePolicy::NoStore
    } else if name.ends_with("cache.js") && starts_with_hex(name) {
        CachePolicy::Immutable
    } else {
        CachePolicy::Short
    }
}

/// True if the file name begins with a hex digit.
///
/// If you want to be stricter and require the *entire* stem to be hex (which
/// is what GWT permutation names actually look like), swap the body for:
///
/// ```ignore
/// let stem = name.trim_end_matches(".cache.js");
/// !stem.is_empty() && stem.chars().all(|c| c.is_ascii_hexdigit())
/// ```
fn starts_with_hex(name: &str) -> bool {
    name.chars().next().is_some_and(|c| c.is_ascii_hexdigit())
}

/// Build a 200 response for `path` with its content type and cache headers.
fn asset_response(path: &str, body: Body) -> Response<Body> {
    // Deduce mime type based on file extension
    let mime = mime_guess::from_path(path).first_or_octet_stream();

    let policy = cache_policy(path);
    debug_eprintln!("Cache policy: {:?}", policy);

    let mut builder = Response::builder().header(header::CONTENT_TYPE, mime.as_ref());

    match policy {
        CachePolicy::Immutable => {
            builder = builder.header(
                header::CACHE_CONTROL,
                format!("public, max-age={ONE_YEAR_SECS}, immutable"),
            );
        }
        CachePolicy::NoStore => {
            builder = builder
                .header(
                    header::CACHE_CONTROL,
                    "no-store, no-cache, must-revalidate, max-age=0",
                )
                // Belt and braces for HTTP/1.0 caches and old proxies.
                .header(header::PRAGMA, "no-cache")
                .header(header::EXPIRES, "0");
        }
        CachePolicy::Short => {
            builder = builder.header(
                header::CACHE_CONTROL,
                format!("public, max-age={FIVE_MINUTES_SECS}"),
            );
        }
    }

    builder.body(body).unwrap()
}

// Core helper to retrieve files out of the compiled binary memory space
fn serve_asset_by_path(path: &str) -> Response<Body> {
    debug_eprintln!("Path: {}", path);
    match Asset::get(path) {
        Some(content) => asset_response(path, Body::from(content.data)),
        None => Response::builder()
            .status(StatusCode::NOT_FOUND)
            .header(header::CACHE_CONTROL, "no-store")
            .body(Body::from("404 Not Found"))
            .unwrap(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_paths() {
        assert_eq!(
            cache_policy("A1B2C3D4E5F60718293A4B5C6D7E8F90.cache.js"),
            CachePolicy::Immutable
        );
        assert_eq!(cache_policy("deadbeef.cache.js"), CachePolicy::Immutable);
        assert_eq!(
            cache_policy("sub/dir/0123abcd.cache.js"),
            CachePolicy::Immutable
        );

        // nocache wins even though it also ends in "cache.js"
        assert_eq!(cache_policy("myapp.nocache.js"), CachePolicy::NoStore);
        assert_eq!(cache_policy("abc123.nocache.js"), CachePolicy::NoStore);

        // Leading char is not a hex digit
        assert_eq!(cache_policy("zebra.cache.js"), CachePolicy::Short);

        assert_eq!(cache_policy("index.html"), CachePolicy::Short);
        assert_eq!(cache_policy("style.css"), CachePolicy::Short);
    }

    #[test]
    fn extracts_client_ip() {
        let mut headers = HeaderMap::new();
        assert_eq!(client_ip(&headers), "-");

        headers.insert("x-real-ip", HeaderValue::from_static("10.0.0.9"));
        assert_eq!(client_ip(&headers), "10.0.0.9");

        headers.insert(
            "x-forwarded-for",
            HeaderValue::from_static("203.0.113.7, 10.0.0.1, 10.0.0.2"),
        );
        assert_eq!(client_ip(&headers), "203.0.113.7");
    }

    #[test]
    fn extracts_referer_origin() {
        assert_eq!(
            referer_origin("https://ai2.appinventor.mit.edu/some/page?x=1").as_deref(),
            Some("https://ai2.appinventor.mit.edu")
        );
        // Non-default port is kept, default port is dropped.
        assert_eq!(
            referer_origin("http://localhost:8888/").as_deref(),
            Some("http://localhost:8888")
        );
        assert_eq!(
            referer_origin("https://Example.COM:443/").as_deref(),
            Some("https://example.com")
        );
        assert_eq!(
            referer_origin("http://[::1]:3000/").as_deref(),
            Some("http://[::1]:3000")
        );

        // Rejected
        assert_eq!(referer_origin("not a url"), None);
        assert_eq!(referer_origin("javascript:alert(1)"), None);
        assert_eq!(referer_origin("file:///etc/passwd"), None);
        assert_eq!(referer_origin("data:text/html,hi"), None);
    }

    #[test]
    fn hostile_referer_cannot_escape_string() {
        for r in [
            "http://evil\"><script>x</script>/",
            "http://a'b.com/",
            "http://a<b.com/",
            "http://a`b.com/",
        ] {
            if let Some(o) = referer_origin(r) {
                assert!(!o.contains(['<', '>', '"', '\'', '`', '\\']), "{r} -> {o}");
            }
        }
    }

    fn render(html: &str, origin: &str) -> Option<String> {
        split_template(html).map(|t| format!("{}{}{}", t.before, origin, t.after))
    }

    #[test]
    fn replaces_parent_origin_value() {
        let html = "<script>\nconst PARENT_ORIGIN = 'https://example.com';\n</script>";
        assert_eq!(
            render(html, "https://foo.org").as_deref(),
            Some("<script>\nconst PARENT_ORIGIN = 'https://foo.org';\n</script>")
        );

        // Double quotes, no spaces
        assert_eq!(
            render(r#"const PARENT_ORIGIN="https://example.com";"#, "http://h:1").as_deref(),
            Some(r#"const PARENT_ORIGIN="http://h:1";"#)
        );

        // Comparisons and earlier uses are skipped; the assignment is found.
        let html = "if (x === PARENT_ORIGIN) {}\nconst PARENT_ORIGIN = 'https://example.com';";
        assert_eq!(
            render(html, "https://foo.org").as_deref(),
            Some("if (x === PARENT_ORIGIN) {}\nconst PARENT_ORIGIN = 'https://foo.org';")
        );
        assert!(render("if (o == PARENT_ORIGIN) {}", "x").is_none());

        // No assignment at all
        assert!(render("<html></html>", "https://foo.org").is_none());
    }
}
