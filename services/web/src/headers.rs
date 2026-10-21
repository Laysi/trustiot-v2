//! The security headers on every response of the service (Laysi/trustiot-v2#123; checklist L2),
//! and the cache policies. The values are pinned by tests/serve.rs; change them only together
//! with the README's table.

use axum::http::header::{
    CONTENT_SECURITY_POLICY, CONTENT_SECURITY_POLICY_REPORT_ONLY, HeaderMap, HeaderName,
    HeaderValue, REFERRER_POLICY, STRICT_TRANSPORT_SECURITY, X_CONTENT_TYPE_OPTIONS,
    X_FRAME_OPTIONS,
};

/// Only the site's own files run, load and connect: no inline script or style, no `eval`, no
/// `data:` URIs (frontend/tools/check-dist.ts keeps the build free of them), no framing, no
/// plugins, and forms post only to the same origin. `connect-src 'self'` covers the API and its
/// event streams, which are on the same host (decision 2).
pub const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self'; \
                       font-src 'self'; connect-src 'self'; manifest-src 'self'; \
                       worker-src 'self'; base-uri 'none'; form-action 'self'; \
                       frame-ancestors 'none'; object-src 'none'";

/// Trusted Types, reported only (the browser's console) until the first real pages have been
/// checked in a browser; then it moves into [`CSP`].
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

/// `Cache-Control` of a file whose name stays the same across builds (`index.html`, deep links).
pub const CACHE_REVALIDATE: &str = "no-cache";

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

/// Sets every security header on `headers`, replacing any value already there.
pub fn secure(headers: &mut HeaderMap) {
    for (name, value) in SECURITY_HEADERS {
        headers.insert(name, HeaderValue::from_static(value));
    }
}
