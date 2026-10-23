//! Email addresses as the identity service accepts, compares and stores them: what the login
//! flow (`src/login.rs`) looks a user up by, what the SMTP client (`src/mail.rs`) sends to, and
//! what `identity.users.email` holds.
//!
//! Two crates compile this file: the service, and the migration Job (`tools/migrate`, with
//! `#[path]`), whose `bootstrap-platform-admin` stores the first platform administrator's address
//! (Laysi/trustiot-v2#582). One normalization, so the address it stores is the one a login looks
//! up. Keep the file self-contained: the standard library only.

use std::fmt;

/// The longest address (RFC 5321 section 4.5.3.1.3 and its errata: 254 characters).
pub const MAX_ADDRESS_LEN: usize = 254;

/// Whether `text` is an address the service sends to or from: ASCII, a dot-atom local part of
/// at most 64 characters, and a domain of letters, digits and hyphens in labels of at most 63
/// characters; at most [`MAX_ADDRESS_LEN`] in all. No quoted local parts, comments or address
/// literals, so an address can never carry a line break, `<`, `>` or a space into a command or
/// a header.
#[must_use]
pub fn is_address(text: &str) -> bool {
    if text.len() > MAX_ADDRESS_LEN {
        return false;
    }
    let Some((local, domain)) = text.split_once('@') else {
        return false;
    };
    let local_ok = (1..=64).contains(&local.len())
        && local
            .split('.')
            .all(|atom| !atom.is_empty() && atom.bytes().all(is_atext));
    let domain_ok = (1..=253).contains(&domain.len())
        && domain.split('.').all(|label| {
            (1..=63).contains(&label.len())
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                && !label.starts_with('-')
                && !label.ends_with('-')
        });
    local_ok && domain_ok
}

/// RFC 5322 `atext`: the characters of an unquoted local part besides `.`.
const fn is_atext(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'/'
                | b'='
                | b'?'
                | b'^'
                | b'_'
                | b'`'
                | b'{'
                | b'|'
                | b'}'
                | b'~'
        )
}

/// An email address as the service compares and stores it: trimmed, ASCII lower case, and
/// accepted by [`is_address`]. Personal data: `Debug` never prints it, and nothing logs it.
#[derive(Clone, PartialEq, Eq)]
pub struct EmailAddress(String);

impl EmailAddress {
    /// Normalizes and checks an address; `None` when it is not one.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let address = text.trim().to_ascii_lowercase();
        is_address(&address).then_some(Self(address))
    }

    /// The address.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for EmailAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("EmailAddress(<redacted>)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_are_plain_ascii_dot_atoms_at_hostnames() {
        for valid in [
            "user@example.com",
            "first.last@example.org",
            "user+tag@sub.example.com",
            "o'brien@example.com",
            "a@b",
            "x-1@example-host.example",
        ] {
            assert!(is_address(valid), "{valid:?}");
        }
        let long_local = format!("{}@example.com", "a".repeat(65));
        let long_label = format!("user@{}.com", "a".repeat(64));
        let too_long = format!("{}@{}.com", "a".repeat(64), "b".repeat(190));
        for invalid in [
            "",
            "user",
            "@example.com",
            "user@",
            "user@@example.com",
            "a@b@example.com",
            ".user@example.com",
            "user.@example.com",
            "us..er@example.com",
            "user@-example.com",
            "user@example-.com",
            "user@example..com",
            "user@example.com.",
            "user name@example.com",
            "user@exa mple.com",
            "\"user\"@example.com",
            "user@[192.0.2.1]",
            "user@example.com\r\nRCPT TO:<x@example.org>",
            "user@example.com>",
            "<user@example.com",
            "user\n@example.com",
            "üser@example.com",
            "user@exämple.com",
            long_local.as_str(),
            long_label.as_str(),
            too_long.as_str(),
        ] {
            assert!(!is_address(invalid), "{invalid:?}");
        }
    }

    #[test]
    fn addresses_are_trimmed_and_lower_cased() {
        assert_eq!(
            EmailAddress::parse("  User.Name+Tag@Example.COM \n").map(|email| email.0),
            Some("user.name+tag@example.com".to_owned())
        );
        for invalid in ["", "user", "user@", "user @example.com", "üser@example.com"] {
            assert!(EmailAddress::parse(invalid).is_none(), "{invalid:?}");
        }
        assert_eq!(
            format!("{:?}", EmailAddress::parse("user@example.com")),
            "Some(EmailAddress(<redacted>))"
        );
    }
}
