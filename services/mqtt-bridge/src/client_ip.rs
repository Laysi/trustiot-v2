//! The gateway's source IP, read only from the header the deployment trusts (decisions 6 and 27;
//! README.md, "來源 IP").
//!
//! Behind Cloudflare (SaaS) only `CF-Connecting-IP` is trustworthy: Cloudflare sets it on every
//! request itself, over whatever the client sent. `X-Forwarded-For` (its
//! left-most entries) and `True-Client-IP` reach the bridge exactly as the client wrote them, so
//! they are never read, not even as a fallback. On-prem the ingress gateway appends the address it
//! saw to the right of `X-Forwarded-For`; that mode is Laysi/trustiot-v2#446 and the bridge refuses
//! to start with it until then.

use std::net::IpAddr;

use axum::http::HeaderMap;

/// The setting value of the SaaS source (`TRUSTIOT_CLIENT_IP_SOURCE`, chart
/// `trustiot.clientIpSource`).
pub const CF_CONNECTING_IP: &str = "cf-connecting-ip";
/// The setting value of the on-prem source, not built yet (Laysi/trustiot-v2#446).
pub const X_FORWARDED_FOR_RIGHTMOST: &str = "x-forwarded-for-rightmost";

/// Where the trusted source IP comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClientIpSource {
    /// `CF-Connecting-IP`, set by Cloudflare (SaaS, `ingress.exposure: tunnel`).
    CfConnectingIp,
}

/// Why a request has no trusted source IP. The request is refused: a request without it did not
/// come the way the deployment says traffic comes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClientIpError {
    /// The trusted header is missing.
    Missing,
    /// The trusted header appears more than once.
    Repeated,
    /// The trusted header is not one IP address.
    Invalid,
}

impl ClientIpError {
    /// The `reason` field of the refusal's log line.
    pub const fn reason(self) -> &'static str {
        match self {
            Self::Missing => "client_ip_missing",
            Self::Repeated => "client_ip_repeated",
            Self::Invalid => "client_ip_invalid",
        }
    }
}

impl ClientIpSource {
    /// The source a setting value names. The message of an error never contains the value.
    ///
    /// # Errors
    ///
    /// A value the bridge does not know, or the on-prem source, which is not built yet.
    pub fn parse(value: &str) -> Result<Self, &'static str> {
        match value {
            CF_CONNECTING_IP => Ok(Self::CfConnectingIp),
            X_FORWARDED_FOR_RIGHTMOST => Err(
                "x-forwarded-for-rightmost (on-prem, direct exposure) is not supported yet \
                 (Laysi/trustiot-v2#446); the bridge supports cf-connecting-ip only",
            ),
            _ => Err(
                "is not a source the bridge knows; it supports cf-connecting-ip only \
                 (the chart derives it from ingress.exposure)",
            ),
        }
    }

    /// The header this source reads.
    pub const fn header(self) -> &'static str {
        match self {
            Self::CfConnectingIp => CF_CONNECTING_IP,
        }
    }

    /// The source IP of a request, from this source's header only.
    ///
    /// # Errors
    ///
    /// [`ClientIpError`] when the header is missing, repeated or not one IP address.
    pub fn client_ip(self, headers: &HeaderMap) -> Result<IpAddr, ClientIpError> {
        let mut values = headers.get_all(self.header()).iter();
        let value = values.next().ok_or(ClientIpError::Missing)?;
        if values.next().is_some() {
            return Err(ClientIpError::Repeated);
        }
        let text = value.to_str().map_err(|_| ClientIpError::Invalid)?.trim();
        text.parse().map_err(|_| ClientIpError::Invalid)
    }
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::*;

    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.append(*name, HeaderValue::from_static(value));
        }
        headers
    }

    const CF: ClientIpSource = ClientIpSource::CfConnectingIp;

    #[test]
    fn only_cf_connecting_ip_counts() {
        let spoofed = headers(&[
            ("x-forwarded-for", "198.51.100.1, 10.0.0.1"),
            ("true-client-ip", "198.51.100.2"),
            ("x-real-ip", "198.51.100.3"),
            ("forwarded", "for=198.51.100.4"),
            ("cf-connecting-ip", "203.0.113.7"),
        ]);
        assert_eq!(
            CF.client_ip(&spoofed),
            Ok("203.0.113.7".parse().expect("IP"))
        );
        let v6 = headers(&[("CF-Connecting-IP", " 2001:db8::7 ")]);
        assert_eq!(CF.client_ip(&v6), Ok("2001:db8::7".parse().expect("IP")));
    }

    #[test]
    fn other_headers_are_no_fallback() {
        let only_spoofed = headers(&[
            ("x-forwarded-for", "198.51.100.1"),
            ("true-client-ip", "198.51.100.2"),
        ]);
        assert_eq!(CF.client_ip(&only_spoofed), Err(ClientIpError::Missing));
    }

    #[test]
    fn repeated_or_malformed_values_are_refused() {
        let repeated = headers(&[
            ("cf-connecting-ip", "203.0.113.7"),
            ("cf-connecting-ip", "198.51.100.1"),
        ]);
        assert_eq!(CF.client_ip(&repeated), Err(ClientIpError::Repeated));
        for bad in [
            "",
            "203.0.113.7, 198.51.100.1",
            "203.0.113.7:443",
            "[2001:db8::7]",
            "fe80::1%eth0",
            "example.com",
            "unknown",
        ] {
            let mut map = HeaderMap::new();
            map.insert(
                "cf-connecting-ip",
                HeaderValue::from_str(bad).expect("value"),
            );
            assert_eq!(CF.client_ip(&map), Err(ClientIpError::Invalid), "{bad:?}");
        }
        let mut opaque = HeaderMap::new();
        opaque.insert(
            "cf-connecting-ip",
            HeaderValue::from_bytes(b"203.0.113.7\xff").expect("value"),
        );
        assert_eq!(CF.client_ip(&opaque), Err(ClientIpError::Invalid));
    }

    #[test]
    fn settings_parse_strictly() {
        assert_eq!(ClientIpSource::parse("cf-connecting-ip"), Ok(CF));
        assert!(
            ClientIpSource::parse("x-forwarded-for-rightmost")
                .expect_err("not yet")
                .contains("Laysi/trustiot-v2#446")
        );
        for unknown in [
            "CF-Connecting-IP",
            "x-forwarded-for",
            "true-client-ip",
            "forwarded",
        ] {
            let message = ClientIpSource::parse(unknown).expect_err("unknown");
            assert!(!message.contains(unknown), "{unknown} echoed: {message}");
        }
        assert!(ClientIpSource::parse("").is_err());
    }
}
