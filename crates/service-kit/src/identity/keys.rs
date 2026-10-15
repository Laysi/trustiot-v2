//! The identity service's public keys: a JWK Set (RFC 7517) of Ed25519 keys (RFC 8037), parsed
//! and checked whole each time the published set changes ([`super::PublishedKeys`]).

use std::fmt;
use std::str::FromStr;
use std::sync::Arc;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use contract::json::JsonError;
use ed25519_dalek::{PUBLIC_KEY_LENGTH, VerifyingKey};
use serde::Deserialize;
use serde::de::IgnoredAny;

use super::ALGORITHM;

/// Most keys a set may hold. A rotation needs two or three at a time (README.md, "公鑰與輪替");
/// the cap keeps a broken set from growing without bound. `crates/identity-keys` holds the
/// identity service's own set to the same cap.
pub const MAX_KEYS: usize = 8;

/// Longest key id accepted, in bytes.
pub const MAX_KEY_ID_LEN: usize = 64;

/// The identity service's public keys, by key id (`kid`). Built from a JWK Set whose every key is
/// an Ed25519 public key; a set with anything else is refused whole, so a service never verifies
/// with a key it cannot use or with a private key in its keys file.
///
/// A service gets the set from the file the platform setting
/// [`KEYS_FILE_SETTING`](super::KEYS_FILE_SETTING) names, through
/// [`PublishedKeys::watch`](super::PublishedKeys::watch). Cloning is cheap.
#[derive(Clone)]
pub struct IdentityKeys {
    keys: Arc<[(String, VerifyingKey)]>,
}

impl IdentityKeys {
    /// Parses and checks a JWK Set in JSON, for example
    /// `{"keys":[{"kty":"OKP","crv":"Ed25519","kid":"2026-10-a","x":"<base64url>"}]}`.
    ///
    /// Every key must have `kty` `OKP`, `crv` `Ed25519`, a `kid` of 1 to 64 visible ASCII
    /// characters that no other key in the set uses, and an `x` that is the base64url (without
    /// padding) encoding of a 32-byte public key that is not of small order. `alg`, `use` and
    /// `key_ops` are optional but, when present, must be `EdDSA`, `sig`, and a list that includes
    /// `verify`. A key with a private part (`d`) is rejected. Other members are ignored, as RFC
    /// 7517 requires.
    ///
    /// # Errors
    ///
    /// When the text is not such a set; the error names the position of the offending key but
    /// never quotes the text.
    pub fn from_jwks(text: &str) -> Result<Self, InvalidKeys> {
        let set: JwkSet =
            serde_json::from_str(text).map_err(|error| InvalidKeys::NotJwkSet(error.into()))?;
        if set.keys.is_empty() {
            return Err(InvalidKeys::Empty);
        }
        if set.keys.len() > MAX_KEYS {
            return Err(InvalidKeys::TooMany);
        }
        let mut keys: Vec<(String, VerifyingKey)> = Vec::with_capacity(set.keys.len());
        for (index, jwk) in set.keys.into_iter().enumerate() {
            let (kid, key) = jwk
                .into_key()
                .map_err(|problem| InvalidKeys::Key { index, problem })?;
            if keys.iter().any(|(existing, _)| *existing == kid) {
                return Err(InvalidKeys::Key {
                    index,
                    problem: KeyProblem::DuplicateKeyId,
                });
            }
            keys.push((kid, key));
        }
        Ok(Self { keys: keys.into() })
    }

    /// The public key with this key id.
    pub(crate) fn get(&self, kid: &str) -> Option<&VerifyingKey> {
        self.keys
            .iter()
            .find(|(candidate, _)| candidate == kid)
            .map(|(_, key)| key)
    }

    /// The key ids in the set, in the set's order.
    pub fn key_ids(&self) -> impl Iterator<Item = &str> {
        self.keys.iter().map(|(kid, _)| kid.as_str())
    }
}

impl fmt::Debug for IdentityKeys {
    /// Only the key ids; public keys are not secret, but they add nothing to a log line.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IdentityKeys")
            .field("key_ids", &self.key_ids().collect::<Vec<_>>())
            .finish()
    }
}

impl FromStr for IdentityKeys {
    type Err = InvalidKeys;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::from_jwks(text)
    }
}

/// Why a JWK Set cannot be the identity service's public keys. Messages never quote the set.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum InvalidKeys {
    /// The text is not a JSON object with a `keys` array of objects.
    #[error("not a JWK Set with a `keys` array ({0})")]
    NotJwkSet(JsonError),
    /// The set has no keys.
    #[error("the JWK Set has no keys")]
    Empty,
    /// The set has more than [`MAX_KEYS`] keys.
    #[error("the JWK Set has more than {MAX_KEYS} keys")]
    TooMany,
    /// One key cannot be used.
    #[error("key {index} of the JWK Set (counting from 0) {problem}")]
    Key {
        /// Position of the key in the `keys` array, from 0.
        index: usize,
        /// What is wrong with it.
        problem: KeyProblem,
    },
}

/// What is wrong with one key of a JWK Set.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum KeyProblem {
    /// `kty` is not `OKP` or `crv` is not `Ed25519`.
    #[error("is not an Ed25519 key: `kty` must be `OKP` and `crv` must be `Ed25519`")]
    NotEd25519,
    /// The key has a private part.
    #[error("contains a private key (`d`); configure only public keys")]
    PrivateKey,
    /// `kid` is missing, empty, too long, or has a character that is not visible ASCII.
    #[error("needs a `kid` of 1 to {MAX_KEY_ID_LEN} visible ASCII characters")]
    BadKeyId,
    /// Another key earlier in the set has the same `kid`.
    #[error("has the same `kid` as an earlier key")]
    DuplicateKeyId,
    /// `x` is missing, not base64url without padding, or not 32 bytes.
    #[error("needs an `x` that is a 32-byte public key in base64url without padding")]
    BadPublicKey,
    /// `x` is not a point on the curve, or is a point of small order.
    #[error("has an `x` that is not a usable Ed25519 public key")]
    WeakPublicKey,
    /// `alg`, `use`, or `key_ops` says the key is for something else.
    #[error("is not for verifying EdDSA signatures: `alg`, `use`, or `key_ops` disagrees")]
    WrongUse,
}

#[derive(Deserialize)]
struct JwkSet {
    keys: Vec<Jwk>,
}

/// The JWK members this kit reads. Duplicate members are a deserialization error, so a set
/// cannot say two different things about one key. A member that is present must have a value of
/// its type: `"alg": null` is an error, not an absent `alg`, and any `d`, even `null`, is a
/// private part.
#[derive(Deserialize)]
struct Jwk {
    #[serde(default, deserialize_with = "super::present")]
    kty: Option<String>,
    #[serde(default, deserialize_with = "super::present")]
    crv: Option<String>,
    #[serde(default, deserialize_with = "super::present")]
    kid: Option<String>,
    #[serde(default, deserialize_with = "super::present")]
    x: Option<String>,
    #[serde(default, deserialize_with = "super::present")]
    d: Option<IgnoredAny>,
    #[serde(default, deserialize_with = "super::present")]
    alg: Option<String>,
    #[serde(rename = "use", default, deserialize_with = "super::present")]
    public_key_use: Option<String>,
    #[serde(default, deserialize_with = "super::present")]
    key_ops: Option<Vec<String>>,
}

impl Jwk {
    fn into_key(self) -> Result<(String, VerifyingKey), KeyProblem> {
        if self.kty.as_deref() != Some("OKP") || self.crv.as_deref() != Some("Ed25519") {
            return Err(KeyProblem::NotEd25519);
        }
        if self.d.is_some() {
            return Err(KeyProblem::PrivateKey);
        }
        let kid = self.kid.ok_or(KeyProblem::BadKeyId)?;
        let kid_valid = !kid.is_empty()
            && kid.len() <= MAX_KEY_ID_LEN
            && kid.bytes().all(|byte| byte.is_ascii_graphic());
        if !kid_valid {
            return Err(KeyProblem::BadKeyId);
        }
        let for_verifying = self.alg.as_deref().is_none_or(|alg| alg == ALGORITHM)
            && self
                .public_key_use
                .as_deref()
                .is_none_or(|usage| usage == "sig")
            && self
                .key_ops
                .as_ref()
                .is_none_or(|ops| ops.iter().any(|op| op == "verify"));
        if !for_verifying {
            return Err(KeyProblem::WrongUse);
        }
        let bytes = self
            .x
            .and_then(|x| URL_SAFE_NO_PAD.decode(x).ok())
            .and_then(|bytes| <[u8; PUBLIC_KEY_LENGTH]>::try_from(bytes).ok())
            .ok_or(KeyProblem::BadPublicKey)?;
        let key = VerifyingKey::from_bytes(&bytes).map_err(|_| KeyProblem::WeakPublicKey)?;
        if key.is_weak() {
            return Err(KeyProblem::WeakPublicKey);
        }
        Ok((kid, key))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::identity::test_keys::TestKey;

    fn jwks(keys: &[serde_json::Value]) -> String {
        json!({ "keys": keys }).to_string()
    }

    fn parse(keys: &[serde_json::Value]) -> Result<IdentityKeys, InvalidKeys> {
        IdentityKeys::from_jwks(&jwks(keys))
    }

    fn problem(keys: &[serde_json::Value]) -> (usize, KeyProblem) {
        match parse(keys) {
            Err(InvalidKeys::Key { index, problem }) => (index, problem),
            other => panic!("expected a key problem for {keys:?}, got {other:?}"),
        }
    }

    /// RFC 8037 appendix A.2: the public part of the example Ed25519 key.
    const RFC8037_X: &str = "11qYAYKxCrfVS_7TyWQHOg7hcvPapiMlrwIaaPcHURo";

    #[test]
    fn a_standard_ed25519_jwk_set_parses() {
        let keys = parse(&[
            json!({"kty": "OKP", "crv": "Ed25519", "kid": "rfc8037", "x": RFC8037_X}),
            json!({
                "kty": "OKP", "crv": "Ed25519", "kid": "2026-10-b", "alg": "EdDSA", "use": "sig",
                "key_ops": ["verify"], "x": TestKey::new(2).x(), "x5t": "ignored member"
            }),
        ])
        .expect("valid set");
        assert_eq!(keys.key_ids().collect::<Vec<_>>(), ["rfc8037", "2026-10-b"]);
        assert!(keys.get("rfc8037").is_some());
        assert!(keys.get("2026-10-b").is_some());
        assert!(keys.get("2026-10-c").is_none());
        assert_eq!(
            format!("{keys:?}"),
            r#"IdentityKeys { key_ids: ["rfc8037", "2026-10-b"] }"#
        );
    }

    #[test]
    fn keys_that_are_not_ed25519_public_keys_are_rejected() {
        let x = TestKey::new(1).x();
        let base = json!({"kty": "OKP", "crv": "Ed25519", "kid": "k", "x": x});
        let with = |member: &str, value: serde_json::Value| {
            let mut key = base.clone();
            key[member] = value;
            key
        };
        let without = |member: &str| {
            let mut key = base.clone();
            key.as_object_mut().expect("object").remove(member);
            key
        };
        let cases = [
            (with("kty", json!("RSA")), KeyProblem::NotEd25519),
            (with("kty", json!("oct")), KeyProblem::NotEd25519),
            (with("crv", json!("Ed448")), KeyProblem::NotEd25519),
            (with("crv", json!("X25519")), KeyProblem::NotEd25519),
            (without("crv"), KeyProblem::NotEd25519),
            // Any private part, whatever its value.
            (
                with("d", json!("presence-is-enough")),
                KeyProblem::PrivateKey,
            ),
            (with("d", json!(null)), KeyProblem::PrivateKey),
            (without("kid"), KeyProblem::BadKeyId),
            (with("kid", json!("")), KeyProblem::BadKeyId),
            (with("kid", json!("two words")), KeyProblem::BadKeyId),
            (with("kid", json!("k".repeat(65))), KeyProblem::BadKeyId),
            (with("alg", json!("RS256")), KeyProblem::WrongUse),
            (with("alg", json!("HS256")), KeyProblem::WrongUse),
            (with("alg", json!("none")), KeyProblem::WrongUse),
            (with("use", json!("enc")), KeyProblem::WrongUse),
            (with("key_ops", json!(["sign"])), KeyProblem::WrongUse),
            (without("x"), KeyProblem::BadPublicKey),
            (with("x", json!(format!("{x}="))), KeyProblem::BadPublicKey),
            (with("x", json!("AAAA")), KeyProblem::BadPublicKey),
            // `/` belongs to standard base64, not base64url.
            (
                with(
                    "x",
                    json!(format!("/{}", x.chars().skip(1).collect::<String>())),
                ),
                KeyProblem::BadPublicKey,
            ),
            // The identity point: small order, so it would accept forged signatures.
            (
                with("x", json!(URL_SAFE_NO_PAD.encode(identity_point()))),
                KeyProblem::WeakPublicKey,
            ),
        ];
        for (key, expected) in cases {
            assert_eq!(problem(std::slice::from_ref(&key)), (0, expected), "{key}");
        }
    }

    fn identity_point() -> [u8; 32] {
        let mut bytes = [0; 32];
        bytes[0] = 1;
        bytes
    }

    #[test]
    fn a_key_id_may_appear_only_once() {
        let first = json!({"kty": "OKP", "crv": "Ed25519", "kid": "k", "x": TestKey::new(1).x()});
        let second = json!({"kty": "OKP", "crv": "Ed25519", "kid": "k", "x": TestKey::new(2).x()});
        assert_eq!(problem(&[first, second]), (1, KeyProblem::DuplicateKeyId));
    }

    #[test]
    fn the_set_must_hold_one_to_eight_keys() {
        assert_eq!(parse(&[]).err(), Some(InvalidKeys::Empty));
        let nine: Vec<_> = (0..9u8)
            .map(|seed| TestKey::new(seed).jwk(&format!("k{seed}")))
            .collect();
        assert_eq!(parse(&nine).err(), Some(InvalidKeys::TooMany));
        assert!(parse(&nine[..8]).is_ok());
    }

    #[test]
    fn explicit_nulls_are_not_absent_members() {
        for member in ["kty", "crv", "kid", "x", "alg", "use", "key_ops"] {
            let mut key = TestKey::new(1).jwk("k");
            key[member] = json!(null);
            assert!(
                matches!(parse(&[key]), Err(InvalidKeys::NotJwkSet(_))),
                "{member}: null"
            );
        }
    }

    #[test]
    fn duplicate_members_are_rejected() {
        let x = TestKey::new(1).x();
        let text = format!(
            r#"{{"keys":[{{"kty":"OKP","crv":"Ed25519","kid":"k","x":"{x}","x":"{x}"}}]}}"#
        );
        assert!(matches!(
            IdentityKeys::from_jwks(&text),
            Err(InvalidKeys::NotJwkSet(_))
        ));
    }

    #[test]
    fn errors_never_quote_the_configuration() {
        let secretish = "do-not-echo-this-value";
        for text in [
            secretish.to_owned(),
            format!(r#"{{"keys": "{secretish}"}}"#),
            format!(
                r#"{{"keys": [{{"kty": "OKP", "crv": "Ed25519", "kid": 1, "x": "{secretish}"}}]}}"#
            ),
            jwks(&[json!({"kty": "OKP", "crv": "Ed25519", "kid": secretish, "x": secretish})]),
            jwks(&[
                json!({"kty": "OKP", "crv": "Ed25519", "kid": "k", "x": TestKey::new(1).x(), "d": secretish}),
            ]),
        ] {
            let error = IdentityKeys::from_jwks(&text).expect_err("invalid set");
            assert!(!error.to_string().contains(secretish), "{error}");
        }
    }
}
