//! Internal identity tokens for tests: a stand-in for the identity service (decision 27).
//!
//! A [`TestIssuer`] has an Ed25519 key made for the test process, so nothing outside the test
//! ever trusts it, and no key material is stored in the repository. Write [`TestIssuer::jwks`] to
//! a file ([`TestIssuer::write_jwks`]) and give the service under test its path as
//! `TRUSTIOT_INTERNAL_IDENTITY_KEYS_FILE`, as the chart's ConfigMap volume does, then sign a
//! token per request with [`TestIssuer::token`]. The tokens follow the format service-kit verifies (see its README,
//! "token 格式"); signing real tokens is the identity service's job (Laysi/trustiot-v2#61).

use std::fmt;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use contract::{InternalClaims, ServiceName, TenantGrant, TenantId, TenantRole, UserId};
use ed25519_dalek::{Signer, SigningKey};
use serde_json::json;
use service_kit::identity::{IdentityKeys, IdentityVerifier, UserIdentity};

/// How long the tokens are valid, as the identity service signs them.
pub const TOKEN_LIFETIME_SECS: i64 = 60;

/// Signs internal identity tokens with a key made for this test process.
pub struct TestIssuer {
    key: SigningKey,
    kid: String,
}

impl TestIssuer {
    /// A new issuer with a random key.
    #[must_use]
    pub fn new() -> Self {
        let mut seed = [0_u8; 32];
        seed[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
        seed[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
        Self {
            key: SigningKey::from_bytes(&seed),
            kid: format!("testkit-{}", uuid::Uuid::new_v4().simple()),
        }
    }

    /// The public key as a JWK Set, the content of the file `TRUSTIOT_INTERNAL_IDENTITY_KEYS_FILE`
    /// names.
    #[must_use]
    pub fn jwks(&self) -> String {
        let x = URL_SAFE_NO_PAD.encode(self.key.verifying_key().as_bytes());
        json!({"keys": [{"kty": "OKP", "crv": "Ed25519", "kid": self.kid, "x": x}]}).to_string()
    }

    /// Writes [`TestIssuer::jwks`] to `path`, the file to name in
    /// `TRUSTIOT_INTERNAL_IDENTITY_KEYS_FILE`.
    ///
    /// # Errors
    ///
    /// When the file cannot be written.
    pub fn write_jwks(&self, path: &Path) -> std::io::Result<()> {
        std::fs::write(path, self.jwks())
    }

    /// The public key, parsed as a service parses its configuration.
    ///
    /// # Panics
    ///
    /// Never: the key set is valid by construction.
    #[must_use]
    pub fn keys(&self) -> IdentityKeys {
        IdentityKeys::from_jwks(&self.jwks()).expect("the test key set is valid")
    }

    /// A verifier for tokens addressed to `audience`, as that service would make it.
    #[must_use]
    pub fn verifier(&self, audience: ServiceName) -> IdentityVerifier {
        IdentityVerifier::new(audience, self.keys())
    }

    /// Claims for a new user acting in `tenant` (none: the user has not picked a tenant),
    /// addressed to `audience`, issued now, valid for [`TOKEN_LIFETIME_SECS`].
    #[must_use]
    pub fn claims(audience: ServiceName, tenant: Option<TenantGrant>) -> InternalClaims {
        let iat = unix_now();
        InternalClaims {
            iss: ServiceName::Identity,
            sub: UserId::generate(),
            aud: audience,
            iat,
            exp: iat + TOKEN_LIFETIME_SECS,
            tenant,
            platform_admin: false,
            act: None,
        }
    }

    /// The signed token for `claims`, for the `x-internal-identity` header.
    ///
    /// # Panics
    ///
    /// Never: claims always serialize.
    #[must_use]
    pub fn token(&self, claims: &InternalClaims) -> String {
        let header = json!({"alg": "EdDSA", "typ": "internal-identity+jwt", "kid": self.kid});
        let claims = serde_json::to_string(claims).expect("claims serialize");
        let signed = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(header.to_string()),
            URL_SAFE_NO_PAD.encode(claims)
        );
        let signature = self.key.sign(signed.as_bytes());
        format!("{signed}.{}", URL_SAFE_NO_PAD.encode(signature.to_bytes()))
    }

    /// A token for a new user with `role` in `tenant`, addressed to `audience`.
    #[must_use]
    pub fn tenant_token(
        &self,
        audience: ServiceName,
        tenant: TenantId,
        role: TenantRole,
    ) -> String {
        let grant = TenantGrant { id: tenant, role };
        self.token(&Self::claims(audience, Some(grant)))
    }

    /// The verified identity a handler of `audience` would receive for `claims`.
    ///
    /// # Panics
    ///
    /// When the claims do not verify, for example because they are addressed elsewhere.
    #[must_use]
    pub fn user(&self, audience: ServiceName, claims: &InternalClaims) -> UserIdentity {
        self.verifier(audience)
            .verify(&self.token(claims))
            .unwrap_or_else(|error| panic!("the test token does not verify: {error}"))
    }
}

impl Default for TestIssuer {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for TestIssuer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TestIssuer")
            .field("kid", &self.kid)
            .finish_non_exhaustive()
    }
}

/// Seconds since the Unix epoch.
fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_secs()).ok())
        .unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_verify_for_their_audience_only() {
        let issuer = TestIssuer::new();
        let tenant = TenantId::generate();
        let token = issuer.tenant_token(ServiceName::Assets, tenant, TenantRole::Admin);
        let user = issuer
            .verifier(ServiceName::Assets)
            .verify(&token)
            .expect("verifies for its audience");
        assert_eq!(user.tenant_id(), Some(tenant));
        assert!(
            issuer.verifier(ServiceName::Alerts).verify(&token).is_err(),
            "a token for assets must not verify for alerts"
        );
    }

    #[test]
    fn another_issuers_tokens_do_not_verify() {
        let issuer = TestIssuer::new();
        let other = TestIssuer::new();
        let token = other.token(&TestIssuer::claims(ServiceName::Assets, None));
        assert!(issuer.verifier(ServiceName::Assets).verify(&token).is_err());
    }

    #[test]
    fn a_user_without_a_tenant_has_none() {
        let issuer = TestIssuer::new();
        let user = issuer.user(
            ServiceName::Assets,
            &TestIssuer::claims(ServiceName::Assets, None),
        );
        assert_eq!(user.tenant_id(), None);
    }
}
