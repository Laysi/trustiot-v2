//! Verification of internal identity tokens (decision 27; A4 in
//! `docs/requirements/rebuild-checklist.md`). Requires the `contract` feature.
//!
//! The identity service signs a short-lived token for each request that passes the entry
//! gateway, and every service checks it itself before trusting it: signature, issuer, audience,
//! and lifetime. A service trusts nothing else about the caller.
//!
//! - [`IdentityKeys`]: the identity service's public keys, a JWK Set that every service gets
//!   from the platform setting [`KEYS_SETTING`] (`TRUSTIOT_INTERNAL_IDENTITY_KEYS`).
//! - [`IdentityVerifier`]: checks tokens addressed to one service.
//! - [`UserIdentity`]: the axum extractor for the user a request acts for. Only a verified token
//!   produces one, so work that does not run inside a request cannot act as a user.
//! - [`ServiceIdentity`]: the service acting on its own behalf, for background work and event
//!   consumers.
//! - [`IdentityError`]: why a token was rejected; as a response, 401 with the M1 body
//!   `unauthenticated`.
//!
//! # Token format
//!
//! A JWS in compact serialization (RFC 7515) whose payload is
//! [`InternalClaims`](contract::InternalClaims) as JSON:
//!
//! - protected header: exactly `{"alg": "EdDSA", "typ": "internal-identity+jwt", "kid": "<key
//!   id>"}` ([`ALGORITHM`], [`TOKEN_TYPE`]); any other header parameter is rejected;
//! - signature: Ed25519 (RFC 8037) with the key named by `kid`, verified strictly (no
//!   malleable signatures, no small-order keys);
//! - carried only in the `x-internal-identity` header
//!   ([`INTERNAL_IDENTITY_HEADER`](contract::INTERNAL_IDENTITY_HEADER)), exactly once, at most
//!   [`InternalToken::MAX_LEN`](contract::InternalToken::MAX_LEN) bytes; never in
//!   `Authorization`, a cookie, or the query.
//!
//! The claims must have `iss` `identity`, `aud` the verifying service, `exp - iat` between 1 and
//! [`MAX_LIFETIME_SECS`] seconds, and be current: `iat` (and `nbf` when present) no later than
//! now plus [`LEEWAY_SECS`], and now before `exp` plus [`LEEWAY_SECS`].
//!
//! ```
//! use axum::Router;
//! use axum::routing::get;
//! use contract::ServiceName;
//! use service_kit::identity::{IdentityKeys, IdentityVerifier, UserIdentity};
//!
//! async fn whoami(user: UserIdentity) -> String {
//!     user.user_id().to_string()
//! }
//!
//! fn routes(keys: IdentityKeys) -> Router {
//!     // A real service's state holds the verifier next to its other state and implements
//!     // `FromRef<State> for IdentityVerifier`.
//!     Router::new()
//!         .route("/v1/whoami", get(whoami))
//!         .with_state(IdentityVerifier::new(ServiceName::Assets, keys))
//! }
//! ```

mod keys;
#[cfg(test)]
mod test_keys;
mod token;

use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::{FromRef, FromRequestParts};
use axum::http::HeaderMap;
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use contract::{
    ErrorBody, ErrorCode, INTERNAL_IDENTITY_HEADER, InternalClaims, InternalToken, ServiceName,
    TenantGrant, TenantId, UserId,
};

pub use self::keys::{IdentityKeys, InvalidKeys, KeyProblem, MAX_KEY_ID_LEN, MAX_KEYS};

/// The only JWS `alg` accepted: EdDSA (RFC 8037), with Ed25519 keys only.
pub const ALGORITHM: &str = "EdDSA";

/// The JWS `typ` every token carries (explicit typing, RFC 8725 section 3.11), so no other JWT
/// signed with the same key can pass for an internal identity. Compared case-insensitively.
pub const TOKEN_TYPE: &str = "internal-identity+jwt";

/// Clock skew tolerated between the identity service and a verifier, in seconds: a token is
/// accepted until `exp + LEEWAY_SECS` and from `iat - LEEWAY_SECS`. Nodes run NTP, so skew is far
/// below this; much more skew makes every token fail as expired or not yet valid.
pub const LEEWAY_SECS: i64 = 5;

/// Longest lifetime (`exp - iat`) accepted, in seconds. The identity service issues tokens that
/// live far shorter (60 seconds in the 2026-10-10 PoC); the cap bounds what a leaked token is
/// worth and how long key rotation must wait.
pub const MAX_LIFETIME_SECS: i64 = 300;

/// The platform setting with the identity service's public keys: `platform.internal_identity_keys`
/// in a service's configuration, set by the deployment as `TRUSTIOT_INTERNAL_IDENTITY_KEYS`.
/// Platform settings cannot be overridden per service, so no service can be pointed at other
/// keys.
pub const KEYS_SETTING: &str = "internal_identity_keys";

/// Verifies internal identity tokens addressed to one service. Build it once at startup and put
/// it in the router's state; cloning is cheap.
#[derive(Clone, Debug)]
pub struct IdentityVerifier {
    audience: ServiceName,
    keys: IdentityKeys,
}

impl IdentityVerifier {
    /// A verifier that accepts tokens addressed to `audience`, the service itself, signed with
    /// one of `keys`.
    #[must_use]
    pub fn new(audience: ServiceName, keys: IdentityKeys) -> Self {
        Self { audience, keys }
    }

    /// The service tokens must be addressed to.
    #[must_use]
    pub fn audience(&self) -> ServiceName {
        self.audience
    }

    /// The service's own identity, for work it does on its own behalf.
    #[must_use]
    pub fn service_identity(&self) -> ServiceIdentity {
        ServiceIdentity::new(self.audience)
    }

    /// Verifies the token in a request's [`INTERNAL_IDENTITY_HEADER`]. The header must appear
    /// exactly once; no other header is consulted.
    ///
    /// # Errors
    ///
    /// When the header is missing, repeated, or not a valid token for this service now.
    pub fn verify_headers(&self, headers: &HeaderMap) -> Result<UserIdentity, IdentityError> {
        let mut values = headers.get_all(INTERNAL_IDENTITY_HEADER).into_iter();
        let value = values.next().ok_or(IdentityError::Missing)?;
        if values.next().is_some() {
            return Err(IdentityError::Repeated);
        }
        if value.is_empty() {
            return Err(IdentityError::Missing);
        }
        if value.len() > InternalToken::MAX_LEN {
            return Err(IdentityError::Oversized);
        }
        let token = value
            .to_str()
            .ok()
            .and_then(|text| InternalToken::new(text).ok())
            .ok_or(IdentityError::Malformed)?;
        self.verify(token.expose())
    }

    /// Verifies one token against the current time.
    ///
    /// # Errors
    ///
    /// When the token is not valid for this service now; see the module documentation.
    pub fn verify(&self, token: &str) -> Result<UserIdentity, IdentityError> {
        self.verify_at(token, unix_now())
    }

    fn verify_at(&self, token: &str, now: i64) -> Result<UserIdentity, IdentityError> {
        token::verify(token, &self.keys, self.audience, now).map(|claims| UserIdentity { claims })
    }
}

/// For an optional member that, when present, must hold a `T`: use with
/// `#[serde(default, deserialize_with = "present")]`. A plain `Option<T>` reads an explicit
/// `null` as absent, which would let a token or key say `"nbf": null` or `"alg": null` and skip
/// the check that the member triggers.
fn present<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

/// Seconds since the Unix epoch. A clock before the epoch reads as the far future, so every
/// token is rejected as expired rather than accepted.
fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_secs()).ok())
        .unwrap_or(i64::MAX)
}

/// The verified identity of the user a request acts for: what the identity service signed for
/// this request, and nothing else.
///
/// Only [`IdentityVerifier`] creates one, from a token that passed every check, and the axum
/// extractor is how handlers get it. There is no other constructor, no `Default`, no
/// `Deserialize`, and no conversion from [`ServiceIdentity`], so background work and event
/// consumers, which have no request, cannot obtain one: they act as [`ServiceIdentity`].
///
/// ```compile_fail,E0451
/// # fn forge(claims: contract::InternalClaims) {
/// // The fields are private: claims cannot be turned into a user identity.
/// let user = service_kit::identity::UserIdentity { claims };
/// # }
/// ```
///
/// ```compile_fail,E0277
/// # fn forge(service: service_kit::identity::ServiceIdentity) {
/// // A service's own identity never becomes a user's.
/// let user: service_kit::identity::UserIdentity = service.into();
/// # }
/// ```
///
/// ```compile_fail,E0277
/// # fn forge(claims: contract::InternalClaims) {
/// let user = service_kit::identity::UserIdentity::from(claims);
/// # }
/// ```
///
/// ```compile_fail,E0277
/// // Not deserializable: an identity cannot be read back from a queue or a database row.
/// let user: service_kit::identity::UserIdentity = serde_json::from_str("{}").unwrap();
/// ```
///
/// ```compile_fail,E0599
/// let user = service_kit::identity::UserIdentity::default();
/// ```
#[derive(Debug)]
pub struct UserIdentity {
    claims: InternalClaims,
}

impl UserIdentity {
    /// The user the request acts for.
    #[must_use]
    pub fn user_id(&self) -> UserId {
        self.claims.sub
    }

    /// The tenant the user acts in and their role there; `None` until the user has picked or
    /// joined a tenant. Tenant-scoped endpoints reject a request without one.
    #[must_use]
    pub fn tenant(&self) -> Option<TenantGrant> {
        self.claims.tenant
    }

    /// The tenant the user acts in, if any.
    #[must_use]
    pub fn tenant_id(&self) -> Option<TenantId> {
        self.claims.tenant_id()
    }

    /// Whether the user is a platform administrator (A2).
    #[must_use]
    pub fn is_platform_admin(&self) -> bool {
        self.claims.platform_admin
    }

    /// All verified claims.
    #[must_use]
    pub fn claims(&self) -> &InternalClaims {
        &self.claims
    }
}

impl<S> FromRequestParts<S> for UserIdentity
where
    IdentityVerifier: FromRef<S>,
    S: Send + Sync,
{
    type Rejection = IdentityError;

    /// Verifies the request's token with the state's [`IdentityVerifier`]. A rejection is logged
    /// at WARN with its `reason` (never the token) and answers 401 `unauthenticated`.
    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        IdentityVerifier::from_ref(state)
            .verify_headers(&parts.headers)
            .inspect_err(|error| {
                tracing::warn!(
                    reason = error.reason(),
                    "rejected the request's internal identity"
                );
            })
    }
}

/// A service acting on its own behalf, not for any user: background work, scheduled jobs, and
/// event consumers. It grants no user's permissions and cannot become a [`UserIdentity`].
/// How it authorizes calls to other services is Laysi/trustiot-v2#106.
///
/// ```
/// use contract::ServiceName;
/// use service_kit::Context;
/// use service_kit::config::NoConfig;
/// use service_kit::identity::ServiceIdentity;
///
/// async fn purge_expired(actor: ServiceIdentity) {
///     assert_eq!(actor.service(), ServiceName::Assets);
/// }
///
/// fn start_jobs(context: &Context<NoConfig>) {
///     // Background work gets the service's identity; there is no request to take a user's from.
///     context.spawn(purge_expired(ServiceIdentity::new(ServiceName::Assets)));
/// }
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ServiceIdentity {
    service: ServiceName,
}

impl ServiceIdentity {
    /// `service` acting on its own behalf.
    #[must_use]
    pub const fn new(service: ServiceName) -> Self {
        Self { service }
    }

    /// The service.
    #[must_use]
    pub const fn service(self) -> ServiceName {
        self.service
    }
}

/// Why a request's internal identity was rejected. Every reason answers the same response, 401
/// with the M1 body `unauthenticated`, so a caller learns nothing about which check failed;
/// [`IdentityError::reason`] tells the logs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum IdentityError {
    /// The request has no `x-internal-identity` header, or an empty one.
    #[error("the request has no internal identity token")]
    Missing,
    /// The request has the header more than once.
    #[error("the request has more than one internal identity header")]
    Repeated,
    /// The token is longer than [`InternalToken::MAX_LEN`] bytes.
    #[error("the internal identity token is too long")]
    Oversized,
    /// The token is not three base64url segments, has a character a token cannot have, or its
    /// signature is not 64 bytes.
    #[error("the internal identity token is malformed")]
    Malformed,
    /// The header is not exactly `alg`, `typ` and `kid` as JSON strings.
    #[error("the internal identity token's header is malformed")]
    MalformedHeader,
    /// `alg` is not [`ALGORITHM`].
    #[error("the internal identity token uses an algorithm other than EdDSA")]
    UnsupportedAlgorithm,
    /// `typ` is not [`TOKEN_TYPE`].
    #[error("the token is not an internal identity token")]
    WrongType,
    /// `kid` names no configured key.
    #[error("the internal identity token is signed with an unknown key")]
    UnknownKey,
    /// The signature does not verify with the named key.
    #[error("the internal identity token's signature is invalid")]
    BadSignature,
    /// The signed payload is not [`InternalClaims`] as JSON.
    #[error("the internal identity token's claims are malformed")]
    MalformedClaims,
    /// `iss` is not the identity service.
    #[error("the internal identity token was not issued by the identity service")]
    WrongIssuer,
    /// `aud` is another service.
    #[error("the internal identity token is addressed to another service")]
    WrongAudience,
    /// `exp - iat` is not between 1 and [`MAX_LIFETIME_SECS`] seconds.
    #[error("the internal identity token's lifetime is not acceptable")]
    InvalidLifetime,
    /// Now is at or after `exp` plus the leeway.
    #[error("the internal identity token has expired")]
    Expired,
    /// `iat` or `nbf` is later than now plus the leeway.
    #[error("the internal identity token is not valid yet")]
    NotYetValid,
}

impl IdentityError {
    /// A stable name for logs and metrics, for example `expired`.
    #[must_use]
    pub const fn reason(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Repeated => "repeated",
            Self::Oversized => "oversized",
            Self::Malformed => "malformed",
            Self::MalformedHeader => "malformed_header",
            Self::UnsupportedAlgorithm => "unsupported_algorithm",
            Self::WrongType => "wrong_type",
            Self::UnknownKey => "unknown_key",
            Self::BadSignature => "bad_signature",
            Self::MalformedClaims => "malformed_claims",
            Self::WrongIssuer => "wrong_issuer",
            Self::WrongAudience => "wrong_audience",
            Self::InvalidLifetime => "invalid_lifetime",
            Self::Expired => "expired",
            Self::NotYetValid => "not_yet_valid",
        }
    }

    /// The M1 body every rejection answers: `unauthenticated`, with no detail.
    #[must_use]
    pub fn to_error_body(self) -> ErrorBody {
        ErrorBody::new(ErrorCode::Unauthenticated)
    }
}

impl IntoResponse for IdentityError {
    /// 401 with the M1 body `{"code":"unauthenticated","message_key":"error.unauthenticated"}`.
    fn into_response(self) -> Response {
        let body = self.to_error_body();
        (body.http_status(), axum::Json(body)).into_response()
    }
}

#[cfg(test)]
mod tests {
    use axum::http::{HeaderValue, StatusCode};
    use serde_json::json;

    use super::test_keys::{NOW, TENANT, TestKey, USER, claims_at, keys};
    use super::*;

    fn verifier(entries: &[(&TestKey, &str)]) -> IdentityVerifier {
        IdentityVerifier::new(ServiceName::Assets, keys(entries))
    }

    fn headers(values: &[&str]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for value in values {
            headers.append(
                INTERNAL_IDENTITY_HEADER,
                HeaderValue::from_str(value).expect("header value"),
            );
        }
        headers
    }

    #[test]
    fn a_verified_identity_exposes_the_signed_claims() {
        let signer = TestKey::new(1);
        let verifier = verifier(&[(&signer, "k1")]);
        let mut claims = claims_at(NOW);
        claims["platform_admin"] = json!(true);
        let user = verifier
            .verify_at(&signer.sign("k1", &claims), NOW)
            .expect("valid token");
        assert_eq!(user.user_id().to_string(), USER);
        assert_eq!(
            user.tenant_id().map(|id| id.to_string()).as_deref(),
            Some(TENANT)
        );
        assert_eq!(
            user.tenant().map(|grant| grant.role),
            Some(contract::TenantRole::Editor)
        );
        assert!(user.is_platform_admin());
        assert_eq!(user.claims().aud, ServiceName::Assets);
        assert_eq!(verifier.audience(), ServiceName::Assets);
        assert_eq!(
            verifier.service_identity(),
            ServiceIdentity::new(ServiceName::Assets)
        );
        assert_eq!(verifier.service_identity().service(), ServiceName::Assets);
    }

    #[test]
    fn verify_uses_the_current_time() {
        let signer = TestKey::new(1);
        let verifier = verifier(&[(&signer, "k1")]);
        let now = unix_now();
        assert!(verifier.verify(&signer.sign("k1", &claims_at(now))).is_ok());
        assert_eq!(
            verifier
                .verify(&signer.sign("k1", &claims_at(now - 3600)))
                .err(),
            Some(IdentityError::Expired)
        );
    }

    /// Acceptance: a token signed under a new key id still verifies while the keys rotate, and
    /// the order of the rotation steps matters.
    #[test]
    fn tokens_verify_throughout_a_key_rotation() {
        let old = TestKey::new(1);
        let new = TestKey::new(2);
        let claims = claims_at(NOW);
        let old_token = old.sign("2026-10-a", &claims);
        let new_token = new.sign("2026-11-a", &claims);

        // Before: only the old key is configured and the identity service signs with it.
        let before = verifier(&[(&old, "2026-10-a")]);
        assert!(before.verify_at(&old_token, NOW).is_ok());
        // Signing with the new key before every service has it would be rejected.
        assert_eq!(
            before.verify_at(&new_token, NOW).err(),
            Some(IdentityError::UnknownKey)
        );

        // Step 1: every service gets both keys; tokens from either verify.
        let during = verifier(&[(&old, "2026-10-a"), (&new, "2026-11-a")]);
        assert!(during.verify_at(&old_token, NOW).is_ok());
        assert!(during.verify_at(&new_token, NOW).is_ok());
        // The key id binds the key: the old key cannot sign under the new id.
        assert_eq!(
            during.verify_at(&old.sign("2026-11-a", &claims), NOW).err(),
            Some(IdentityError::BadSignature)
        );

        // Step 3: once old tokens have expired, the old key is removed.
        let after = verifier(&[(&new, "2026-11-a")]);
        assert!(after.verify_at(&new_token, NOW).is_ok());
        assert_eq!(
            after.verify_at(&old_token, NOW).err(),
            Some(IdentityError::UnknownKey)
        );
    }

    #[test]
    fn the_token_comes_from_exactly_one_identity_header() {
        let signer = TestKey::new(1);
        let verifier = verifier(&[(&signer, "k1")]);
        let token = signer.sign("k1", &claims_at(unix_now()));
        assert!(verifier.verify_headers(&headers(&[&token])).is_ok());

        assert_eq!(
            verifier.verify_headers(&HeaderMap::new()).err(),
            Some(IdentityError::Missing)
        );
        assert_eq!(
            verifier.verify_headers(&headers(&[""])).err(),
            Some(IdentityError::Missing)
        );
        assert_eq!(
            verifier.verify_headers(&headers(&[&token, &token])).err(),
            Some(IdentityError::Repeated)
        );
        assert_eq!(
            verifier
                .verify_headers(&headers(&[&format!("Bearer {token}")]))
                .err(),
            Some(IdentityError::Malformed)
        );
        let mut non_ascii = HeaderMap::new();
        non_ascii.insert(
            INTERNAL_IDENTITY_HEADER,
            HeaderValue::from_bytes(b"caf\xe9").expect("opaque header value"),
        );
        assert_eq!(
            verifier.verify_headers(&non_ascii).err(),
            Some(IdentityError::Malformed)
        );
        let oversized = "a".repeat(InternalToken::MAX_LEN + 1);
        assert_eq!(
            verifier.verify_headers(&headers(&[&oversized])).err(),
            Some(IdentityError::Oversized)
        );

        // A valid token anywhere else does not count.
        let mut elsewhere = HeaderMap::new();
        for name in ["authorization", "x-internal-identity-token", "cookie"] {
            elsewhere.insert(
                name,
                HeaderValue::from_str(&format!("Bearer {token}")).expect("header value"),
            );
        }
        assert_eq!(
            verifier.verify_headers(&elsewhere).err(),
            Some(IdentityError::Missing)
        );
    }

    #[tokio::test]
    async fn every_rejection_answers_401_unauthenticated() {
        for error in [
            IdentityError::Missing,
            IdentityError::BadSignature,
            IdentityError::Expired,
            IdentityError::WrongAudience,
        ] {
            let response = error.into_response();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
            let body = axum::body::to_bytes(response.into_body(), 4096)
                .await
                .expect("body");
            let body: serde_json::Value = serde_json::from_slice(&body).expect("JSON body");
            assert_eq!(
                body,
                json!({"code": "unauthenticated", "message_key": "error.unauthenticated"}),
                "{error:?}"
            );
        }
    }

    #[test]
    fn reasons_are_distinct() {
        let all = [
            IdentityError::Missing,
            IdentityError::Repeated,
            IdentityError::Oversized,
            IdentityError::Malformed,
            IdentityError::MalformedHeader,
            IdentityError::UnsupportedAlgorithm,
            IdentityError::WrongType,
            IdentityError::UnknownKey,
            IdentityError::BadSignature,
            IdentityError::MalformedClaims,
            IdentityError::WrongIssuer,
            IdentityError::WrongAudience,
            IdentityError::InvalidLifetime,
            IdentityError::Expired,
            IdentityError::NotYetValid,
        ];
        let mut reasons: Vec<&str> = all.iter().map(|error| error.reason()).collect();
        reasons.sort_unstable();
        reasons.dedup();
        assert_eq!(reasons.len(), all.len());
    }

    /// A product service's configuration: the keys are a required platform setting.
    #[derive(Debug, serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct ServiceConfig {
        platform: Platform,
    }

    #[derive(Debug, serde::Deserialize)]
    struct Platform {
        internal_identity_keys: IdentityKeys,
    }

    impl crate::config::Validate for ServiceConfig {}

    fn load(vars: &[(&str, &str)]) -> Result<ServiceConfig, crate::config::ConfigError> {
        let vars = vars
            .iter()
            .map(|(name, value)| ((*name).into(), (*value).into()));
        crate::config::load_from::<ServiceConfig, _>("TRUSTIOT", vars).map(|settings| settings.app)
    }

    #[test]
    fn keys_come_from_the_platform_setting_only() {
        let signer = TestKey::new(1);
        let jwks = json!({"keys": [signer.jwk("2026-10-a")]}).to_string();
        let config = load(&[("TRUSTIOT_INTERNAL_IDENTITY_KEYS", &jwks)]).expect("valid keys");
        assert_eq!(
            config
                .platform
                .internal_identity_keys
                .key_ids()
                .collect::<Vec<_>>(),
            ["2026-10-a"]
        );
        assert_eq!(
            crate::config::env_var_for("TRUSTIOT", &format!("platform.{KEYS_SETTING}")),
            "TRUSTIOT_INTERNAL_IDENTITY_KEYS"
        );

        // Required: a service without keys does not start.
        let error = load(&[]).expect_err("keys are required");
        assert!(
            error
                .to_string()
                .contains("TRUSTIOT_INTERNAL_IDENTITY_KEYS"),
            "{error}"
        );

        // A service's own variables cannot replace the deployment's keys.
        let error = load(&[
            ("TRUSTIOT_INTERNAL_IDENTITY_KEYS", &jwks),
            ("TRUSTIOT__PLATFORM__INTERNAL_IDENTITY_KEYS", &jwks),
        ])
        .expect_err("platform settings come only from the deployment");
        assert!(
            error
                .to_string()
                .contains("TRUSTIOT__PLATFORM__INTERNAL_IDENTITY_KEYS"),
            "{error}"
        );

        // An unusable set stops startup; the error names the setting, not the value.
        let private = json!({"keys": [{
            "kty": "OKP", "crv": "Ed25519", "kid": "2026-10-a",
            "x": signer.x(), "d": "presence-is-enough"
        }]})
        .to_string();
        let error = load(&[("TRUSTIOT_INTERNAL_IDENTITY_KEYS", &private)])
            .expect_err("a private key is refused");
        let message = error.to_string();
        assert!(
            message.contains("platform.internal_identity_keys"),
            "{message}"
        );
        assert!(message.contains("private key"), "{message}");
        assert!(!message.contains("presence-is-enough"), "{message}");
        assert!(!message.contains(&signer.x()), "{message}");
    }
}
