//! The sandbox's files and response headers, defined once: `dashboard-check` serves them to its
//! browser now, and `services/web` will serve the same files with the same headers to users
//! (Laysi/trustiot-v2#462). The files come from the frontend's sandbox build
//! (`frontend/sandbox/README.md`, 「建置結果」), under `dist/` at the paths below.
//!
//! # How page scripts run
//!
//! The runtime runs each of the page's scripts with `new Function("tq", "d3", source)`, which
//! the sandbox document's CSP allows with `'unsafe-eval'`; no other document gets it. The
//! alternatives fail one of the requirements:
//!
//! - `'unsafe-inline'` with script elements: it also runs inline event handlers and
//!   `javascript:` URLs, outside the runtime's order, error capture and arguments.
//! - A nonce: the document is one static file shared by every dashboard and cached; a nonce
//!   needs a different header per response.
//! - Hashes: every page has different scripts, and the header is fixed per runtime version.
//!
//! Evaluating the page's text gives it no capability it lacks: the page is already arbitrary
//! script, and the document may load and connect to nothing (`default-src 'none'`, every fetch
//! directive `'none'`), has an opaque origin (`sandbox allow-scripts` without
//! `allow-same-origin`), and cannot open windows, submit forms or navigate the top-level page.
//! Where the sandbox's own frame may navigate is decided by the embedding page's CSP
//! (`frame-src`), not by these headers: [`CHECK_HOST_CSP`] for the checker, and the SPA's CSP
//! at display time (Laysi/trustiot-v2#462), which must allow no frame source but the sandbox.
//!
//! `'unsafe-eval'` also allows `eval` and compiling WebAssembly; neither adds a capability, as
//! both only compute inside the same document.
//!
//! `script-src 'self'` and `style-src 'self'` let the document load the runtime's files from
//! the host that serves them, and so they also let a page script request any path of that
//! host as a script or stylesheet. Such a request reaches only the platform's own host, and it
//! goes without the `SameSite=Lax` session cookie: the document's origin is opaque, so the
//! browser treats it as a request from another site and leaves that cookie out (both checked
//! in Chromium on 2026-10-26). That is all that was checked: a cookie of that host set with
//! `SameSite=None` would go with the request. The checker refuses every such request and
//! fails the page with
//! `check::Problem::Requested` (`frontend/sandbox/README.md`, 「檢查規則」 and 「檢查服務的流程」;
//! `services/dashboard-check/README.md`, 「流程」). A page could still make such a request
//! only when it is displayed, which is why what the request can reach matters.
//!
//! What these headers do not cover is tracked in Laysi/trustiot-v2#699.

use http::HeaderName;
use http::header::{CONTENT_SECURITY_POLICY, REFERRER_POLICY, X_CONTENT_TYPE_OPTIONS};

use super::RuntimeVersion;

/// The sandbox document's CSP.
///
/// - `sandbox allow-scripts`: the document is sandboxed even when opened directly, not only
///   inside the host's `<iframe sandbox="allow-scripts">`; its origin is opaque.
/// - `script-src 'self' 'unsafe-eval'`: the runtime and d3 from the same host (allowed although
///   the document's origin is opaque), and the page's scripts through the runtime (see the
///   module documentation).
/// - `style-src 'self' 'unsafe-inline'`: the runtime's stylesheet, and the page's `<style>`
///   elements and `style` attributes.
/// - every other fetch directive `'none'`: no images, fonts, media, frames, workers or
///   connections; `form-action` and `base-uri` `'none'`.
/// - `frame-ancestors 'self'`: only a page of the same host may embed it: the SPA, or the
///   checker's host page.
pub const SANDBOX_CSP: &str = "sandbox allow-scripts; default-src 'none'; \
                               script-src 'self' 'unsafe-eval'; \
                               style-src 'self' 'unsafe-inline'; img-src 'none'; \
                               font-src 'none'; connect-src 'none'; media-src 'none'; \
                               frame-src 'none'; child-src 'none'; worker-src 'none'; \
                               manifest-src 'none'; object-src 'none'; form-action 'none'; \
                               base-uri 'none'; frame-ancestors 'self'";

/// The checker's host page's CSP: its own script and style, and frames of the same host (the
/// sandbox document); nothing else, and nothing may embed it. The checker also refuses every
/// request for a file it does not serve.
pub const CHECK_HOST_CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; \
                                  frame-src 'self'; base-uri 'none'; form-action 'none'; \
                                  frame-ancestors 'none'";

/// The browser features the sandbox never uses.
pub const SANDBOX_PERMISSIONS: &str = "accelerometer=(), autoplay=(), camera=(), \
                                       display-capture=(), encrypted-media=(), fullscreen=(), \
                                       geolocation=(), gyroscope=(), magnetometer=(), \
                                       microphone=(), midi=(), payment=(), \
                                       picture-in-picture=(), publickey-credentials-get=(), \
                                       screen-wake-lock=(), serial=(), usb=(), \
                                       xr-spatial-tracking=()";

const PERMISSIONS_POLICY: HeaderName = HeaderName::from_static("permissions-policy");
const CROSS_ORIGIN_RESOURCE_POLICY: HeaderName =
    HeaderName::from_static("cross-origin-resource-policy");

/// `Cross-Origin-Resource-Policy` of the sandbox's files: `cross-origin`, because the sandbox
/// document's origin is opaque, so even the same host's scripts are cross-origin to it, and
/// `same-origin` (`services/web`'s value for the SPA) blocks them.
pub const SANDBOX_CORP: &str = "cross-origin";

/// The headers of the sandbox document, besides `Content-Type: text/html; charset=utf-8` and
/// the server's cache policy. No `X-Frame-Options`: `frame-ancestors` decides who embeds it.
/// They replace the SPA's document headers rather than add to them: a browser enforces every
/// CSP header it gets, and the SPA's `frame-ancestors 'none'` would forbid the frame.
pub static SANDBOX_DOCUMENT_HEADERS: [(HeaderName, &str); 5] = [
    (CONTENT_SECURITY_POLICY, SANDBOX_CSP),
    (X_CONTENT_TYPE_OPTIONS, "nosniff"),
    (REFERRER_POLICY, "no-referrer"),
    (PERMISSIONS_POLICY, SANDBOX_PERMISSIONS),
    (CROSS_ORIGIN_RESOURCE_POLICY, SANDBOX_CORP),
];

/// The headers of the sandbox's scripts and stylesheet, besides their `Content-Type`
/// (`text/javascript; charset=utf-8`, `text/css; charset=utf-8`). No CORS headers: the
/// runtime loads them without `crossorigin`, so the browser hides the text of a page script's
/// later errors from the page (`Script error.`); the checker reads them through the DevTools
/// protocol instead.
pub static SANDBOX_ASSET_HEADERS: [(HeaderName, &str); 2] = [
    (X_CONTENT_TYPE_OPTIONS, "nosniff"),
    (CROSS_ORIGIN_RESOURCE_POLICY, SANDBOX_CORP),
];

/// The headers of the checker's host page and its script, besides `Content-Type`.
pub static CHECK_HOST_HEADERS: [(HeaderName, &str); 3] = [
    (CONTENT_SECURITY_POLICY, CHECK_HOST_CSP),
    (X_CONTENT_TYPE_OPTIONS, "nosniff"),
    (REFERRER_POLICY, "no-referrer"),
];

/// The checker's host page: unversioned, it loads the sandbox document of the request's
/// runtime version.
pub const CHECK_HOST_PATH: &str = "/sandbox/check.html";

/// The host page's script: the host adapter and the checker's driver side.
pub const CHECK_HOST_SCRIPT_PATH: &str = "/sandbox/check.js";

/// The files of one runtime version, under `/sandbox/v<N>/`. A version's files are frozen:
/// a dashboard checked with one keeps running with it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SandboxFile {
    /// The sandbox document, with [`SANDBOX_DOCUMENT_HEADERS`].
    Document,
    /// The runtime (`window.tq`, the formula evaluator, the postMessage side).
    Runtime,
    /// d3, the version the runtime was built with.
    D3,
    /// The runtime's stylesheet.
    Style,
    /// The comparison: injected by the checker into an isolated world, and loaded by the
    /// runtime at display time (Laysi/trustiot-v2#462).
    Compare,
}

impl SandboxFile {
    /// Every file of a version.
    pub const ALL: [Self; 5] = [
        Self::Document,
        Self::Runtime,
        Self::D3,
        Self::Style,
        Self::Compare,
    ];

    /// The file's name in its version's directory.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Document => "sandbox.html",
            Self::Runtime => "runtime.js",
            Self::D3 => "d3.js",
            Self::Style => "sandbox.css",
            Self::Compare => "compare.js",
        }
    }

    /// The `Content-Type` to serve it with.
    #[must_use]
    pub const fn content_type(self) -> &'static str {
        match self {
            Self::Document => "text/html; charset=utf-8",
            Self::Runtime | Self::D3 | Self::Compare => "text/javascript; charset=utf-8",
            Self::Style => "text/css; charset=utf-8",
        }
    }

    /// The headers to serve it with, besides `Content-Type` and caching.
    #[must_use]
    pub fn headers(self) -> &'static [(HeaderName, &'static str)] {
        match self {
            Self::Document => &SANDBOX_DOCUMENT_HEADERS,
            Self::Runtime | Self::D3 | Self::Style | Self::Compare => &SANDBOX_ASSET_HEADERS,
        }
    }

    /// The URL path of this file for `runtime`, such as `/sandbox/v1/sandbox.html`; the build
    /// writes it to `frontend/dist` at the same path.
    #[must_use]
    pub fn path(self, runtime: RuntimeVersion) -> String {
        format!("/sandbox/v{}/{}", runtime.get(), self.name())
    }
}

#[cfg(test)]
mod tests {
    use http::HeaderValue;

    use super::*;

    #[test]
    fn header_values_are_valid() {
        for (name, value) in SANDBOX_DOCUMENT_HEADERS
            .iter()
            .chain(&SANDBOX_ASSET_HEADERS)
            .chain(&CHECK_HOST_HEADERS)
        {
            assert!(HeaderValue::from_str(value).is_ok(), "{name}");
        }
    }

    #[test]
    fn the_sandbox_policy_allows_only_what_the_runtime_needs() {
        let directives: Vec<&str> = SANDBOX_CSP.split("; ").collect();
        assert_eq!(directives[0], "sandbox allow-scripts");
        assert!(directives.contains(&"default-src 'none'"));
        assert!(directives.contains(&"script-src 'self' 'unsafe-eval'"));
        assert!(directives.contains(&"frame-ancestors 'self'"));
        for directive in [
            "img-src",
            "font-src",
            "connect-src",
            "media-src",
            "frame-src",
            "child-src",
            "worker-src",
            "manifest-src",
            "object-src",
            "form-action",
            "base-uri",
        ] {
            assert!(
                directives.contains(&format!("{directive} 'none'").as_str()),
                "{directive}"
            );
        }
        assert!(!SANDBOX_CSP.contains("allow-same-origin"));
        assert!(!SANDBOX_CSP.contains("wasm-unsafe-eval"));
        // 'unsafe-inline' only for styles: never for scripts.
        assert_eq!(
            directives
                .iter()
                .filter(|directive| directive.contains("'unsafe-inline'"))
                .collect::<Vec<_>>(),
            vec![&"style-src 'self' 'unsafe-inline'"]
        );
        assert!(CHECK_HOST_CSP.contains("frame-ancestors 'none'"));
        assert!(!CHECK_HOST_CSP.contains("unsafe"));
    }

    #[test]
    fn files_have_versioned_paths() {
        assert_eq!(
            SandboxFile::Document.path(RuntimeVersion::V1),
            "/sandbox/v1/sandbox.html"
        );
        assert_eq!(
            SandboxFile::Compare.path(RuntimeVersion::V1),
            "/sandbox/v1/compare.js"
        );
        assert_eq!(SandboxFile::Document.headers().len(), 5);
        assert!(SandboxFile::ALL.iter().all(|file| {
            file.headers()
                .iter()
                .any(|(_, value)| *value == SANDBOX_CORP)
        }));
    }
}
