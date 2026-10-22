//! The web service (Laysi/trustiot-v2#123; decision 2, decision 27): serves the SPA's static
//! files (frontend/, built into the image at `/srv/www`) on the app host. The ingress sends it
//! every path of the app host outside `/api`, `/auth` and `/internal`, without the cookie check
//! and without the session cookie, so the service has no API, no identity and no state:
//!
//! - a file of the site answers with its bytes;
//! - a path under `/static/` that is not a file answers 404;
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
    ALLOW, CACHE_CONTROL, CONTENT_LENGTH, CONTENT_TYPE, ETAG, HeaderMap, HeaderValue, IF_NONE_MATCH,
};
use axum::http::{Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
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
    let file = match site.get(path) {
        Some(file) => file,
        None if site::is_static(path) => return not_found(),
        None => site.index(),
    };
    if none_match(&request, &file.etag) {
        return not_modified(file);
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
    describe(fields, file);
    response
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
fn describe(fields: &mut HeaderMap, file: &File) {
    fields.insert(ETAG, file.etag.clone());
    fields.insert(
        CACHE_CONTROL,
        HeaderValue::from_static(match file.cache {
            CachePolicy::Revalidate => headers::CACHE_REVALIDATE,
            CachePolicy::Immutable => headers::CACHE_IMMUTABLE,
        }),
    );
    headers::secure(fields);
}

fn not_modified(file: &File) -> Response {
    let mut response = StatusCode::NOT_MODIFIED.into_response();
    describe(response.headers_mut(), file);
    response
}

fn not_found() -> Response {
    error(StatusCode::NOT_FOUND, "not found\n")
}

fn method_not_allowed() -> Response {
    let mut response = error(StatusCode::METHOD_NOT_ALLOWED, "method not allowed\n");
    response
        .headers_mut()
        .insert(ALLOW, HeaderValue::from_static("GET, HEAD"));
    response
}

fn error(status: StatusCode, text: &'static str) -> Response {
    let mut response = (status, text).into_response();
    let fields = response.headers_mut();
    fields.insert(CACHE_CONTROL, HeaderValue::from_static(headers::CACHE_NONE));
    headers::secure(fields);
    response
}
