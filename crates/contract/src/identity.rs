//! Internal identity (decision 27, A4 in `docs/requirements/rebuild-checklist.md`).
//!
//! For each request that passes the entry gateway, the identity service signs a short-lived
//! token, and every service verifies its signature, audience and expiry itself before trusting
//! it. This module holds what both sides share: the header that carries the token, the claims
//! inside it, and an opaque wrapper for the signed text. Signing (Laysi/trustiot-v2#61) and
//! verification (Laysi/trustiot-v2#52) are not here.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::id::{SessionId, TenantId, UserId};
use crate::service::ServiceName;

/// The request header that carries the signed internal identity token to a service. The entry
/// gateway overwrites it with the identity service's value on routes that need a session and
/// strips it on the listed exceptions (decision 27), so a client can never supply it.
pub const INTERNAL_IDENTITY_HEADER: &str = "x-internal-identity";

/// The claims of an internal identity token. The names follow JWT (RFC 7519) where one exists,
/// so the claims can be a JWT payload as they are.
///
/// Verifiers ignore claims they do not know, so new claims are added as optional ones.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InternalClaims {
    /// Issuer: always [`ServiceName::Identity`].
    pub iss: ServiceName,
    /// Subject: the user the request acts for.
    pub sub: UserId,
    /// Audience: the one service that may accept the token. Every service rejects tokens for
    /// another audience, so a token cannot be replayed against a different service.
    pub aud: ServiceName,
    /// Issued at, in seconds since the Unix epoch.
    pub iat: i64,
    /// Expiry, in seconds since the Unix epoch. Tokens are short-lived; the identity service
    /// sets the lifetime.
    pub exp: i64,
    /// The tenant the user acts in and their role there; `None` until the user has picked or
    /// joined a tenant. Tenant-scoped endpoints reject a token without one.
    pub tenant: Option<TenantGrant>,
    /// Whether the user is a platform administrator (A2), which is independent of any tenant
    /// role. Absent means `false`.
    #[serde(default)]
    pub platform_admin: bool,
    /// The actor (RFC 8693, section 4.1): the service that got this token from the identity
    /// service's token exchange for a call on the user's behalf. Absent on the tokens the
    /// identity service signs for the ingress gateway's requests. The exchange never exchanges a
    /// token that has one, so a user's identity goes at most one service further than the
    /// service the request was routed to (Laysi/trustiot-v2#106).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub act: Option<Actor>,
    /// The session the ingress gateway's request came with. Only the tokens the ext_authz check
    /// signs carry it (never an exchanged token), so the identity service can bind an agent run's
    /// delegation to that session and refuse the delegation once the session ends
    /// (Laysi/trustiot-v2#551). It names the session and is no credential; other services
    /// ignore it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sid: Option<SessionId>,
}

/// The `act` claim of an exchanged token: who acts for the user.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Actor {
    /// The service that exchanged the token it received, addressed to itself, for this one.
    pub sub: ServiceName,
}

impl InternalClaims {
    /// The tenant the user acts in, if any.
    #[must_use]
    pub fn tenant_id(&self) -> Option<TenantId> {
        self.tenant.map(|grant| grant.id)
    }
}

/// The tenant a user acts in, with their role there.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TenantGrant {
    /// The tenant.
    pub id: TenantId,
    /// The user's role in the tenant.
    pub role: TenantRole,
}

/// A user's role within one tenant (A2). Each role includes the ones before it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TenantRole {
    /// Reads the tenant's data.
    Viewer,
    /// Also changes devices, models, alert rules and other configuration.
    Editor,
    /// Also manages members, the plan, and tenant settings.
    Admin,
    /// A role this build does not know, from a newer identity service during a rolling update.
    /// It grants nothing; the identity service never issues it.
    #[serde(other)]
    Unrecognized,
}

impl TenantRole {
    /// Whether this role grants everything `required` grants: admin includes editor, which
    /// includes viewer. [`TenantRole::Unrecognized`] neither includes nor is included by any role.
    #[must_use]
    pub const fn includes(self, required: Self) -> bool {
        match (self.rank(), required.rank()) {
            (Some(held), Some(needed)) => held >= needed,
            _ => false,
        }
    }

    const fn rank(self) -> Option<u8> {
        match self {
            Self::Viewer => Some(0),
            Self::Editor => Some(1),
            Self::Admin => Some(2),
            Self::Unrecognized => None,
        }
    }
}

/// A signed internal identity token, as carried in [`INTERNAL_IDENTITY_HEADER`]. Its format is
/// up to the signer and the verifier; this type only keeps it out of logs: `Debug` prints
/// `InternalToken(<redacted>)`, and there is no `Display` or `Serialize`.
#[derive(Clone)]
pub struct InternalToken(String);

impl InternalToken {
    /// Longest token accepted, in bytes.
    pub const MAX_LEN: usize = 8192;

    /// Wraps a token.
    ///
    /// # Errors
    ///
    /// When the token is empty, longer than [`InternalToken::MAX_LEN`], or has a character that
    /// is not visible ASCII (so it is always a valid header value).
    pub fn new(token: impl Into<String>) -> Result<Self, InvalidToken> {
        let token = token.into();
        let valid = !token.is_empty()
            && token.len() <= Self::MAX_LEN
            && token.bytes().all(|byte| byte.is_ascii_graphic());
        if valid {
            Ok(Self(token))
        } else {
            Err(InvalidToken)
        }
    }

    /// The token text, for the HTTP header. Never log it.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for InternalToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("InternalToken(<redacted>)")
    }
}

/// The text cannot be an internal identity token.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("internal identity token must be 1 to 8192 visible ASCII characters")]
pub struct InvalidToken;

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn claims() -> InternalClaims {
        InternalClaims {
            iss: ServiceName::Identity,
            sub: "01928f6e-7a3c-7d4e-8f00-0123456789ab"
                .parse()
                .expect("user id"),
            aud: ServiceName::Assets,
            iat: 1_791_590_400,
            exp: 1_791_590_460,
            tenant: Some(TenantGrant {
                id: "01928f6e-7a3c-7d4e-8f00-00000000000a"
                    .parse()
                    .expect("tenant id"),
                role: TenantRole::Editor,
            }),
            platform_admin: false,
            act: None,
            sid: None,
        }
    }

    #[test]
    fn claims_use_jwt_names_and_round_trip() {
        let value = serde_json::to_value(claims()).expect("serialize");
        assert_eq!(
            value,
            json!({
                "iss": "identity",
                "sub": "01928f6e-7a3c-7d4e-8f00-0123456789ab",
                "aud": "assets",
                "iat": 1_791_590_400,
                "exp": 1_791_590_460,
                "tenant": {"id": "01928f6e-7a3c-7d4e-8f00-00000000000a", "role": "editor"},
                "platform_admin": false
            })
        );
        assert_eq!(
            serde_json::from_value::<InternalClaims>(value).expect("deserialize"),
            claims()
        );
    }

    #[test]
    fn claims_tolerate_unknown_claims_and_default_platform_admin() {
        let mut value = serde_json::to_value(claims()).expect("serialize");
        let object = value.as_object_mut().expect("object");
        object.remove("platform_admin");
        object.insert("jti".to_owned(), json!("f81d4fae"));
        let parsed: InternalClaims = serde_json::from_value(value).expect("deserialize");
        assert!(!parsed.platform_admin);
        assert_eq!(parsed.tenant_id(), claims().tenant_id());
    }

    #[test]
    fn the_session_claim_round_trips_and_is_omitted_when_absent() {
        let mut with_session = claims();
        let session: SessionId = "01928f6e-7a3c-7d4e-8f00-00000000005e"
            .parse()
            .expect("session id");
        with_session.sid = Some(session);
        let value = serde_json::to_value(&with_session).expect("serialize");
        assert_eq!(value["sid"], json!("01928f6e-7a3c-7d4e-8f00-00000000005e"));
        assert_eq!(
            serde_json::from_value::<InternalClaims>(value).expect("deserialize"),
            with_session
        );
        // Without a session (an exchanged token, or one from an identity service before
        // Laysi/trustiot-v2#551) the member is absent, and `null` reads as absent.
        let value = serde_json::to_value(claims()).expect("serialize");
        assert!(value.get("sid").is_none(), "{value}");
        let mut explicit_null = value;
        explicit_null["sid"] = json!(null);
        assert_eq!(
            serde_json::from_value::<InternalClaims>(explicit_null)
                .expect("deserialize")
                .sid,
            None
        );
    }

    #[test]
    fn claims_without_a_tenant_have_no_tenant_id() {
        let mut no_tenant = claims();
        no_tenant.tenant = None;
        let value = serde_json::to_value(&no_tenant).expect("serialize");
        assert_eq!(value["tenant"], json!(null));
        assert_eq!(no_tenant.tenant_id(), None);
    }

    #[test]
    fn roles_include_lower_roles_and_unknown_roles_grant_nothing() {
        use TenantRole::{Admin, Editor, Unrecognized, Viewer};
        assert!(Admin.includes(Editor) && Admin.includes(Viewer) && Editor.includes(Viewer));
        assert!(!Viewer.includes(Editor) && !Editor.includes(Admin));
        let unknown: TenantRole = serde_json::from_value(json!("auditor")).expect("deserialize");
        assert_eq!(unknown, Unrecognized);
        for role in [Viewer, Editor, Admin, Unrecognized] {
            assert!(!Unrecognized.includes(role), "{role:?}");
            assert!(!role.includes(Unrecognized), "{role:?}");
        }
    }

    #[test]
    fn tokens_are_header_safe_and_redacted() {
        let token = InternalToken::new("header.payload.signature").expect("token");
        assert_eq!(format!("{token:?}"), "InternalToken(<redacted>)");
        assert_eq!(token.expose(), "header.payload.signature");
        let too_long = "a".repeat(InternalToken::MAX_LEN + 1);
        for bad in [
            "",
            "two words",
            "line\nbreak",
            "caf\u{e9}",
            too_long.as_str(),
        ] {
            assert_eq!(InternalToken::new(bad).err(), Some(InvalidToken), "{bad:?}");
        }
    }
}
