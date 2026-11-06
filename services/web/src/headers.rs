//! The security headers on every response of the service (Laysi/trustiot-v2#123; checklist L2),
//! and the cache policies. The values are pinned by tests/serve.rs; change them only together
//! with the README's table.
//!
//! The SPA's CSP has one part that depends on the request: `frame-src`, which allows only the
//! dashboard sandbox's documents on the host the browser asked (Laysi/trustiot-v2#997;
//! [`frame_src`]). The sandbox's own files get the headers `crates/contract` defines for them
//! instead ([`secure_sandbox`]).

use axum::http::header::{
    CONTENT_SECURITY_POLICY, CONTENT_SECURITY_POLICY_REPORT_ONLY, HeaderMap, HeaderName,
    HeaderValue, REFERRER_POLICY, STRICT_TRANSPORT_SECURITY, X_CONTENT_TYPE_OPTIONS,
    X_FRAME_OPTIONS,
};
use contract::api::dashboard::RuntimeVersion;
use contract::api::dashboard::sandbox::SandboxFile;

/// Only the site's own files run, load and connect: no inline script or style, no `eval`, no
/// `data:` URIs (frontend/tools/check-dist.ts keeps the build free of them), no framing, no
/// plugins, and forms post only to the same origin. `connect-src 'self'` covers the API and its
/// event streams, which are on the same host (decision 2). No frames either (`default-src`),
/// except what [`frame_src`] adds for the request's host.
pub const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self'; \
                       font-src 'self'; connect-src 'self'; manifest-src 'self'; \
                       worker-src 'self'; base-uri 'none'; form-action 'self'; \
                       frame-ancestors 'none'; object-src 'none'";

/// Trusted Types, reported only (the browser's console) until the first real pages have been
/// checked in a browser, the AI reply's Markdown rendering among them (frontend/README.md,
/// 「CSP 的限制」); then it moves into [`CSP`].
pub const CSP_REPORT_ONLY: &str = "require-trusted-types-for 'script'; trusted-types 'none'";

/// Not `no-referrer`: with it browsers send `Origin: null` on every request that is not GET or
/// HEAD, including the SPA's own, and the identity service's cross-site check would refuse them
/// (docs/architecture/secrets.md, "不放網址").
pub const REFERRER: &str = "strict-origin-when-cross-origin";

/// The browser features the SPA does not use.
pub const PERMISSIONS: &str = "camera=(), microphone=(), geolocation=(), payment=(), usb=()";

/// HTTPS only, for a year. Without `includeSubDomains` and `preload`: on premises the domain is
/// the customer's.
pub const HSTS: &str = "max-age=31536000";

/// `Cache-Control` of a file whose name stays the same across builds and is not HTML (the icon,
/// `robots.txt`, the sandbox's scripts and stylesheet).
pub const CACHE_REVALIDATE: &str = "no-cache";

/// `Cache-Control` of an HTML file whose name stays the same across builds (`index.html`, deep
/// links, the sandbox document). `no-transform` keeps Cloudflare's JavaScript detections from
/// injecting an inline script into the HTML, which the CSP refuses and which also drops the ETag
/// (Laysi/trustiot-v2#988; https://developers.cloudflare.com/cloudflare-challenges/challenge-types/javascript-detections/).
/// Only HTML gets it: `no-transform` also stops Cloudflare from compressing the response
/// (https://developers.cloudflare.com/speed/optimization/content/compression/).
pub const CACHE_REVALIDATE_HTML: &str = "no-cache, no-transform";

/// `Cache-Control` of a hashed asset under `/static/`.
pub const CACHE_IMMUTABLE: &str = "public, max-age=31536000, immutable";

/// `Cache-Control` of an error (404, 405).
pub const CACHE_NONE: &str = "no-store";

const PERMISSIONS_POLICY: HeaderName = HeaderName::from_static("permissions-policy");
const CROSS_ORIGIN_OPENER_POLICY: HeaderName =
    HeaderName::from_static("cross-origin-opener-policy");
const CROSS_ORIGIN_RESOURCE_POLICY: HeaderName =
    HeaderName::from_static("cross-origin-resource-policy");

/// Every security header and its value, in the order they are written.
pub const SECURITY_HEADERS: [(HeaderName, &str); 9] = [
    (CONTENT_SECURITY_POLICY, CSP),
    (CONTENT_SECURITY_POLICY_REPORT_ONLY, CSP_REPORT_ONLY),
    (X_CONTENT_TYPE_OPTIONS, "nosniff"),
    (REFERRER_POLICY, REFERRER),
    (PERMISSIONS_POLICY, PERMISSIONS),
    (CROSS_ORIGIN_OPENER_POLICY, "same-origin"),
    (CROSS_ORIGIN_RESOURCE_POLICY, "same-origin"),
    (X_FRAME_OPTIONS, "DENY"),
    (STRICT_TRANSPORT_SECURITY, HSTS),
];

/// The `frame-src` directive the SPA's CSP gets for a request to `host` (the `Host` header): the
/// sandbox document of every runtime the build ships, on that host, and nothing else, so a
/// dashboard's custom page can be shown in its frame and the frame cannot be navigated anywhere
/// else (`frontend/sandbox/README.md`, 「顯示時比對的強制」). A source with a path matches that
/// exact path; the scheme is the page's own (CSP upgrades `http` to `https`, never the reverse).
///
/// `None` when `host` is not a plain host name or IPv4 address with an optional port (CSP has no
/// syntax for an IPv6 literal, and anything else could change the directive): the CSP then has no
/// `frame-src`, `default-src 'none'` applies, and no frame loads. The value comes from the
/// request, so it only shapes the CSP of the response to that request.
#[must_use]
pub fn frame_src(host: &str) -> Option<String> {
    if !csp_host(host) {
        return None;
    }
    let sources: Vec<String> = RuntimeVersion::SHIPPED
        .iter()
        .map(|runtime| format!("{host}{}", SandboxFile::Document.path(*runtime)))
        .collect();
    Some(format!("frame-src {}", sources.join(" ")))
}

/// Whether `host` is `name[:port]` as a CSP host-source spells it: dot-separated labels of ASCII
/// letters, digits and `-`, then an optional port of 1 to 5 digits; at most 255 bytes.
fn csp_host(host: &str) -> bool {
    if host.is_empty() || host.len() > 255 {
        return false;
    }
    let (name, port) = match host.split_once(':') {
        Some((name, port)) => (name, Some(port)),
        None => (host, None),
    };
    let labels_ok = name.split('.').all(|label| {
        !label.is_empty()
            && label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    });
    let port_ok = port.is_none_or(|port| {
        (1..=5).contains(&port.len()) && port.bytes().all(|byte| byte.is_ascii_digit())
    });
    labels_ok && port_ok
}

/// The SPA's CSP for a request to `host`: [`CSP`], with [`frame_src`] when the host is usable.
#[must_use]
pub fn csp(host: Option<&str>) -> HeaderValue {
    match host.and_then(frame_src) {
        Some(frames) => HeaderValue::from_str(&format!("{CSP}; {frames}"))
            .unwrap_or_else(|_| HeaderValue::from_static(CSP)),
        None => HeaderValue::from_static(CSP),
    }
}

/// Sets every security header on `headers`, replacing any value already there; `host` is the
/// request's (see [`csp`]).
pub fn secure(headers: &mut HeaderMap, host: Option<&str>) {
    for (name, value) in SECURITY_HEADERS {
        headers.insert(name, HeaderValue::from_static(value));
    }
    headers.insert(CONTENT_SECURITY_POLICY, csp(host));
}

/// Sets the headers of a sandbox file (`crates/contract`, `SandboxFile::headers`) and HSTS on
/// `headers`. None of the SPA's: its `frame-ancestors 'none'`, `X-Frame-Options: DENY` and
/// `Cross-Origin-Resource-Policy: same-origin` would forbid the frame and its scripts.
pub fn secure_sandbox(headers: &mut HeaderMap, file: SandboxFile) {
    for (name, value) in file.headers() {
        headers.insert(name.clone(), HeaderValue::from_static(value));
    }
    headers.insert(STRICT_TRANSPORT_SECURITY, HeaderValue::from_static(HSTS));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_the_csp_can_name() {
        for host in [
            "app.example.test",
            "app.example.test:8443",
            "localhost",
            "localhost:3000",
            "192.0.2.10",
            "192.0.2.10:8080",
            "a-b.example",
        ] {
            assert!(csp_host(host), "{host}");
        }
        for host in [
            "",
            "app.example.test.",
            ".example.test",
            "a..b",
            "app.example.test:",
            "app.example.test:123456",
            "app.example.test:80:80",
            "app.example.test:8a",
            "[2001:db8::1]",
            "[2001:db8::1]:443",
            "app.example.test/x",
            "app.example.test; script-src *",
            "app example",
            "*.example.test",
            "user@app.example.test",
            "app_example.test",
            "app.example.test\r\n",
        ] {
            assert!(!csp_host(host), "{host:?}");
        }
        assert!(!csp_host(&"a".repeat(256)));
    }

    #[test]
    fn the_frame_source_is_the_sandbox_document_on_the_host() {
        assert_eq!(
            frame_src("app.example.test:8443").as_deref(),
            Some("frame-src app.example.test:8443/sandbox/v1/sandbox.html")
        );
        assert_eq!(frame_src("[::1]"), None);
        assert_eq!(csp(None), HeaderValue::from_static(CSP));
        assert_eq!(csp(Some("bad host")), HeaderValue::from_static(CSP));
        assert_eq!(
            csp(Some("app.example.test")),
            HeaderValue::from_str(&format!(
                "{CSP}; frame-src app.example.test/sandbox/v1/sandbox.html"
            ))
            .expect("a header value")
        );
    }
}
