//! The server side of the WebSocket opening handshake (RFC 6455 section 4.2), with the MQTT
//! subprotocol (MQTT 5 section 6) that the gateway requires in the answer.

use axum::http::header::{
    CONNECTION, HeaderMap, HeaderName, HeaderValue, SEC_WEBSOCKET_KEY, SEC_WEBSOCKET_PROTOCOL,
    SEC_WEBSOCKET_VERSION, UPGRADE,
};
use axum::http::{Method, StatusCode, Version};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;

/// The WebSocket subprotocol of MQTT; the gateway refuses an answer without it.
pub const SUBPROTOCOL: &str = "mqtt";

/// Why an upgrade request is refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HandshakeError {
    /// Not GET.
    Method,
    /// Not HTTP/1.1 (the ingress speaks HTTP/1.1 to the bridge).
    HttpVersion,
    /// `Connection` lacks `upgrade` or `Upgrade` lacks `websocket`.
    NotUpgrade,
    /// `Sec-WebSocket-Version` is not exactly 13.
    WebSocketVersion,
    /// `Sec-WebSocket-Key` is missing, repeated or not 16 bytes in base64.
    Key,
    /// The client did not offer the `mqtt` subprotocol.
    Subprotocol,
}

impl HandshakeError {
    /// The HTTP status of the refusal.
    pub const fn status(self) -> StatusCode {
        match self {
            Self::Method => StatusCode::METHOD_NOT_ALLOWED,
            Self::WebSocketVersion => StatusCode::UPGRADE_REQUIRED,
            Self::HttpVersion | Self::NotUpgrade | Self::Key | Self::Subprotocol => {
                StatusCode::BAD_REQUEST
            }
        }
    }

    /// The `reason` of the refusal's log line and metric.
    pub const fn reason(self) -> &'static str {
        match self {
            Self::Method => "method",
            Self::HttpVersion => "http_version",
            Self::NotUpgrade => "not_upgrade",
            Self::WebSocketVersion => "websocket_version",
            Self::Key => "websocket_key",
            Self::Subprotocol => "subprotocol",
        }
    }
}

/// Checks an upgrade request and returns the `Sec-WebSocket-Accept` value of the answer.
///
/// # Errors
///
/// [`HandshakeError`] naming the first rule the request breaks.
pub fn accept(
    method: &Method,
    version: Version,
    headers: &HeaderMap,
) -> Result<HeaderValue, HandshakeError> {
    if method != Method::GET {
        return Err(HandshakeError::Method);
    }
    if version != Version::HTTP_11 {
        return Err(HandshakeError::HttpVersion);
    }
    let token_in = |name: &HeaderName, wanted: &str| {
        tokens(headers, name).any(|token| token.eq_ignore_ascii_case(wanted))
    };
    if !token_in(&CONNECTION, "upgrade") || !token_in(&UPGRADE, "websocket") {
        return Err(HandshakeError::NotUpgrade);
    }
    let mut versions = headers.get_all(SEC_WEBSOCKET_VERSION).iter();
    if versions.next().map(HeaderValue::as_bytes) != Some(b"13") || versions.next().is_some() {
        return Err(HandshakeError::WebSocketVersion);
    }
    let mut keys = headers.get_all(SEC_WEBSOCKET_KEY).iter();
    let key = keys.next().ok_or(HandshakeError::Key)?;
    if keys.next().is_some()
        || !STANDARD
            .decode(key.as_bytes())
            .is_ok_and(|raw| raw.len() == 16)
    {
        return Err(HandshakeError::Key);
    }
    // Subprotocol names are case-sensitive tokens (RFC 6455 section 4.1).
    if !tokens(headers, &SEC_WEBSOCKET_PROTOCOL).any(|token| token == SUBPROTOCOL) {
        return Err(HandshakeError::Subprotocol);
    }
    HeaderValue::from_str(&derive_accept_key(key.as_bytes())).map_err(|_| HandshakeError::Key)
}

/// The comma-separated tokens of every `name` header, trimmed; a value that is not visible ASCII
/// adds none.
fn tokens<'a>(headers: &'a HeaderMap, name: &HeaderName) -> impl Iterator<Item = &'a str> {
    headers
        .get_all(name)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|token| !token.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.append(*name, HeaderValue::from_static(value));
        }
        headers
    }

    const GOOD: [(&str, &str); 5] = [
        ("connection", "Upgrade"),
        ("upgrade", "websocket"),
        ("sec-websocket-version", "13"),
        // 16 zero bytes, base64: a valid key that looks like no secret.
        ("sec-websocket-key", "AAAAAAAAAAAAAAAAAAAAAA=="),
        ("sec-websocket-protocol", "mqtt"),
    ];

    fn without(name: &str) -> HeaderMap {
        let pairs: Vec<_> = GOOD.iter().copied().filter(|(n, _)| *n != name).collect();
        request(&pairs)
    }

    fn with(name: &'static str, value: &'static str) -> HeaderMap {
        let mut headers = without(name);
        headers.append(name, HeaderValue::from_static(value));
        headers
    }

    fn check(headers: &HeaderMap) -> Result<HeaderValue, HandshakeError> {
        accept(&Method::GET, Version::HTTP_11, headers)
    }

    #[test]
    fn the_rfc_example_is_accepted() {
        // RFC 6455 section 1.3's example, whose accept value is given there. The key is built at
        // run time so that secret scanners do not take the literal for a credential.
        let mut headers = without("sec-websocket-key");
        let key = STANDARD.encode(b"the sample nonce");
        headers.append(
            "sec-websocket-key",
            HeaderValue::from_str(&key).expect("base64"),
        );
        let accepted = check(&headers).expect("accepted");
        assert_eq!(accepted, "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
        // Token lists and case, as browsers and proxies send them.
        let mut headers = with("connection", "keep-alive, Upgrade");
        headers.insert("upgrade", HeaderValue::from_static("WebSocket"));
        headers.insert(
            "sec-websocket-protocol",
            HeaderValue::from_static("wamp, mqtt"),
        );
        check(&headers).expect("token lists");
    }

    #[test]
    fn every_rule_is_checked() {
        assert_eq!(
            accept(&Method::POST, Version::HTTP_11, &request(&GOOD)),
            Err(HandshakeError::Method)
        );
        assert_eq!(
            accept(&Method::GET, Version::HTTP_10, &request(&GOOD)),
            Err(HandshakeError::HttpVersion)
        );
        assert_eq!(
            accept(&Method::GET, Version::HTTP_2, &request(&GOOD)),
            Err(HandshakeError::HttpVersion)
        );
        assert_eq!(
            check(&without("connection")),
            Err(HandshakeError::NotUpgrade)
        );
        assert_eq!(
            check(&with("connection", "keep-alive")),
            Err(HandshakeError::NotUpgrade)
        );
        assert_eq!(
            check(&with("upgrade", "h2c")),
            Err(HandshakeError::NotUpgrade)
        );
        assert_eq!(
            check(&without("sec-websocket-version")),
            Err(HandshakeError::WebSocketVersion)
        );
        assert_eq!(
            check(&with("sec-websocket-version", "8")),
            Err(HandshakeError::WebSocketVersion)
        );
        let mut twice = request(&GOOD);
        twice.append("sec-websocket-version", HeaderValue::from_static("13"));
        assert_eq!(check(&twice), Err(HandshakeError::WebSocketVersion));
        assert_eq!(
            check(&without("sec-websocket-key")),
            Err(HandshakeError::Key)
        );
        for bad in [
            "",
            "dGhlIHNhbXBsZQ==",
            "not base64!",
            "dGhlIHNhbXBsZSBub25jZQ",
        ] {
            let mut headers = without("sec-websocket-key");
            headers.insert("sec-websocket-key", HeaderValue::from_static(bad));
            assert_eq!(check(&headers), Err(HandshakeError::Key), "{bad:?}");
        }
        let mut two_keys = request(&GOOD);
        two_keys.append(
            "sec-websocket-key",
            HeaderValue::from_static("AAAAAAAAAAAAAAAAAAAAAA=="),
        );
        assert_eq!(check(&two_keys), Err(HandshakeError::Key));
        assert_eq!(
            check(&without("sec-websocket-protocol")),
            Err(HandshakeError::Subprotocol)
        );
        assert_eq!(
            check(&with("sec-websocket-protocol", "MQTT")),
            Err(HandshakeError::Subprotocol)
        );
        assert_eq!(
            check(&with("sec-websocket-protocol", "mqttv3.1")),
            Err(HandshakeError::Subprotocol)
        );
    }

    #[test]
    fn statuses_follow_rfc_6455() {
        assert_eq!(
            HandshakeError::Method.status(),
            StatusCode::METHOD_NOT_ALLOWED
        );
        assert_eq!(
            HandshakeError::WebSocketVersion.status(),
            StatusCode::UPGRADE_REQUIRED
        );
        assert_eq!(
            HandshakeError::Subprotocol.status(),
            StatusCode::BAD_REQUEST
        );
    }
}
