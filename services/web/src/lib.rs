//! The web service (Laysi/trustiot-v2#123; decision 2, decision 27): serves the SPA's static
//! files (frontend/, built into the image at `/srv/www`) on the app host. The ingress sends it
//! every path of the app host outside `/api`, `/auth` and `/internal`, without the cookie check
//! and without the session cookie, so the service has no API, no identity and no state:
//!
//! - a file of the site answers with its bytes;
//! - a path under `/static/` that is not a file answers 404;
//! - under `/sandbox/`, only the dashboard sandbox's files of the runtimes the build ships
//!   (`/sandbox/v<N>/<file>`, `crates/contract`'s `SandboxFile`) answer, with the sandbox's
//!   headers instead of the SPA's (Laysi/trustiot-v2#997); every other path there answers 404,
//!   the checker's host page included;
//! - any other path answers with `index.html`, so a reload of a deep link loads the SPA, whose
//!   router shows the page (or its own not-found page);
//! - only GET and HEAD; every response carries the [security headers](headers).
//!
//! - [`site`]: loading the files at startup.
//! - [`headers`]: the security headers and cache policies.
//! - [`config`]: the settings.
//!
//! See `README.md` in this crate.

pub mod config;
pub mod headers;
pub mod site;

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::header::{
    ALLOW, CACHE_CONTROL, CONTENT_LENGTH, CONTENT_TYPE, ETAG, HOST, HeaderMap, HeaderValue,
    IF_NONE_MATCH,
};
use axum::http::{Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use contract::api::dashboard::RuntimeVersion;
use contract::api::dashboard::sandbox::SandboxFile;
use service_kit::{BoxError, Context};

use crate::config::Config;
use crate::site::{CachePolicy, File, MAX_SITE_BYTES, Site};

/// The service's startup for `service_kit::run`: loads the site from `root_dir` and serves it.
/// Readiness needs nothing more: the files are in memory once this returns.
///
/// # Errors
///
/// When the site cannot be loaded (see [`site::SiteError`]). Nothing is served then: the
/// service exits with code 1.
pub async fn start(context: Context<Config>) -> Result<Router, BoxError> {
    let root = context.config().root_dir.clone();
    let site = tokio::task::spawn_blocking(move || Site::load(&root, MAX_SITE_BYTES)).await??;
    tracing::info!(files = site.len(), bytes = site.bytes(), "loaded the site");
    Ok(routes(Arc::new(site)))
}

/// The routes: every request goes to the site, whatever its path. service-kit adds `/healthz`
/// and `/readyz` in front, so the SPA must not use those two paths.
pub fn routes(site: Arc<Site>) -> Router {
    Router::new().fallback(serve).with_state(site)
}

async fn serve(
    State(site): State<Arc<Site>>,
    method: Method,
    uri: Uri,
    request: HeaderMap,
) -> Response {
    let head = match method {
        Method::GET => false,
        Method::HEAD => true,
        _ => return method_not_allowed(),
    };
    let path = uri.path();
    let host = request_host(&request, &uri);
    let headers = if is_sandbox(path) {
        match sandbox_file(path) {
            Some(sandbox) => Headers::Sandbox(sandbox),
            None => return not_found(host),
        }
    } else {
        Headers::Site(host)
    };
    let file = match site.get(path) {
        Some(file) => file,
        None if site::is_static(path) || matches!(headers, Headers::Sandbox(_)) => {
            return not_found(host);
        }
        None => site.index(),
    };
    if none_match(&request, &file.etag) {
        return not_modified(file, headers);
    }
    let mut response = if head {
        let mut response = Response::new(Body::empty());
        response
            .headers_mut()
            .insert(CONTENT_LENGTH, HeaderValue::from(file.body.len()));
        response
    } else {
        Response::new(Body::from(file.body.clone()))
    };
    let fields = response.headers_mut();
    fields.insert(CONTENT_TYPE, HeaderValue::from_static(file.content_type));
    describe(fields, file, headers);
    response
}

/// Which security headers a response gets.
#[derive(Clone, Copy)]
enum Headers<'a> {
    /// The SPA's, with the request's host for the CSP's `frame-src` (`headers::secure`).
    Site(Option<&'a str>),
    /// A sandbox file's (`headers::secure_sandbox`).
    Sandbox(SandboxFile),
}

/// The host the browser asked: the `Host` header, or the request target's authority. Only the
/// CSP's `frame-src` uses it, after `headers::frame_src` checked its form.
fn request_host<'a>(request: &'a HeaderMap, uri: &'a Uri) -> Option<&'a str> {
    match request.get(HOST) {
        Some(value) => value.to_str().ok(),
        None => uri.authority().map(axum::http::uri::Authority::as_str),
    }
}

/// Whether `path` is in the sandbox's directory, where only the sandbox's files answer.
fn is_sandbox(path: &str) -> bool {
    path == "/sandbox" || path.starts_with("/sandbox/")
}

/// The sandbox file at exactly `path`, of a runtime the build ships.
fn sandbox_file(path: &str) -> Option<SandboxFile> {
    RuntimeVersion::SHIPPED.iter().find_map(|runtime| {
        SandboxFile::ALL
            .into_iter()
            .find(|file| file.path(*runtime) == path)
    })
}

/// Whether `If-None-Match` lists the file's ETag (or `*`). Weak comparison, as RFC 9110
/// requires for this header: `W/"x"` matches `"x"`.
fn none_match(request: &HeaderMap, etag: &HeaderValue) -> bool {
    let etag = etag.as_bytes();
    request
        .get_all(IF_NONE_MATCH)
        .iter()
        .flat_map(|value| value.as_bytes().split(|byte| *byte == b','))
        .map(<[u8]>::trim_ascii)
        .any(|tag| tag == b"*" || tag.strip_prefix(b"W/").unwrap_or(tag) == etag)
}

/// The ETag, the cache policy and the security headers of a file's response.
fn describe(fields: &mut HeaderMap, file: &File, headers: Headers<'_>) {
    fields.insert(ETAG, file.etag.clone());
    fields.insert(
        CACHE_CONTROL,
        HeaderValue::from_static(match file.cache {
            CachePolicy::Revalidate => headers::CACHE_REVALIDATE,
            CachePolicy::Immutable => headers::CACHE_IMMUTABLE,
        }),
    );
    match headers {
        Headers::Site(host) => headers::secure(fields, host),
        Headers::Sandbox(file) => headers::secure_sandbox(fields, file),
    }
}

fn not_modified(file: &File, headers: Headers<'_>) -> Response {
    let mut response = StatusCode::NOT_MODIFIED.into_response();
    describe(response.headers_mut(), file, headers);
    response
}

fn not_found(host: Option<&str>) -> Response {
    error(StatusCode::NOT_FOUND, "not found\n", host)
}

fn method_not_allowed() -> Response {
    let mut response = error(StatusCode::METHOD_NOT_ALLOWED, "method not allowed\n", None);
    response
        .headers_mut()
        .insert(ALLOW, HeaderValue::from_static("GET, HEAD"));
    response
}

fn error(status: StatusCode, text: &'static str, host: Option<&str>) -> Response {
    let mut response = (status, text).into_response();
    let fields = response.headers_mut();
    fields.insert(CACHE_CONTROL, HeaderValue::from_static(headers::CACHE_NONE));
    headers::secure(fields, host);
    response
}
