//! The cross-site request check of the ingress gateway's cookie check (decision 27,
//! Laysi/trustiot-v2#123).
//!
//! The SPA and the API share one origin (decision 2), so every request the SPA sends is
//! same-origin. A request that another site makes a browser send can carry the session cookie as
//! well, so [`cross_site`] refuses it before the session is looked up. The gateway forwards
//! `Origin` and `Sec-Fetch-Site` to the check (`includeRequestHeadersInCheck` in
//! `deploy/istio/istiod-values.yaml`):
//!
//! | The request carries | It may go on when |
//! | --- | --- |
//! | `Sec-Fetch-Site` (every current browser) | it has exactly one value, `same-origin`, or for GET and HEAD also `none` (an address the user typed or bookmarked) |
//! | No `Sec-Fetch-Site`, but `Origin`, on a method other than GET and HEAD | it has exactly one value, `https://` and the `Host`'s name and port (explicit, or 443 when absent); `http://` only for `localhost` and `127.0.0.1`, with 80 as the default port |
//! | Neither | always: not a browser, or one too old to send them |
//!
//! `same-site` is cross-site here: another host of the same site, such as `hosts.gateway`, has no
//! business sending requests with the session to `hosts.app`. `Origin: null` and an `Origin` that
//! is not a plain scheme, host and port are cross-site too.

use axum::http::header::{HOST, ORIGIN};
use axum::http::{HeaderMap, HeaderName, Method};
use contract::MessageKey;

use crate::audience::normalize_host;

/// The message key of the 403 for a cross-site request.
pub const CROSS_SITE_REQUEST: MessageKey = MessageKey::new("security.cross_site_request");

/// The Fetch Metadata header a browser sets on every request.
const SEC_FETCH_SITE: HeaderName = HeaderName::from_static("sec-fetch-site");

/// Hosts that may be reached over plain HTTP: the local development environment.
const LOOPBACK_HOSTS: [&str; 2] = ["localhost", "127.0.0.1"];

/// Whether a request is cross-site: the header that showed it (`sec-fetch-site` or `origin`), or
/// `None` when the request may go on to the session check.
#[must_use]
pub fn cross_site(method: &Method, headers: &HeaderMap) -> Option<&'static str> {
    let safe = *method == Method::GET || *method == Method::HEAD;
    match single(headers, &SEC_FETCH_SITE) {
        Single::One(b"same-origin") => return None,
        Single::One(b"none") if safe => return None,
        Single::One(_) | Single::Repeated => return Some("sec-fetch-site"),
        Single::Absent => {}
    }
    if safe {
        return None;
    }
    match single(headers, &ORIGIN) {
        Single::Absent => None,
        Single::One(origin) if same_origin(origin, headers) => None,
        Single::One(_) | Single::Repeated => Some("origin"),
    }
}

/// How many values a header has.
enum Single<'a> {
    Absent,
    One(&'a [u8]),
    Repeated,
}

fn single<'a>(headers: &'a HeaderMap, name: &HeaderName) -> Single<'a> {
    let mut values = headers.get_all(name).into_iter();
    match (values.next(), values.next()) {
        (None, _) => Single::Absent,
        (Some(value), None) => Single::One(value.as_bytes()),
        (Some(_), Some(_)) => Single::Repeated,
    }
}

/// Whether `origin` is the origin of the request's one `Host`.
fn same_origin(origin: &[u8], headers: &HeaderMap) -> bool {
    let Single::One(host) = single(headers, &HOST) else {
        return false;
    };
    let (Ok(origin), Ok(host)) = (std::str::from_utf8(origin), std::str::from_utf8(host)) else {
        return false;
    };
    let (authority, default_port) = if let Some(rest) = origin.strip_prefix("https://") {
        (rest, 443)
    } else if let Some(rest) = origin.strip_prefix("http://") {
        (rest, 80)
    } else {
        return false;
    };
    let (Some(origin), Some(host)) = (
        name_and_port(authority, default_port),
        name_and_port(host, default_port),
    ) else {
        return false;
    };
    if default_port == 80 && !LOOPBACK_HOSTS.contains(&origin.0.as_str()) {
        return false;
    }
    origin == host
}

/// The name (as [`normalize_host`] compares it) and the port of `host[:port]`, with
/// `default_port` when there is none. `None` for anything else: a path, user information, an
/// empty or non-numeric port, an IPv6 literal.
fn name_and_port(authority: &str, default_port: u16) -> Option<(String, u16)> {
    let (name, port) = match authority.rsplit_once(':') {
        Some((name, port)) => {
            if port.is_empty() || !port.bytes().all(|byte| byte.is_ascii_digit()) {
                return None;
            }
            (name, port.parse().ok()?)
        }
        None => (authority, default_port),
    };
    if name.contains(':') {
        return None;
    }
    let valid = |byte: u8| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-');
    if !name.bytes().all(valid) {
        return None;
    }
    Some((normalize_host(name)?, port))
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::*;

    const APP: &str = "app.example.test";

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.append(
                HeaderName::from_bytes(name.as_bytes()).expect("name"),
                HeaderValue::from_str(value).expect("value"),
            );
        }
        headers
    }

    const UNSAFE: [Method; 4] = [Method::POST, Method::PUT, Method::PATCH, Method::DELETE];

    #[test]
    fn sec_fetch_site_decides_when_present() {
        for method in [Method::GET, Method::HEAD, Method::OPTIONS]
            .into_iter()
            .chain(UNSAFE)
        {
            let same = headers(&[("host", APP), ("sec-fetch-site", "same-origin")]);
            assert_eq!(cross_site(&method, &same), None, "{method}");
            for value in ["same-site", "cross-site", "Same-Origin", "same-origin ", ""] {
                let other = headers(&[("host", APP), ("sec-fetch-site", value)]);
                assert_eq!(
                    cross_site(&method, &other),
                    Some("sec-fetch-site"),
                    "{method} {value:?}"
                );
            }
            // Repeated, even when both say same-origin, and joined into one value.
            let repeated = headers(&[
                ("host", APP),
                ("sec-fetch-site", "same-origin"),
                ("sec-fetch-site", "same-origin"),
            ]);
            assert_eq!(
                cross_site(&method, &repeated),
                Some("sec-fetch-site"),
                "{method}"
            );
            let joined = headers(&[("host", APP), ("sec-fetch-site", "same-origin, cross-site")]);
            assert_eq!(
                cross_site(&method, &joined),
                Some("sec-fetch-site"),
                "{method}"
            );
            // A same-origin Sec-Fetch-Site is enough: Origin is not looked at then.
            let both = headers(&[
                ("host", APP),
                ("sec-fetch-site", "same-origin"),
                ("origin", "https://attacker.example.test"),
            ]);
            assert_eq!(cross_site(&method, &both), None, "{method}");
        }
        // A typed or bookmarked address may read, never write.
        let typed = headers(&[("host", APP), ("sec-fetch-site", "none")]);
        assert_eq!(cross_site(&Method::GET, &typed), None);
        assert_eq!(cross_site(&Method::HEAD, &typed), None);
        for method in UNSAFE {
            assert_eq!(
                cross_site(&method, &typed),
                Some("sec-fetch-site"),
                "{method}"
            );
        }
    }

    #[test]
    fn without_sec_fetch_site_origin_decides_for_writes() {
        let with_origin = |host: &str, origin: &str| headers(&[("host", host), ("origin", origin)]);
        for method in UNSAFE {
            for (host, origin) in [
                (APP, "https://app.example.test"),
                (APP, "https://APP.Example.Test"),
                (APP, "https://app.example.test:443"),
                ("app.example.test:443", "https://app.example.test"),
                ("App.Example.Test.", "https://app.example.test"),
                ("app.example.test:8443", "https://app.example.test:8443"),
                ("localhost:5173", "http://localhost:5173"),
                ("127.0.0.1:8080", "http://127.0.0.1:8080"),
                ("localhost", "http://localhost"),
            ] {
                assert_eq!(
                    cross_site(&method, &with_origin(host, origin)),
                    None,
                    "{method} {host} {origin}"
                );
            }
            for (host, origin) in [
                (APP, "https://attacker.example.test"),
                (APP, "https://gw.example.test"),
                (APP, "https://app.example.test.attacker.example.test"),
                (APP, "https://app.example.test:8443"),
                ("app.example.test:8443", "https://app.example.test"),
                (APP, "http://app.example.test"),
                ("app.example.test:443", "http://app.example.test:443"),
                ("localhost:5173", "https://localhost:5174"),
                ("localhost:5173", "http://127.0.0.1:5173"),
                (APP, "null"),
                (APP, ""),
                (APP, "app.example.test"),
                (APP, "https://app.example.test/"),
                (APP, "https://app.example.test/path"),
                (APP, "https://user@app.example.test"),
                (APP, "https://app.example.test:"),
                (APP, "https://app.example.test:x"),
                (APP, "https://app.example.test:99999"),
                (APP, "https://[::1]"),
                (APP, "HTTPS://app.example.test"),
                (APP, "ftp://app.example.test"),
                ("[::1]:443", "https://[::1]"),
                ("app.example.test:", "https://app.example.test"),
            ] {
                assert_eq!(
                    cross_site(&method, &with_origin(host, origin)),
                    Some("origin"),
                    "{method} {host} {origin}"
                );
            }
            // Repeated Origin or Host, or no Host, is not guessed between.
            let repeated = headers(&[
                ("host", APP),
                ("origin", "https://app.example.test"),
                ("origin", "https://app.example.test"),
            ]);
            assert_eq!(cross_site(&method, &repeated), Some("origin"), "{method}");
            let two_hosts = headers(&[
                ("host", APP),
                ("host", APP),
                ("origin", "https://app.example.test"),
            ]);
            assert_eq!(cross_site(&method, &two_hosts), Some("origin"), "{method}");
            let no_host = headers(&[("origin", "https://app.example.test")]);
            assert_eq!(cross_site(&method, &no_host), Some("origin"), "{method}");
        }
    }

    #[test]
    fn reads_and_requests_without_either_header_go_on() {
        let foreign = headers(&[("host", APP), ("origin", "https://attacker.example.test")]);
        assert_eq!(cross_site(&Method::GET, &foreign), None);
        assert_eq!(cross_site(&Method::HEAD, &foreign), None);
        // OPTIONS can carry a body and change state: it is checked like a write.
        assert_eq!(cross_site(&Method::OPTIONS, &foreign), Some("origin"));
        for method in [Method::GET, Method::HEAD, Method::OPTIONS]
            .into_iter()
            .chain(UNSAFE)
        {
            assert_eq!(
                cross_site(&method, &headers(&[("host", APP)])),
                None,
                "{method}"
            );
            assert_eq!(cross_site(&method, &HeaderMap::new()), None, "{method}");
        }
    }

    #[test]
    fn the_message_key_is_the_contract_one() {
        assert_eq!(CROSS_SITE_REQUEST.as_str(), "security.cross_site_request");
    }
}
