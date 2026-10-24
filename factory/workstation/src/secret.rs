//! Wrappers for the secrets the tool handles: the session cookie, the login code, `Set-Cookie`
//! values and the values of `--headers-file` (the workstation's private key is
//! [`crate::identity::WorkstationKey`]).
//!
//! `Debug` and `Display` never show the value, so a secret that ends up in a log line, an error
//! message or a `{:?}` shows `<redacted>`. The value is wiped from memory when the wrapper is
//! dropped; copies made to send it (a header value, the request body handed to hyper) are not.

use std::fmt;

use zeroize::Zeroizing;

/// What `Debug` and `Display` print instead of a secret.
pub const REDACTED: &str = "<redacted>";

/// A secret text. Only [`Secret::expose`] gives the value.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(Zeroizing<String>);

impl Secret {
    /// Wraps `value`.
    #[must_use]
    pub fn new(value: String) -> Self {
        Self(Zeroizing::new(value))
    }

    /// The value, for the one place that sends or stores it.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(REDACTED)
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(REDACTED)
    }
}

/// The name of the platform's session cookie (services/identity/README.md, "session").
pub const SESSION_COOKIE_NAME: &str = "__Host-trustiot-session";

/// Length of a session cookie value: 32 random bytes in base64url without padding.
pub const SESSION_COOKIE_LEN: usize = 43;

/// The value of the platform administrator's session cookie.
#[derive(Clone, PartialEq, Eq)]
pub struct SessionCookie(Secret);

impl SessionCookie {
    /// `value`, when it has the session cookie's form: 43 characters of the base64url alphabet.
    /// Nothing about a refused value is reported.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        let valid = value.len() == SESSION_COOKIE_LEN
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'));
        valid.then(|| Self(Secret::new(value.to_owned())))
    }

    /// The value.
    #[must_use]
    pub fn expose(&self) -> &str {
        self.0.expose()
    }

    /// The `Cookie` request header's value: `__Host-trustiot-session=<value>`.
    #[must_use]
    pub fn header_value(&self) -> Secret {
        Secret::new(format!("{SESSION_COOKIE_NAME}={}", self.expose()))
    }

    /// The session cookie's value in the `Set-Cookie` headers of a response, when exactly one of
    /// them sets it with a well-formed value.
    #[must_use]
    pub fn from_set_cookies(set_cookies: &[SetCookie]) -> Option<Self> {
        let prefix = format!("{SESSION_COOKIE_NAME}=");
        let mut found = set_cookies.iter().filter_map(|header| {
            let text = header.0.expose();
            let pair = text.split(';').next()?.trim();
            pair.strip_prefix(prefix.as_str())
        });
        let (Some(value), None) = (found.next(), found.next()) else {
            return None;
        };
        Self::parse(value)
    }
}

impl fmt::Debug for SessionCookie {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SessionCookie({REDACTED})")
    }
}

impl fmt::Display for SessionCookie {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(REDACTED)
    }
}

/// Number of digits of a login code (services/identity/README.md, "登入").
pub const CODE_LEN: usize = 8;

/// A login code from the email, read from stdin only.
#[derive(Clone, PartialEq, Eq)]
pub struct VerificationCode(Secret);

impl VerificationCode {
    /// `text` without surrounding white space, when it is exactly eight ASCII digits.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let code = text.trim();
        (code.len() == CODE_LEN && code.bytes().all(|byte| byte.is_ascii_digit()))
            .then(|| Self(Secret::new(code.to_owned())))
    }

    /// The code.
    #[must_use]
    pub fn expose(&self) -> &str {
        self.0.expose()
    }
}

impl fmt::Debug for VerificationCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "VerificationCode({REDACTED})")
    }
}

impl fmt::Display for VerificationCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(REDACTED)
    }
}

/// One `Set-Cookie` header of a response, as received.
#[derive(Clone, PartialEq, Eq)]
pub struct SetCookie(Secret);

impl SetCookie {
    /// Wraps a received header value.
    #[must_use]
    pub fn new(value: String) -> Self {
        Self(Secret::new(value))
    }
}

impl fmt::Debug for SetCookie {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SetCookie({REDACTED})")
    }
}

impl fmt::Display for SetCookie {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(REDACTED)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const COOKIE: &str = "AbCdEfGhIjKlMnOpQrStUvWxYz0123456789-_abcde";
    /// [`COOKIE`] without its first character.
    const SHORT: &str = "bCdEfGhIjKlMnOpQrStUvWxYz0123456789-_abcde";

    #[test]
    fn debug_and_display_never_show_a_secret() {
        let secret = Secret::new("hunter2-secret-value".to_owned());
        let cookie = SessionCookie::parse(COOKIE).expect("a well-formed cookie");
        let code = VerificationCode::parse("12345678").expect("eight digits");
        let set_cookie = SetCookie::new(format!("{SESSION_COOKIE_NAME}={COOKIE}; Path=/"));
        let shown = [
            format!("{secret:?} {secret}"),
            format!("{cookie:?} {cookie}"),
            format!("{:?}", cookie.header_value()),
            format!("{code:?} {code}"),
            format!("{set_cookie:?} {set_cookie}"),
        ]
        .join(" ");
        for value in ["hunter2", COOKIE, "12345678"] {
            assert!(!shown.contains(value), "{value} shows in {shown}");
        }
        assert!(shown.contains(REDACTED));
    }

    #[test]
    fn session_cookies_have_the_identity_services_form() {
        assert!(SessionCookie::parse(COOKIE).is_some());
        assert!(SessionCookie::parse(SHORT).is_none());
        assert!(SessionCookie::parse(&format!("{SHORT}=")).is_none());
        assert!(SessionCookie::parse(&format!("{SHORT}+")).is_none());
        assert_eq!(
            SessionCookie::parse(COOKIE)
                .expect("a cookie")
                .header_value()
                .expose(),
            format!("__Host-trustiot-session={COOKIE}")
        );
    }

    #[test]
    fn the_session_cookie_is_found_in_exactly_one_set_cookie_header() {
        let set = |text: &str| SetCookie::new(text.to_owned());
        let full = format!(
            "{SESSION_COOKIE_NAME}={COOKIE}; Max-Age=604800; Path=/; Secure; HttpOnly; SameSite=Lax"
        );
        let found = SessionCookie::from_set_cookies(&[set("other=1; Path=/"), set(&full)])
            .expect("the session cookie");
        assert_eq!(found.expose(), COOKIE);
        assert!(SessionCookie::from_set_cookies(&[set("other=1")]).is_none());
        assert!(SessionCookie::from_set_cookies(&[set(&full), set(&full)]).is_none());
        assert!(
            SessionCookie::from_set_cookies(&[set(&format!("{SESSION_COOKIE_NAME}=short"))])
                .is_none()
        );
        // A clearing header (logout) is not a session.
        assert!(
            SessionCookie::from_set_cookies(&[set(&format!(
                "{SESSION_COOKIE_NAME}=; Max-Age=0; Path=/"
            ))])
            .is_none()
        );
    }

    #[test]
    fn codes_are_eight_digits() {
        assert_eq!(
            VerificationCode::parse(" 01234567\n")
                .expect("a code")
                .expose(),
            "01234567"
        );
        for refused in ["1234567", "123456789", "1234567a", "１２３４５６７８", ""] {
            assert!(VerificationCode::parse(refused).is_none(), "{refused}");
        }
    }
}
