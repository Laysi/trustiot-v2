//! Passwords the Job sets: where they come from, what it accepts, and the hashed forms it sends
//! instead of the plaintext.

use std::fmt;
use std::path::Path;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};

/// Fewer characters than this is refused: the chart's instructions generate 64 hex digits.
pub const MIN_PASSWORD_CHARS: usize = 16;
/// More is refused too; no generator the runbooks name comes close.
pub const MAX_PASSWORD_CHARS: usize = 512;

/// PBKDF2 iterations of the SCRAM verifier: Postgres's own default (`scram_iterations`).
const SCRAM_ITERATIONS: u32 = 4096;

/// A password. `Debug` and `Display` never show it.
#[derive(Clone, PartialEq, Eq)]
pub struct Password(String);

impl fmt::Debug for Password {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Password(<redacted>)")
    }
}

/// Why a password was refused. Never contains the password.
#[derive(Debug, thiserror::Error)]
pub enum PasswordError {
    #[error(
        "the password of {owner} must have between {MIN_PASSWORD_CHARS} and {MAX_PASSWORD_CHARS} characters"
    )]
    Length { owner: String },
    #[error(
        "the password of {owner} must be printable ASCII without spaces (for example \
         `openssl rand -hex 32`); a trailing newline from `echo` is a common cause"
    )]
    Characters { owner: String },
    #[error("cannot read the password of {owner} from {path}: {source}")]
    Read {
        owner: String,
        path: String,
        source: std::io::Error,
    },
}

impl Password {
    /// Accepts `value` as the password of `owner` (a role or account name, for messages).
    ///
    /// Printable ASCII only: Postgres runs SASLprep over non-ASCII passwords and ClickHouse
    /// does not, so a non-ASCII password could hash differently on the two sides of a login.
    ///
    /// # Errors
    ///
    /// When it is too short, too long, or has other characters.
    pub fn new(owner: &str, value: String) -> Result<Self, PasswordError> {
        let length = value.chars().count();
        if !(MIN_PASSWORD_CHARS..=MAX_PASSWORD_CHARS).contains(&length) {
            return Err(PasswordError::Length {
                owner: owner.to_owned(),
            });
        }
        if !value.bytes().all(|byte| byte.is_ascii_graphic()) {
            return Err(PasswordError::Characters {
                owner: owner.to_owned(),
            });
        }
        Ok(Self(value))
    }

    /// Reads the password of `owner` from `dir/<owner>` (a mounted Secret key). `Ok(None)` when
    /// the file does not exist.
    ///
    /// # Errors
    ///
    /// When the file exists but cannot be read or holds an unacceptable password.
    pub async fn read(dir: &Path, owner: &str) -> Result<Option<Self>, PasswordError> {
        let path = dir.join(owner);
        match tokio::fs::read(&path).await {
            Ok(bytes) => {
                let text = String::from_utf8(bytes).map_err(|_| PasswordError::Characters {
                    owner: owner.to_owned(),
                })?;
                Self::new(owner, text).map(Some)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(PasswordError::Read {
                owner: owner.to_owned(),
                path: path.display().to_string(),
                source,
            }),
        }
    }

    /// The plaintext, for logging in with it.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// A Postgres SCRAM-SHA-256 verifier for this password with a fresh random salt, in the
    /// form `ALTER ROLE ... PASSWORD` stores as is (RFC 5802, RFC 7677):
    /// `SCRAM-SHA-256$<iterations>:<salt>$<StoredKey>:<ServerKey>`.
    ///
    /// # Errors
    ///
    /// When the operating system has no randomness for the salt.
    pub fn scram_verifier(&self) -> Result<String, getrandom::Error> {
        let mut salt = [0_u8; 16];
        getrandom::fill(&mut salt)?;
        Ok(scram_verifier_with(
            self.0.as_bytes(),
            &salt,
            SCRAM_ITERATIONS,
        ))
    }

    /// A ClickHouse `sha256_hash` with a fresh random salt: `(hex(SHA-256(password || salt)),
    /// salt)`, for `IDENTIFIED WITH sha256_hash BY <hash> SALT <salt>`.
    ///
    /// # Errors
    ///
    /// When the operating system has no randomness for the salt.
    pub fn clickhouse_sha256(&self) -> Result<(String, String), getrandom::Error> {
        let mut bytes = [0_u8; 16];
        getrandom::fill(&mut bytes)?;
        let salt = hex(&bytes);
        let mut digest = Sha256::new();
        digest.update(self.0.as_bytes());
        digest.update(salt.as_bytes());
        Ok((hex(&digest.finalize()), salt))
    }
}

fn hex(bytes: &[u8]) -> String {
    use fmt::Write as _;
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        // Writing to a String cannot fail.
        let _ = write!(text, "{byte:02x}");
    }
    text
}

fn hmac(key: &[u8], message: &[u8]) -> [u8; 32] {
    // HMAC accepts keys of any length, so this never fails.
    let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(key)
        .unwrap_or_else(|_| unreachable!("HMAC-SHA-256 takes keys of any length"));
    mac.update(message);
    mac.finalize().into_bytes().into()
}

/// The verifier for `password`, `salt` and `iterations` (deterministic, for tests).
#[must_use]
pub fn scram_verifier_with(password: &[u8], salt: &[u8], iterations: u32) -> String {
    // Hi(password, salt, i): PBKDF2 with HMAC-SHA-256 and one 32-byte block.
    let mut first = salt.to_vec();
    first.extend_from_slice(&1_u32.to_be_bytes());
    let mut block = hmac(password, &first);
    let mut salted = block;
    for _ in 1..iterations {
        block = hmac(password, &block);
        for (out, byte) in salted.iter_mut().zip(block) {
            *out ^= byte;
        }
    }
    let client_key = hmac(&salted, b"Client Key");
    let stored_key = Sha256::digest(client_key);
    let server_key = hmac(&salted, b"Server Key");
    format!(
        "SCRAM-SHA-256${iterations}:{}${}:{}",
        STANDARD.encode(salt),
        STANDARD.encode(stored_key),
        STANDARD.encode(server_key)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_long_and_odd_passwords_are_refused_without_echoing_them() {
        for bad in [
            "short",
            "with space inside it!",
            "trailing-newline-01234\n",
            "ünïcödé-password-xx",
        ] {
            let error = Password::new("identity_app", bad.to_owned()).expect_err(bad);
            let text = error.to_string();
            assert!(text.contains("identity_app"), "{text}");
            assert!(
                !text.contains(bad.trim()),
                "the message must not contain the password: {text}"
            );
        }
        let long = "x".repeat(MAX_PASSWORD_CHARS + 1);
        assert!(Password::new("r", long).is_err());
        assert!(Password::new("r", "0123456789abcdef".to_owned()).is_ok());
    }

    #[test]
    fn debug_hides_the_password() {
        let password = Password::new("r", "0123456789abcdef-secret".to_owned()).expect("valid");
        assert!(!format!("{password:?}").contains("secret"));
    }

    /// RFC 7677's example: user "user", password "pencil", salt "W22ZaJ0SNY7soEsUEjb6gQ==",
    /// 4096 iterations; the server's StoredKey and ServerKey follow from its proof values.
    #[test]
    fn the_verifier_matches_rfc_7677() {
        let salt = STANDARD.decode("W22ZaJ0SNY7soEsUEjb6gQ==").expect("salt");
        let verifier = scram_verifier_with(b"pencil", &salt, 4096);
        assert_eq!(
            verifier,
            "SCRAM-SHA-256$4096:W22ZaJ0SNY7soEsUEjb6gQ==$WG5d8oPm3OtcPnkdi4Uo7BkeZkBFzpcXkuLmtbsT4qY=:wfPLwcE6nTWhTAmQ7tl2KeoiWGPlZqQxSrmfPwDl2dU="
        );
    }

    #[test]
    fn verifiers_use_fresh_salts() {
        let password = Password::new("r", "0123456789abcdef".to_owned()).expect("valid");
        let first = password.scram_verifier().expect("random");
        let second = password.scram_verifier().expect("random");
        assert_ne!(first, second);
        assert!(first.starts_with("SCRAM-SHA-256$4096:"));
        let (hash, salt) = password.clickhouse_sha256().expect("random");
        assert_eq!(hash.len(), 64);
        assert_eq!(salt.len(), 32);
    }
}
