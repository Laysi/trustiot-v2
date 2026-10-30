//! The role checks every service's handlers share (A2; the permission matrix,
//! `docs/architecture/permissions.md`, Laysi/trustiot-v2#585): what an operation requires of the
//! caller beyond a valid token, decided from the token alone, before a handler reads the body or
//! the database.
//!
//! - [`Requirement`]: the user's own request, a minimum role in the token's tenant, or a platform
//!   administrator's own request.
//! - [`UserIdentity::require`]: the check a handler runs first. A refusal is logged at INFO as
//!   `refused to <action>` with the reason (never the token) and answers [`PermissionDenied`]:
//!   403 with the M1 body `permission_denied` and `cache-control: no-store`, the same response
//!   for every reason, so a caller learns nothing about which check failed. The one exception is
//!   a suspended tenant ([`Denial::TenantSuspended`]), which answers 403 `tenant_suspended`:
//!   the user is meant to learn it, so the app shows the suspension page (checklist B1).
//! - [`UserIdentity::meets`]: the same check without logging or a response, for the second
//!   check a store makes before it opens a transaction.
//!
//! "Signed in" needs no check: a [`UserIdentity`] exists only for a verified token.
//!
//! ```
//! use axum::response::{IntoResponse, Response};
//! use contract::TenantRole;
//! use service_kit::identity::{Requirement, UserIdentity};
//!
//! async fn delete_device(user: UserIdentity) -> Response {
//!     if let Err(refused) = user.require(Requirement::Tenant(TenantRole::Editor), "delete a device") {
//!         return refused.into_response();
//!     }
//!     // ... read the body, open the tenant's transaction ...
//! #   ().into_response()
//! }
//! ```

use axum::http::header::CACHE_CONTROL;
use axum::response::{IntoResponse, Response};
use contract::{ErrorBody, ErrorCode, TenantRole};

use super::UserIdentity;

/// What an operation requires of the caller beyond a valid token; the matrix document's 要求
/// column (`docs/architecture/permissions.md`, section 「要求」).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Requirement {
    /// The user's own request through the ingress: a token another service exchanged for a call
    /// on the user's behalf (one with `act`) is refused. The user's own account (the session,
    /// the preferences).
    OwnRequest,
    /// At least this role in the token's tenant: admin includes editor, which includes viewer.
    /// A token without a tenant, with a lower role, or with a role this build does not know
    /// ([`TenantRole::Unrecognized`], which grants nothing) is refused, and so is a platform
    /// administrator without a role in the tenant (no implicit tenant rights, decision 4 of
    /// Laysi/trustiot-v2#104). A token whose tenant is suspended or deleted (the
    /// `tenant_suspended` claim, Laysi/trustiot-v2#777) is refused whatever its role. Exchanged
    /// tokens are accepted. Requiring [`TenantRole::Unrecognized`] refuses everyone.
    Tenant(TenantRole),
    /// A platform administrator's own request: every other user, every tenant role, and an
    /// exchanged token of a platform administrator are refused (Laysi/trustiot-v2#104,
    /// 2026-10-23).
    OwnPlatformAdmin,
}

/// Why a valid token does not meet a [`Requirement`]. Only the logs see it: every denial but
/// [`Denial::TenantSuspended`] answers the same [`PermissionDenied`] body.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum Denial {
    /// The user has not picked or joined a tenant.
    #[error("the token has no tenant")]
    NoTenant,
    /// The token's tenant is suspended or deleted. Unlike every other denial, the response says
    /// so: 403 `tenant_suspended`.
    #[error("the token's tenant is suspended")]
    TenantSuspended,
    /// The token's role in its tenant is one this build does not know.
    #[error("the token's role in its tenant is not one this build knows")]
    UnrecognizedRole,
    /// The token's role in its tenant does not include the one required.
    #[error("the token's role in its tenant is below the one required")]
    RoleTooLow,
    /// The user is not a platform administrator.
    #[error("the user is not a platform administrator")]
    NotPlatformAdmin,
    /// Another service exchanged the token, and the operation takes the user's own request only.
    #[error("another service exchanged the token")]
    ExchangedToken,
}

impl Denial {
    /// A stable name for logs, for example `role_too_low`.
    #[must_use]
    pub const fn reason(self) -> &'static str {
        match self {
            Self::NoTenant => "no_tenant",
            Self::TenantSuspended => "tenant_suspended",
            Self::UnrecognizedRole => "unrecognized_role",
            Self::RoleTooLow => "role_too_low",
            Self::NotPlatformAdmin => "not_platform_admin",
            Self::ExchangedToken => "exchanged_token",
        }
    }
}

/// The refusal of a valid token that does not meet the operation's [`Requirement`]: as a
/// response, 403 with the M1 body `{"code":"permission_denied","message_key":
/// "error.permission_denied"}` and `cache-control: no-store`, whatever the [`Denial`], except
/// [`Denial::TenantSuspended`]: 403 `{"code":"tenant_suspended","message_key":
/// "error.tenant_suspended"}`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("permission denied: {denial}")]
pub struct PermissionDenied {
    denial: Denial,
}

impl PermissionDenied {
    /// Why the token was refused.
    #[must_use]
    pub const fn denial(self) -> Denial {
        self.denial
    }

    /// The M1 body the refusal answers: `tenant_suspended` for a suspended tenant, otherwise
    /// `permission_denied`; with no detail.
    #[must_use]
    pub fn to_error_body(self) -> ErrorBody {
        match self.denial {
            Denial::TenantSuspended => ErrorBody::new(ErrorCode::TenantSuspended),
            _ => ErrorBody::new(ErrorCode::PermissionDenied),
        }
    }
}

impl From<Denial> for PermissionDenied {
    fn from(denial: Denial) -> Self {
        Self { denial }
    }
}

impl IntoResponse for PermissionDenied {
    /// 403 [`PermissionDenied::to_error_body`], not to be stored.
    fn into_response(self) -> Response {
        let body = self.to_error_body();
        (
            body.http_status(),
            [(CACHE_CONTROL, "no-store")],
            axum::Json(body),
        )
            .into_response()
    }
}

impl UserIdentity {
    /// Whether another service exchanged this token for a call on the user's behalf (it has
    /// `act`, Laysi/trustiot-v2#106), rather than the ingress's cookie check issuing it for the
    /// user's own request.
    #[must_use]
    pub fn is_exchanged(&self) -> bool {
        self.claims.act.is_some()
    }

    /// Whether the token's tenant is suspended or deleted (the `tenant_suspended` claim,
    /// Laysi/trustiot-v2#777). Only the identity service gets such a token: the ingress refuses
    /// every other route of a suspended tenant.
    #[must_use]
    pub fn is_tenant_suspended(&self) -> bool {
        self.claims.tenant.is_some() && self.claims.tenant_suspended
    }

    /// Whether the token meets `requirement`; the [`Denial`] when it does not. Logs nothing:
    /// a handler calls [`UserIdentity::require`], and this is for a store's second check.
    ///
    /// # Errors
    ///
    /// The [`Denial`] when the token does not meet `requirement`.
    pub fn meets(&self, requirement: Requirement) -> Result<(), Denial> {
        match requirement {
            Requirement::OwnRequest => {
                if self.is_exchanged() {
                    return Err(Denial::ExchangedToken);
                }
            }
            Requirement::Tenant(needed) => {
                let grant = self.claims.tenant.ok_or(Denial::NoTenant)?;
                // Before the role: no role acts in a suspended tenant.
                if self.claims.tenant_suspended {
                    return Err(Denial::TenantSuspended);
                }
                if grant.role == TenantRole::Unrecognized {
                    return Err(Denial::UnrecognizedRole);
                }
                if !grant.role.includes(needed) {
                    return Err(Denial::RoleTooLow);
                }
            }
            Requirement::OwnPlatformAdmin => {
                if !self.is_platform_admin() {
                    return Err(Denial::NotPlatformAdmin);
                }
                if self.is_exchanged() {
                    return Err(Denial::ExchangedToken);
                }
            }
        }
        Ok(())
    }

    /// Checks `requirement` before the handler does anything else. A refusal is logged at INFO
    /// as `refused to <action>` (`action` names what the request asked for, for example `delete
    /// a device`), with the user, the token's tenant, the reason and the service that exchanged
    /// the token, if any; never the token.
    ///
    /// # Errors
    ///
    /// [`PermissionDenied`] when the token does not meet `requirement`; answer it as it is.
    pub fn require(
        &self,
        requirement: Requirement,
        action: &'static str,
    ) -> Result<(), PermissionDenied> {
        self.meets(requirement).map_err(|denial| {
            tracing::info!(
                user_id = %self.user_id(),
                tenant_id = self.tenant_id().map(|tenant| tenant.to_string()).as_deref(),
                action,
                reason = denial.reason(),
                exchanged = self.is_exchanged(),
                actor = self.claims.act.map(|actor| actor.sub.as_str()),
                "refused to {action}"
            );
            PermissionDenied::from(denial)
        })
    }
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use contract::ServiceName;
    use serde_json::{Value, json};

    use super::super::IdentityVerifier;
    use super::super::test_keys::{NOW, TestKey, claims_at, keys};
    use super::*;

    /// The identity of `claims`, verified as the identity service signs them.
    fn user(claims: &Value) -> UserIdentity {
        let signer = TestKey::new(1);
        IdentityVerifier::new(ServiceName::Assets, keys(&[(&signer, "k1")]))
            .verify_at(&signer.sign("k1", claims), NOW)
            .expect("valid token")
    }

    /// Claims with this tenant role (`None`: no tenant), platform administrator flag and `act`.
    fn claims(role: Option<&str>, platform_admin: bool, act: Option<&str>) -> Value {
        let mut claims = claims_at(NOW);
        match role {
            Some(role) => claims["tenant"]["role"] = json!(role),
            None => {
                claims.as_object_mut().expect("an object").remove("tenant");
            }
        }
        claims["platform_admin"] = json!(platform_admin);
        if let Some(act) = act {
            claims["act"] = json!({"sub": act});
        }
        claims
    }

    const ROLES: [(&str, TenantRole); 3] = [
        ("viewer", TenantRole::Viewer),
        ("editor", TenantRole::Editor),
        ("admin", TenantRole::Admin),
    ];

    #[test]
    fn a_tenant_role_admits_that_role_and_the_ones_above() {
        for (index, (_, needed)) in ROLES.iter().enumerate() {
            for (held_index, (held, _)) in ROLES.iter().enumerate() {
                let result =
                    user(&claims(Some(held), false, None)).meets(Requirement::Tenant(*needed));
                if held_index >= index {
                    assert_eq!(result, Ok(()), "{held} for {needed:?}");
                } else {
                    assert_eq!(result, Err(Denial::RoleTooLow), "{held} for {needed:?}");
                }
                // Exchanged tokens act with the user's role.
                let exchanged = user(&claims(Some(held), false, Some("assistant")));
                assert_eq!(
                    exchanged.meets(Requirement::Tenant(*needed)).is_ok(),
                    held_index >= index,
                    "{held} exchanged for {needed:?}"
                );
            }
        }
    }

    #[test]
    fn an_unrecognized_role_no_tenant_or_a_platform_administrator_gets_no_tenant_rights() {
        for (_, needed) in ROLES {
            let requirement = Requirement::Tenant(needed);
            // A role a newer identity service could issue; serde reads it as Unrecognized.
            let unrecognized = user(&claims(Some("owner"), true, None));
            assert_eq!(
                unrecognized.tenant().map(|grant| grant.role),
                Some(TenantRole::Unrecognized)
            );
            assert_eq!(
                unrecognized.meets(requirement),
                Err(Denial::UnrecognizedRole)
            );
            assert_eq!(
                user(&claims(None, false, None)).meets(requirement),
                Err(Denial::NoTenant)
            );
            assert_eq!(
                user(&claims(None, true, None)).meets(requirement),
                Err(Denial::NoTenant),
                "a platform administrator has no implicit tenant rights"
            );
        }
        // Requiring the unknown role refuses even an admin.
        assert_eq!(
            user(&claims(Some("admin"), false, None))
                .meets(Requirement::Tenant(TenantRole::Unrecognized)),
            Err(Denial::RoleTooLow)
        );
    }

    #[test]
    fn a_platform_administrators_own_request_only() {
        let requirement = Requirement::OwnPlatformAdmin;
        assert_eq!(user(&claims(None, true, None)).meets(requirement), Ok(()));
        assert_eq!(
            user(&claims(Some("admin"), true, None)).meets(requirement),
            Ok(()),
            "a platform administrator with a tenant role is still one"
        );
        assert_eq!(
            user(&claims(None, true, Some("assistant"))).meets(requirement),
            Err(Denial::ExchangedToken)
        );
        for (role, _) in ROLES {
            assert_eq!(
                user(&claims(Some(role), false, None)).meets(requirement),
                Err(Denial::NotPlatformAdmin),
                "tenant {role}"
            );
        }
        assert_eq!(
            user(&claims(None, false, None)).meets(requirement),
            Err(Denial::NotPlatformAdmin)
        );
    }

    #[test]
    fn an_own_request_refuses_exchanged_tokens_only() {
        let requirement = Requirement::OwnRequest;
        assert!(!user(&claims(None, false, None)).is_exchanged());
        assert_eq!(user(&claims(None, false, None)).meets(requirement), Ok(()));
        assert_eq!(
            user(&claims(Some("viewer"), true, None)).meets(requirement),
            Ok(())
        );
        let exchanged = user(&claims(Some("admin"), true, Some("assistant")));
        assert!(exchanged.is_exchanged());
        assert_eq!(exchanged.meets(requirement), Err(Denial::ExchangedToken));
    }

    #[tokio::test]
    async fn every_denial_answers_the_same_403() {
        let mut bodies = Vec::new();
        for denial in [
            Denial::NoTenant,
            Denial::UnrecognizedRole,
            Denial::RoleTooLow,
            Denial::NotPlatformAdmin,
            Denial::ExchangedToken,
        ] {
            let response = PermissionDenied::from(denial).into_response();
            assert_eq!(response.status(), StatusCode::FORBIDDEN, "{denial:?}");
            assert_eq!(
                response
                    .headers()
                    .get(CACHE_CONTROL)
                    .map(|value| value.as_bytes()),
                Some(&b"no-store"[..]),
                "{denial:?}"
            );
            let body = axum::body::to_bytes(response.into_body(), 1024)
                .await
                .expect("the body");
            bodies.push(serde_json::from_slice::<Value>(&body).expect("JSON"));
        }
        assert_eq!(
            bodies[0],
            json!({"code": "permission_denied", "message_key": "error.permission_denied"})
        );
        assert!(bodies.iter().all(|body| *body == bodies[0]), "{bodies:?}");
    }

    /// `claims` with the `tenant_suspended` claim set.
    fn suspended(mut claims: Value) -> Value {
        claims["tenant_suspended"] = json!(true);
        claims
    }

    #[test]
    fn a_suspended_tenant_grants_no_role_but_leaves_own_requests_and_platform_admins() {
        for (role, _) in ROLES {
            for act in [None, Some("assistant")] {
                let user = user(&suspended(claims(Some(role), false, act)));
                assert!(user.is_tenant_suspended());
                for (_, required) in ROLES {
                    assert_eq!(
                        user.meets(Requirement::Tenant(required)),
                        Err(Denial::TenantSuspended),
                        "{role} ({act:?}) for {required:?}"
                    );
                }
            }
            // The session, preferences and logout stay usable.
            assert_eq!(
                user(&suspended(claims(Some(role), false, None))).meets(Requirement::OwnRequest),
                Ok(())
            );
            // A platform administrator whose current tenant is suspended can still resume it.
            assert_eq!(
                user(&suspended(claims(Some(role), true, None)))
                    .meets(Requirement::OwnPlatformAdmin),
                Ok(())
            );
        }
        // Without a tenant the claim means nothing (the identity service never signs that).
        let no_tenant = user(&suspended(claims(None, false, None)));
        assert!(!no_tenant.is_tenant_suspended());
        assert_eq!(
            no_tenant.meets(Requirement::Tenant(TenantRole::Viewer)),
            Err(Denial::NoTenant)
        );
        assert!(!user(&claims(Some("admin"), false, None)).is_tenant_suspended());
    }

    #[tokio::test]
    async fn a_suspended_tenant_answers_403_tenant_suspended() {
        let refused = user(&suspended(claims(Some("admin"), false, None)))
            .require(Requirement::Tenant(TenantRole::Viewer), "read a device")
            .expect_err("a suspended tenant is refused");
        assert_eq!(refused.denial(), Denial::TenantSuspended);
        assert_eq!(refused.denial().reason(), "tenant_suspended");
        let response = refused.into_response();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            response
                .headers()
                .get(CACHE_CONTROL)
                .map(|value| value.as_bytes()),
            Some(&b"no-store"[..])
        );
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .expect("the body");
        assert_eq!(
            serde_json::from_slice::<Value>(&body).expect("JSON"),
            json!({"code": "tenant_suspended", "message_key": "error.tenant_suspended"})
        );
    }

    #[test]
    fn require_reports_the_denial_and_passes_what_meets_it() {
        let viewer = user(&claims(Some("viewer"), false, None));
        assert_eq!(
            viewer.require(Requirement::Tenant(TenantRole::Viewer), "read a device"),
            Ok(())
        );
        let refused = viewer
            .require(Requirement::Tenant(TenantRole::Editor), "delete a device")
            .expect_err("a viewer cannot delete");
        assert_eq!(refused.denial(), Denial::RoleTooLow);
        assert_eq!(refused.denial().reason(), "role_too_low");
    }
}
