//! What the web service answers (services/web/README.md, "回應"): the router on the fixture site
//! in `tests/fixtures/site`, without a socket, and how loading a site fails. The real binary
//! behind service-kit is in `tests/process.rs`.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode};
use contract::api::dashboard::RuntimeVersion;
use contract::api::dashboard::sandbox::{SANDBOX_CORP, SANDBOX_CSP, SandboxFile};
use tower::ServiceExt;
use web::headers::{CSP, CSP_REPORT_ONLY, HSTS, PERMISSIONS, REFERRER};
use web::site::{CONTENT_TYPES, MAX_SITE_BYTES, Site, SiteError};

const BODY_LIMIT: usize = 1024 * 1024;

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/site")
}

fn fixture_file(path: &str) -> Vec<u8> {
    fs::read(fixture().join(path)).expect("read a fixture file")
}

struct Answer {
    status: StatusCode,
    headers: HeaderMap,
    body: Vec<u8>,
}

impl Answer {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(name)
            .map(|value| value.to_str().expect("an ASCII header"))
    }
}

async fn request(method: Method, target: &str, headers: &[(&str, &str)]) -> Answer {
    let site = Site::load(&fixture(), MAX_SITE_BYTES).expect("load the fixture site");
    let mut builder = Request::builder().method(method).uri(target);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let response = web::routes(Arc::new(site))
        .oneshot(builder.body(Body::empty()).expect("a request"))
        .await
        .expect("the router is infallible");
    let status = response.status();
    let headers = response.headers().clone();
    let body = axum::body::to_bytes(response.into_body(), BODY_LIMIT)
        .await
        .expect("read the body")
        .to_vec();
    Answer {
        status,
        headers,
        body,
    }
}

async fn get(target: &str) -> Answer {
    request(Method::GET, target, &[]).await
}

/// The security headers with their exact values (README, "安全標頭"), for a request without a
/// `Host` header: the CSP has no `frame-src`.
fn assert_security_headers(answer: &Answer, what: &str) {
    assert_security_headers_with(answer, what, CSP);
}

/// The security headers with their exact values and the CSP `csp`.
fn assert_security_headers_with(answer: &Answer, what: &str, csp: &str) {
    for (name, value) in [
        ("content-security-policy", csp),
        ("content-security-policy-report-only", CSP_REPORT_ONLY),
        ("x-content-type-options", "nosniff"),
        ("referrer-policy", REFERRER),
        ("permissions-policy", PERMISSIONS),
        ("cross-origin-opener-policy", "same-origin"),
        ("cross-origin-resource-policy", "same-origin"),
        ("x-frame-options", "DENY"),
        ("strict-transport-security", HSTS),
    ] {
        assert_eq!(answer.header(name), Some(value), "{name} on {what}");
        assert_eq!(
            answer.headers.get_all(name).iter().count(),
            1,
            "{name} on {what}"
        );
    }
}

#[test]
fn the_header_values_are_the_documented_ones() {
    assert_eq!(
        CSP,
        "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self'; \
         font-src 'self'; connect-src 'self'; manifest-src 'self'; worker-src 'self'; \
         base-uri 'none'; form-action 'self'; frame-ancestors 'none'; object-src 'none'"
    );
    assert_eq!(
        CSP_REPORT_ONLY,
        "require-trusted-types-for 'script'; trusted-types 'none'"
    );
    // `no-referrer` would make browsers send `Origin: null` on the SPA's own POSTs.
    assert_eq!(REFERRER, "strict-origin-when-cross-origin");
    assert_eq!(
        PERMISSIONS,
        "camera=(), microphone=(), geolocation=(), payment=(), usb=()"
    );
    assert_eq!(HSTS, "max-age=31536000");
}

#[tokio::test]
async fn deep_links_serve_index_html() {
    let index = fixture_file("index.html");
    for target in [
        "/",
        "/index.html",
        "/devices/42",
        "/devices/42/",
        "/settings/profile?tab=language",
        "/assets/sites",
        "/staticx",
        "/static.js",
        "/a/static/b.js",
        "/favicon.ico",
    ] {
        let answer = get(target).await;
        assert_eq!(answer.status, StatusCode::OK, "{target}");
        assert_eq!(answer.body, index, "{target}");
        assert_eq!(
            answer.header("content-type"),
            Some("text/html; charset=utf-8"),
            "{target}"
        );
        assert_eq!(
            answer.header("cache-control"),
            Some("no-cache, no-transform"),
            "{target}"
        );
        assert!(answer.header("etag").is_some(), "{target}");
        assert_security_headers(&answer, target);
    }
}

#[tokio::test]
async fn files_are_served_with_their_type_and_cache_policy() {
    for (target, file, content_type, cache) in [
        (
            "/static/B7f3kQ2x.js",
            "static/B7f3kQ2x.js",
            "text/javascript; charset=utf-8",
            "public, max-age=31536000, immutable",
        ),
        (
            "/static/Cq9_Lm-1.css",
            "static/Cq9_Lm-1.css",
            "text/css; charset=utf-8",
            "public, max-age=31536000, immutable",
        ),
        ("/favicon.svg", "favicon.svg", "image/svg+xml", "no-cache"),
        (
            "/robots.txt",
            "robots.txt",
            "text/plain; charset=utf-8",
            "no-cache",
        ),
        // The query string never selects another file.
        (
            "/static/B7f3kQ2x.js?v=2",
            "static/B7f3kQ2x.js",
            "text/javascript; charset=utf-8",
            "public, max-age=31536000, immutable",
        ),
    ] {
        let answer = get(target).await;
        assert_eq!(answer.status, StatusCode::OK, "{target}");
        assert_eq!(answer.body, fixture_file(file), "{target}");
        assert_eq!(
            answer.header("content-type"),
            Some(content_type),
            "{target}"
        );
        assert_eq!(answer.header("cache-control"), Some(cache), "{target}");
        let length = answer.body.len().to_string();
        assert_eq!(answer.header("content-length"), Some(length.as_str()));
        assert_security_headers(&answer, target);
    }
}

#[tokio::test]
async fn missing_static_files_are_404_not_index_html() {
    for target in [
        "/static",
        "/static/",
        "/static/gone.js",
        "/static/B7f3kQ2x.js/",
        "/static/../index.html",
        "/static/%2e%2e/index.html",
        "/static/b7f3kq2x.js",
    ] {
        let answer = get(target).await;
        assert_eq!(answer.status, StatusCode::NOT_FOUND, "{target}");
        assert_eq!(answer.body, b"not found\n", "{target}");
        assert_eq!(answer.header("cache-control"), Some("no-store"), "{target}");
        assert_eq!(
            answer.header("content-type"),
            Some("text/plain; charset=utf-8"),
            "{target}"
        );
        assert_security_headers(&answer, target);
    }
}

#[tokio::test]
async fn head_answers_the_get_headers_without_a_body() {
    for target in ["/", "/devices/42", "/static/B7f3kQ2x.js"] {
        let get = get(target).await;
        let head = request(Method::HEAD, target, &[]).await;
        assert_eq!(head.status, StatusCode::OK, "{target}");
        assert!(head.body.is_empty(), "{target}");
        for name in ["content-type", "content-length", "etag", "cache-control"] {
            assert_eq!(head.header(name), get.header(name), "{name} of {target}");
        }
        assert_security_headers(&head, target);
    }
}

#[tokio::test]
async fn a_matching_etag_answers_304() {
    for target in ["/devices/42", "/static/B7f3kQ2x.js"] {
        let first = get(target).await;
        let etag = first.header("etag").expect("an ETag").to_owned();
        assert!(
            etag.len() == 34 && etag.starts_with('"') && etag.ends_with('"'),
            "{etag}"
        );
        let weak = format!("W/{etag}");
        let listed = format!("\"0000\", {etag}");
        for value in [etag.as_str(), weak.as_str(), listed.as_str(), "*"] {
            let answer = request(Method::GET, target, &[("if-none-match", value)]).await;
            assert_eq!(
                answer.status,
                StatusCode::NOT_MODIFIED,
                "{value} on {target}"
            );
            assert!(answer.body.is_empty());
            assert_eq!(answer.header("etag"), Some(etag.as_str()));
            assert_eq!(
                answer.header("cache-control"),
                first.header("cache-control")
            );
            assert_security_headers(&answer, "a 304");
        }
        let changed = request(Method::GET, target, &[("if-none-match", "\"0000\"")]).await;
        assert_eq!(changed.status, StatusCode::OK, "{target}");
        assert_eq!(changed.body, first.body);
    }
    // Every deep link is index.html, so it has index.html's ETag.
    assert_eq!(
        get("/devices/42").await.header("etag"),
        get("/index.html").await.header("etag")
    );
    assert_ne!(
        get("/").await.header("etag"),
        get("/static/B7f3kQ2x.js").await.header("etag")
    );
}

#[tokio::test]
async fn methods_other_than_get_and_head_are_405() {
    for method in [
        Method::POST,
        Method::PUT,
        Method::PATCH,
        Method::DELETE,
        Method::OPTIONS,
        Method::TRACE,
        Method::CONNECT,
    ] {
        for target in ["/", "/devices/42", "/static/B7f3kQ2x.js", "/static/gone.js"] {
            let answer = request(method.clone(), target, &[]).await;
            assert_eq!(
                answer.status,
                StatusCode::METHOD_NOT_ALLOWED,
                "{method} {target}"
            );
            assert_eq!(answer.header("allow"), Some("GET, HEAD"));
            assert_eq!(answer.header("cache-control"), Some("no-store"));
            assert_eq!(answer.body, b"method not allowed\n");
            assert_security_headers(&answer, "a 405");
        }
    }
}

/// A fresh, empty directory under the system's temporary directory, removed on drop.
struct TempSite(PathBuf);

impl TempSite {
    fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "trustiot-web-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("create a temporary site");
        Self(path)
    }

    fn write(&self, path: &str, bytes: &[u8]) -> &Self {
        let path = self.0.join(path);
        fs::create_dir_all(path.parent().expect("a parent")).expect("create directories");
        fs::write(path, bytes).expect("write a file");
        self
    }

    fn load(&self) -> Result<Site, SiteError> {
        Site::load(&self.0, MAX_SITE_BYTES)
    }
}

impl Drop for TempSite {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn a_site_loads_every_known_type() {
    let site = TempSite::new();
    site.write("index.html", b"<!doctype html>");
    for (extension, _) in CONTENT_TYPES {
        site.write(&format!("static/a1.{extension}"), b"x");
    }
    let loaded = site.load().expect("a valid site");
    assert_eq!(loaded.len(), CONTENT_TYPES.len() + 1);
    assert_eq!(
        loaded.bytes(),
        u64::try_from(15 + CONTENT_TYPES.len()).expect("small")
    );
}

#[test]
fn a_site_without_index_html_is_refused() {
    let site = TempSite::new();
    site.write("static/a.js", b"x").write("app.html", b"x");
    assert!(matches!(site.load(), Err(SiteError::MissingIndex)));
    // Not at the root does not count.
    site.write("nested/index.html", b"x");
    assert!(matches!(site.load(), Err(SiteError::MissingIndex)));
}

#[test]
fn unknown_extensions_hidden_files_and_odd_names_are_refused() {
    for (path, expected) in [
        ("static/a.js.map", "/static/a.js.map has an extension"),
        ("static/a.json", "/static/a.json has an extension"),
        ("Makefile", "/Makefile has an extension"),
        (".DS_Store", "/.DS_Store has a name"),
        ("static/.hidden.js", "/static/.hidden.js has a name"),
        ("static/a b.js", "/static/a b.js has a name"),
        (".well-known/x.txt", "/.well-known has a name"),
    ] {
        let site = TempSite::new();
        site.write("index.html", b"x").write(path, b"x");
        let error = site.load().expect_err(path).to_string();
        assert!(error.contains(expected), "{path}: {error}");
        assert!(!error.contains(&site.0.display().to_string()), "{error}");
    }
}

#[cfg(unix)]
#[test]
fn symbolic_links_are_refused() {
    let outside = TempSite::new();
    outside.write("secret.txt", b"not part of the site");
    for link in ["static/a.txt", "linked"] {
        let site = TempSite::new();
        site.write("index.html", b"x");
        let path = site.0.join(link);
        fs::create_dir_all(path.parent().expect("a parent")).expect("create directories");
        let target = if link == "linked" {
            outside.0.clone()
        } else {
            outside.0.join("secret.txt")
        };
        std::os::unix::fs::symlink(target, &path).expect("create a symbolic link");
        let error = site.load().expect_err(link).to_string();
        assert!(error.contains("is a symbolic link"), "{link}: {error}");
    }
}

#[test]
fn a_site_over_the_limit_is_refused() {
    let site = TempSite::new();
    site.write("index.html", &[b'x'; 10])
        .write("static/a.js", &[b'x'; 10]);
    assert!(Site::load(&site.0, 20).is_ok());
    assert!(matches!(
        Site::load(&site.0, 19),
        Err(SiteError::TooLarge { limit: 19 })
    ));
}

#[test]
fn a_missing_root_is_refused_without_naming_it() {
    let site = TempSite::new();
    let missing = site.0.join("missing");
    let error = Site::load(&missing, MAX_SITE_BYTES)
        .expect_err("no directory")
        .to_string();
    assert!(error.contains("root directory"), "{error}");
    assert!(!error.contains(&missing.display().to_string()), "{error}");
}

/// Every response header's name, sorted.
fn header_names(answer: &Answer) -> Vec<String> {
    let mut names: Vec<String> = answer
        .headers
        .keys()
        .map(|name| name.as_str().to_owned())
        .collect();
    names.sort();
    names
}

#[tokio::test]
async fn the_spa_csp_allows_only_the_sandbox_document_of_the_requested_host_as_a_frame() {
    for (host, csp) in [
        (
            "app.example.test",
            format!("{CSP}; frame-src app.example.test/sandbox/v1/sandbox.html"),
        ),
        (
            "app.example.test:8443",
            format!("{CSP}; frame-src app.example.test:8443/sandbox/v1/sandbox.html"),
        ),
        // A host the CSP cannot name safely gets no frame-src: default-src 'none' blocks frames.
        ("[2001:db8::1]:443", CSP.to_owned()),
        ("app.example.test; frame-src *", CSP.to_owned()),
        ("app.example.test/sandbox", CSP.to_owned()),
    ] {
        for target in ["/", "/devices/42", "/static/B7f3kQ2x.js", "/static/gone.js"] {
            let answer = request(Method::GET, target, &[("host", host)]).await;
            assert_security_headers_with(&answer, &format!("{target} on {host}"), &csp);
        }
        // A 304 too.
        let etag = get("/").await.header("etag").expect("an ETag").to_owned();
        let answer = request(
            Method::GET,
            "/",
            &[("host", host), ("if-none-match", etag.as_str())],
        )
        .await;
        assert_eq!(answer.status, StatusCode::NOT_MODIFIED);
        assert_security_headers_with(&answer, "a 304", &csp);
    }
}

#[tokio::test]
async fn sandbox_files_are_served_with_the_sandbox_headers_only() {
    for runtime in RuntimeVersion::SHIPPED {
        for file in SandboxFile::ALL {
            let target = file.path(runtime);
            let answer = request(Method::GET, &target, &[("host", "app.example.test")]).await;
            assert_eq!(answer.status, StatusCode::OK, "{target}");
            assert_eq!(
                answer.body,
                fixture_file(target.trim_start_matches('/')),
                "{target}"
            );
            assert_eq!(
                answer.header("content-type"),
                Some(file.content_type()),
                "{target}"
            );
            // Only the HTML document must not be transformed; the scripts and stylesheet stay
            // compressible by a CDN.
            let cache = if file.content_type().starts_with("text/html") {
                "no-cache, no-transform"
            } else {
                "no-cache"
            };
            assert_eq!(answer.header("cache-control"), Some(cache), "{target}");
            assert_eq!(
                answer.header("cross-origin-resource-policy"),
                Some(SANDBOX_CORP),
                "{target}"
            );
            assert_eq!(
                answer.header("cross-origin-resource-policy"),
                Some("cross-origin")
            );
            assert_eq!(answer.header("x-content-type-options"), Some("nosniff"));
            assert_eq!(answer.header("strict-transport-security"), Some(HSTS));
            for (name, value) in file.headers() {
                assert_eq!(
                    answer.header(name.as_str()),
                    Some(*value),
                    "{name} on {target}"
                );
                assert_eq!(answer.headers.get_all(name).iter().count(), 1, "{name}");
            }
            // None of the SPA's framing or resource headers: they would forbid the frame.
            for name in [
                "x-frame-options",
                "cross-origin-opener-policy",
                "content-security-policy-report-only",
            ] {
                assert!(answer.header(name).is_none(), "{name} on {target}");
            }
            let mut expected: Vec<String> = file
                .headers()
                .iter()
                .map(|(name, _)| name.as_str().to_owned())
                .chain(
                    [
                        "cache-control",
                        "content-length",
                        "content-type",
                        "etag",
                        "strict-transport-security",
                    ]
                    .map(str::to_owned),
                )
                .collect();
            expected.sort();
            assert_eq!(header_names(&answer), expected, "{target}");

            // HEAD and a 304 carry the same headers.
            let head = request(Method::HEAD, &target, &[]).await;
            assert_eq!(head.status, StatusCode::OK);
            assert!(head.body.is_empty());
            assert_eq!(header_names(&head), expected, "HEAD {target}");
            let etag = answer.header("etag").expect("an ETag");
            let cached = request(Method::GET, &target, &[("if-none-match", etag)]).await;
            assert_eq!(cached.status, StatusCode::NOT_MODIFIED);
            for (name, value) in file.headers() {
                assert_eq!(
                    cached.header(name.as_str()),
                    Some(*value),
                    "{name} on a 304"
                );
            }
            assert!(cached.header("x-frame-options").is_none());
        }
    }
    // The document's CSP sandboxes it and lets only the same host embed it.
    let document = get(&SandboxFile::Document.path(RuntimeVersion::V1)).await;
    let policy = document.header("content-security-policy").expect("a CSP");
    assert_eq!(policy, SANDBOX_CSP);
    assert!(policy.starts_with("sandbox allow-scripts;"));
    assert!(policy.contains("frame-ancestors 'self'"));
    assert!(!policy.contains("allow-same-origin"));
}

#[tokio::test]
async fn other_paths_under_sandbox_are_404_with_the_spa_headers() {
    for target in [
        "/sandbox",
        "/sandbox/",
        "/sandbox/check.html",
        "/sandbox/check.js",
        "/sandbox/v1",
        "/sandbox/v1/",
        "/sandbox/v1/other.js",
        "/sandbox/v1/SANDBOX.html",
        "/sandbox/v1/sandbox.html/",
        "/sandbox/v2/sandbox.html",
        "/sandbox/v01/sandbox.html",
        "/sandbox/v1/../../index.html",
        "/sandbox/v1/%73andbox.html",
        "/sandbox/devices/42",
    ] {
        let answer = request(Method::GET, target, &[("host", "app.example.test")]).await;
        assert_eq!(answer.status, StatusCode::NOT_FOUND, "{target}");
        assert_eq!(answer.body, b"not found\n", "{target}");
        assert_eq!(answer.header("cache-control"), Some("no-store"), "{target}");
        assert_security_headers_with(
            &answer,
            target,
            &format!("{CSP}; frame-src app.example.test/sandbox/v1/sandbox.html"),
        );
    }
    // Only exactly /sandbox and below: a path that only starts with the word is a deep link.
    for target in ["/sandboxes", "/sandbox.html"] {
        let answer = get(target).await;
        assert_eq!(answer.status, StatusCode::OK, "{target}");
        assert_eq!(answer.body, fixture_file("index.html"), "{target}");
    }
}

#[test]
fn the_site_and_the_contract_agree_on_the_sandbox_files_types() {
    for file in SandboxFile::ALL {
        let extension = file.name().rsplit_once('.').expect("an extension").1;
        let site_type = CONTENT_TYPES
            .iter()
            .find(|(known, _)| *known == extension)
            .map(|(_, content_type)| *content_type);
        assert_eq!(site_type, Some(file.content_type()), "{}", file.name());
    }
}
