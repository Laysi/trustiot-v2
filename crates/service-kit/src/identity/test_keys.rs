//! Test-only signing. The keys are derived in code from a one-byte seed, so they are throwaway
//! test keys by construction: nothing outside the tests ever trusts them, and no key material
//! is stored in the repository. Signing real tokens is the identity service's job
//! (Laysi/trustiot-v2#61).

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::{Signer, SigningKey};
use serde_json::{Value, json};

use super::{ALGORITHM, IdentityKeys, TOKEN_TYPE};

/// A fixed "current time" for unit tests: 2026-10-10T00:00:00Z.
pub(crate) const NOW: i64 = 1_791_590_400;

/// The user every test token is issued for.
pub(crate) const USER: &str = "01928f6e-7a3c-7d4e-8f00-0123456789ab";

/// The tenant every test token is issued for.
pub(crate) const TENANT: &str = "01928f6e-7a3c-7d4e-8f00-00000000000a";

/// A throwaway Ed25519 key pair for tests.
pub(crate) struct TestKey(SigningKey);

impl TestKey {
    /// The key derived from `seed`; different seeds give unrelated keys.
    pub(crate) fn new(seed: u8) -> Self {
        Self(SigningKey::from_bytes(&[seed; 32]))
    }

    /// The public key, base64url without padding (the JWK `x` member).
    pub(crate) fn x(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.0.verifying_key().as_bytes())
    }

    /// The public JWK under `kid`.
    pub(crate) fn jwk(&self, kid: &str) -> Value {
        json!({"kty": "OKP", "crv": "Ed25519", "kid": kid, "x": self.x()})
    }

    /// A token in the internal identity format, signed under `kid`.
    pub(crate) fn sign(&self, kid: &str, claims: &Value) -> String {
        sign_with_header(self, &header(ALGORITHM, TOKEN_TYPE, kid), claims)
    }

    /// `<header>.<payload>.<signature>` over already encoded segments.
    pub(crate) fn sign_segments(&self, header: &str, payload: &str) -> String {
        let signed = format!("{header}.{payload}");
        let signature = self.0.sign(signed.as_bytes());
        format!("{signed}.{}", URL_SAFE_NO_PAD.encode(signature.to_bytes()))
    }
}

/// Signs `claims` under an arbitrary header.
pub(crate) fn sign_with_header(key: &TestKey, header: &Value, claims: &Value) -> String {
    key.sign_segments(&encode_json(header), &encode_json(claims))
}

/// A JWS header with these parameters.
pub(crate) fn header(alg: &str, typ: &str, kid: &str) -> Value {
    json!({"alg": alg, "typ": typ, "kid": kid})
}

/// The base64url encoding of a JSON value, as a JWS segment.
pub(crate) fn encode_json(value: &Value) -> String {
    URL_SAFE_NO_PAD.encode(value.to_string())
}

/// Claims for a user acting as an editor of a tenant, addressed to `assets`, issued at `iat`
/// and valid for 60 seconds.
pub(crate) fn claims_at(iat: i64) -> Value {
    json!({
        "iss": "identity",
        "sub": USER,
        "aud": "assets",
        "iat": iat,
        "exp": iat + 60,
        "tenant": {"id": TENANT, "role": "editor"},
        "platform_admin": false
    })
}

/// A key set holding these keys under these key ids.
pub(crate) fn keys(entries: &[(&TestKey, &str)]) -> IdentityKeys {
    let keys: Vec<Value> = entries.iter().map(|(key, kid)| key.jwk(kid)).collect();
    IdentityKeys::from_jwks(&json!({ "keys": keys }).to_string()).expect("valid test key set")
}
