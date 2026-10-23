//! Members (Laysi/trustiot-v2#583; services/identity/README.md, "成員"): a platform administrator
//! sets a user's role in a tenant with `PUT /api/identity/v1/platform/tenants/{tenant_id}/members`,
//! which creates the membership or changes its role. The membership and the
//! `identity.member_role_changed` event are written in one transaction, in the row-level security
//! context of the tenant in the path, so the operation log has the change exactly when it
//! happened (checklist A2, K2).
//!
//! A platform administrator has no access to a tenant's data of their own (A2): this membership is
//! the explicit, logged grant. The API only sets the role of a user who already exists; it never
//! creates one (Laysi/trustiot-v2#105 adds invitations).
//!
//! | Request | Response |
//! | --- | --- |
//! | No valid internal identity token | 401 `unauthenticated` |
//! | Not a platform administrator, or a token another service exchanged (`act`) | 403 `permission_denied` |
//! | A tenant ID that is not a UUID | 400 `invalid_request` |
//! | A `Content-Type` other than `application/json` (parameters such as `charset` allowed) | 400 `invalid_request` |
//! | A body over [`MAX_BODY_BYTES`], or one that is not a JSON object with a string `email` and a string `role` | 400 `invalid_request` |
//! | An `email` that is not an address, a `role` other than `viewer`, `editor` or `admin` | 422 `validation_failed`, fields `/email` (`member.email_invalid`), `/role` (`member.role_invalid`) |
//! | No tenant has the ID | 404 `not_found`; nothing was written |
//! | No user has the email address | 422 `validation_failed`, field `/email`, key `member.email_unknown`; nothing was written, no user was created |
//! | The database failed or refused the transaction | 503 `unavailable`; nothing was written |
//! | The user was not a member | 201 with [`Membership`], `previous_role` null |
//! | The user was a member | 200 with [`Membership`]; no write and no event when the role was already this one |
//!
//! The checks run in that order: the permission is decided from the token alone, before the
//! request's body is read and before anything is read from the database. Every response
//! of the handler (all but the identity extractor's 401) carries `cache-control: no-store`. A
//! change logs one INFO line with the administrator's user
//! ID, the tenant, the member's user ID and both roles; the email address is personal data and
//! is never logged, nor carried by the event.

use std::sync::Arc;

use axum::Router;
use std::time::Duration;

use axum::body::Body;
use axum::extract::rejection::PathRejection;
use axum::extract::{FromRef, Path, State};
use axum::http::header::CACHE_CONTROL;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::put;
use contract::event::Actor;
use contract::events::identity::{
    MEMBERSHIP_AGGREGATE, MemberRoleChanged, membership_aggregate_id,
};
use contract::{ErrorBody, ErrorCode, FieldError, MessageKey, TenantId, TenantRole, UserId};
use outbox::Aggregate;
use serde::{Deserialize, Serialize};
use service_kit::identity::{IdentityVerifier, UserIdentity};
use service_kit::tenant::{BackgroundTenantDb, TenantDbError};
use utoipa::ToSchema;

use crate::login::EmailAddress;
use crate::tenants::{error, is_json};

/// The route, as the ingress gateway forwards it (no prefix is stripped).
pub const PLATFORM_MEMBERS_PATH: &str = "/api/identity/v1/platform/tenants/{tenant_id}/members";

/// The message key of an `email` that is not an address.
pub const EMAIL_INVALID: MessageKey = MessageKey::new("member.email_invalid");

/// The message key of an address no user has.
pub const EMAIL_UNKNOWN: MessageKey = MessageKey::new("member.email_unknown");

/// The message key of a `role` that is not `viewer`, `editor` or `admin`.
pub const ROLE_INVALID: MessageKey = MessageKey::new("member.role_invalid");

/// The largest body read: an address of at most 254 bytes and a role fit many times over. The
/// body is read only after the permission check, so others cannot make the service buffer more.
pub const MAX_BODY_BYTES: usize = 4096;

/// The longest the transaction waits for a row lock (`SET LOCAL lock_timeout` at its start).
/// Requests for one membership run one after another; past this bound the waiting one answers
/// 503 instead of holding one of the tenant pool's few connections.
pub const LOCK_TIMEOUT: Duration = Duration::from_secs(5);

/// Sets [`LOCK_TIMEOUT`] for the rest of the transaction, like `SET LOCAL`.
const SET_LOCK_TIMEOUT: &str = "SELECT set_config('lock_timeout', $1, true)";

/// Names the work in the kit's DEBUG log of the tenant transaction.
const SET_ROLE_PURPOSE: &str = "set-member-role";

/// Whether the transaction's tenant exists; row-level security shows no other tenant's row.
const TENANT_EXISTS: &str =
    "SELECT EXISTS (SELECT 1 FROM identity.tenants WHERE tenant_id = $1::uuid)";

/// The user with the address `$1` (normalized as the login flow stores it).
const FIND_USER: &str = "SELECT id::text FROM identity.users WHERE email = $1";

/// The member's current role, locked until the transaction ends, so two requests for one
/// membership change it one after the other and each event names the role it replaced.
const LOCK_MEMBERSHIP: &str = "SELECT tenant_role FROM identity.memberships
WHERE tenant_id = $1::uuid AND user_id = $2::uuid
FOR UPDATE";

/// A new membership. When a concurrent request inserted it first, this waits for that one to
/// commit and inserts nothing. Row-level security checks that `$1` is the transaction's tenant.
const INSERT_MEMBERSHIP: &str = "INSERT INTO identity.memberships (tenant_id, user_id, tenant_role)
VALUES ($1::uuid, $2::uuid, $3)
ON CONFLICT (tenant_id, user_id) DO NOTHING";

/// A new role for a locked membership.
const UPDATE_ROLE: &str = "UPDATE identity.memberships SET tenant_role = $3
WHERE tenant_id = $1::uuid AND user_id = $2::uuid";

/// A role in a tenant, as this API takes and returns it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum MemberRole {
    /// Reads the tenant's data.
    Viewer,
    /// Also changes devices, models, alert rules and other configuration.
    Editor,
    /// Also manages members, the plan, and tenant settings.
    Admin,
}

impl MemberRole {
    /// The role `text` names exactly (`viewer`, `editor`, `admin`); `None` otherwise.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "viewer" => Some(Self::Viewer),
            "editor" => Some(Self::Editor),
            "admin" => Some(Self::Admin),
            _ => None,
        }
    }

    /// The name, as the API and the `tenant_role` column write it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Viewer => "viewer",
            Self::Editor => "editor",
            Self::Admin => "admin",
        }
    }

    /// The same role in the internal identity and the events.
    #[must_use]
    pub const fn tenant_role(self) -> TenantRole {
        match self {
            Self::Viewer => TenantRole::Viewer,
            Self::Editor => TenantRole::Editor,
            Self::Admin => TenantRole::Admin,
        }
    }
}

/// The body of `PUT /api/identity/v1/platform/tenants/{tenant_id}/members`. Unknown fields are
/// ignored. Its `Debug` leaves the address out: it is personal data.
#[derive(Deserialize, ToSchema)]
pub struct SetMemberRole {
    /// The email address of an existing user, compared after trimming and in ASCII lower case,
    /// as logging in compares it.
    pub email: String,
    /// The role to give the user in the tenant.
    #[schema(value_type = MemberRole)]
    pub role: String,
}

impl std::fmt::Debug for SetMemberRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SetMemberRole")
            .field("email", &"<redacted>")
            .field("role", &self.role)
            .finish()
    }
}

/// A user's membership of a tenant.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Membership {
    /// The tenant.
    pub tenant_id: TenantId,
    /// The member.
    pub user_id: UserId,
    /// The role now.
    pub role: MemberRole,
    /// The role before this request; `null` when the user was not a member. Equal to `role`
    /// when nothing changed.
    pub previous_role: Option<MemberRole>,
}

/// What a request did to the membership.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The user was not a member and now is.
    Created,
    /// The member had another role.
    Changed,
    /// The member already had this role: nothing was written, no event.
    Unchanged,
}

/// Why a role was not set.
#[derive(Debug, thiserror::Error)]
pub enum SetRoleError {
    /// The caller is not a platform administrator, or another service exchanged the token.
    #[error("only a platform administrator's own request may set a member's role")]
    NotAllowed,
    /// No tenant has the ID.
    #[error("no tenant has this ID")]
    UnknownTenant,
    /// No user has the email address.
    #[error("no user has this email address")]
    UnknownUser,
    /// A stored row breaks a rule of its table, or a membership another request inserted
    /// vanished before this one could lock it.
    #[error("inconsistent membership data: {0}")]
    Inconsistent(&'static str),
    /// The transaction could not start, or a statement in it failed.
    #[error(transparent)]
    Database(#[from] TenantDbError),
    /// The event could not be written; the transaction was rolled back.
    #[error(transparent)]
    Publish(#[from] outbox::PublishError),
    /// The event's aggregate is not valid; the kind and the IDs always are, so this does not
    /// happen.
    #[error(transparent)]
    Aggregate(#[from] outbox::InvalidAggregate),
}

impl From<sqlx::Error> for SetRoleError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(TenantDbError::Database(error))
    }
}

impl SetRoleError {
    /// A short description for the log: the SQLSTATE or the kind, never data.
    fn log_kind(&self) -> String {
        match self {
            Self::Database(TenantDbError::Database(error)) => crate::session::error_kind(error),
            Self::Database(error) => error.to_string(),
            // `PublishError` prints only the SQLSTATE or kind, `InvalidAggregate` only the rule.
            Self::Publish(error) => error.to_string(),
            Self::Aggregate(error) => error.to_string(),
            Self::NotAllowed | Self::UnknownTenant | Self::UnknownUser | Self::Inconsistent(_) => {
                self.to_string()
            }
        }
    }
}

/// Whether `user` may act as a platform administrator here: a platform administrator whose
/// token the ingress's cookie check issued. An exchanged token (`act`) means another service
/// calls on the administrator's behalf, and granting access to a tenant is the administrator's
/// own act (as for workstation certificates, services/certs/src/workstations.rs).
#[must_use]
pub fn is_own_platform_admin(user: &UserIdentity) -> bool {
    user.is_platform_admin() && user.claims().act.is_none()
}

/// Sets members' roles: with [`crate::tenants::TenantStore`], the only code in the service that
/// opens a transaction for a tenant a request names but its token does not. It holds the
/// background handle privately, and its only method refuses every caller but a platform
/// administrator's own request before it opens a transaction (services/identity/README.md,
/// "資料庫").
#[derive(Clone, Debug)]
pub struct MemberStore {
    db: BackgroundTenantDb,
}

impl MemberStore {
    /// A store on `db`, the background handle of the tenant store's pool
    /// ([`crate::tenants::TenantStore::members`]).
    pub(crate) fn new(db: BackgroundTenantDb) -> Self {
        Self { db }
    }

    /// Gives the user with `email` the role `role` in `tenant`, for the platform administrator
    /// `admin`: in one transaction of `tenant`, the membership and, unless the role was already
    /// `role`, the `identity.member_role_changed` event naming `admin`.
    ///
    /// # Errors
    ///
    /// [`SetRoleError::NotAllowed`] unless `admin` is a platform administrator's own request
    /// ([`is_own_platform_admin`]), before anything is read; [`SetRoleError::UnknownTenant`],
    /// [`SetRoleError::UnknownUser`]; otherwise when the database fails. Nothing was written
    /// then.
    pub async fn set_role(
        &self,
        admin: &UserIdentity,
        tenant: TenantId,
        email: &EmailAddress,
        role: MemberRole,
    ) -> Result<(Membership, Outcome), SetRoleError> {
        if !is_own_platform_admin(admin) {
            return Err(SetRoleError::NotAllowed);
        }
        let mut tx = self.db.begin(tenant, SET_ROLE_PURPOSE).await?;
        sqlx::query(SET_LOCK_TIMEOUT)
            .bind(format!("{}ms", LOCK_TIMEOUT.as_millis()))
            .execute(&mut *tx)
            .await?;
        let exists: bool = sqlx::query_scalar(TENANT_EXISTS)
            .bind(tenant.to_string())
            .fetch_one(&mut *tx)
            .await?;
        if !exists {
            return Err(SetRoleError::UnknownTenant);
        }
        let user: Option<String> = sqlx::query_scalar(FIND_USER)
            .bind(email.as_str())
            .fetch_optional(&mut *tx)
            .await?;
        let user: UserId = user
            .ok_or(SetRoleError::UnknownUser)?
            .parse()
            .map_err(|_| SetRoleError::Inconsistent("user id"))?;

        let previous = match current_role(&mut tx, tenant, user).await? {
            Some(previous) => Some(previous),
            None => {
                let inserted = sqlx::query(INSERT_MEMBERSHIP)
                    .bind(tenant.to_string())
                    .bind(user.to_string())
                    .bind(role.as_str())
                    .execute(&mut *tx)
                    .await?
                    .rows_affected();
                if inserted == 1 {
                    None
                } else {
                    // A concurrent request created the membership after this one looked; the
                    // insert waited for it to commit, so a new statement sees and locks it.
                    Some(
                        current_role(&mut tx, tenant, user)
                            .await?
                            .ok_or(SetRoleError::Inconsistent("membership vanished"))?,
                    )
                }
            }
        };
        let membership = Membership {
            tenant_id: tenant,
            user_id: user,
            role,
            previous_role: previous,
        };
        let outcome = match previous {
            None => Outcome::Created,
            Some(previous) if previous == role => {
                // Nothing to write: no change, no event.
                tx.rollback().await?;
                return Ok((membership, Outcome::Unchanged));
            }
            Some(_) => {
                sqlx::query(UPDATE_ROLE)
                    .bind(tenant.to_string())
                    .bind(user.to_string())
                    .bind(role.as_str())
                    .execute(&mut *tx)
                    .await?;
                Outcome::Changed
            }
        };
        let aggregate =
            Aggregate::new(MEMBERSHIP_AGGREGATE, membership_aggregate_id(tenant, user))?;
        outbox::publish_as(
            tx.transaction(),
            &aggregate,
            &MemberRoleChanged {
                user_id: user,
                previous_role: previous.map(MemberRole::tenant_role),
                role: role.tenant_role(),
            },
            Some(tenant),
            Actor::User {
                user_id: admin.user_id(),
            },
        )
        .await?;
        tx.commit().await?;
        tracing::info!(
            user_id = %admin.user_id(),
            tenant_id = %tenant,
            member_user_id = %user,
            previous_role = previous.map_or("none", MemberRole::as_str),
            role = role.as_str(),
            "set a member's role"
        );
        Ok((membership, outcome))
    }
}

/// The member's role in the transaction's tenant, locked; `None` when they are not a member.
async fn current_role(
    tx: &mut service_kit::tenant::TenantTransaction,
    tenant: TenantId,
    user: UserId,
) -> Result<Option<MemberRole>, SetRoleError> {
    let role: Option<String> = sqlx::query_scalar(LOCK_MEMBERSHIP)
        .bind(tenant.to_string())
        .bind(user.to_string())
        .fetch_optional(&mut **tx)
        .await?;
    role.map(|role| MemberRole::parse(&role).ok_or(SetRoleError::Inconsistent("membership role")))
        .transpose()
}

/// The state of the member routes.
#[derive(Clone)]
pub struct MemberRoutes {
    verifier: IdentityVerifier,
    store: Arc<MemberStore>,
}

impl FromRef<MemberRoutes> for IdentityVerifier {
    fn from_ref(state: &MemberRoutes) -> Self {
        state.verifier.clone()
    }
}

/// The member routes: [`PLATFORM_MEMBERS_PATH`].
pub fn routes(verifier: IdentityVerifier, store: MemberStore) -> Router {
    Router::new()
        .route(PLATFORM_MEMBERS_PATH, put(set_member_role))
        .with_state(MemberRoutes {
            verifier,
            store: Arc::new(store),
        })
}

/// The checked body: the address and the role, or the field errors.
fn parse_body(request: &SetMemberRole) -> Result<(EmailAddress, MemberRole), ErrorBody> {
    let email = EmailAddress::parse(&request.email);
    let role = MemberRole::parse(&request.role);
    match (email, role) {
        (Some(email), Some(role)) => Ok((email, role)),
        (email, role) => {
            let mut body = ErrorBody::new(ErrorCode::ValidationFailed);
            if email.is_none() {
                body = body.with_field_error(FieldError::new("/email", EMAIL_INVALID));
            }
            if role.is_none() {
                body = body.with_field_error(FieldError::new("/role", ROLE_INVALID));
            }
            Err(body)
        }
    }
}

/// Sets a user's role in a tenant, creating the membership when the user has none there.
/// Platform administrators only, in their own request.
#[utoipa::path(
    put,
    path = "/api/identity/v1/platform/tenants/{tenant_id}/members",
    tag = "identity",
    operation_id = "setPlatformTenantMemberRole",
    params(
        ("tenant_id" = String, Path, description = "The tenant's ID."),
    ),
    request_body = SetMemberRole,
    responses(
        (status = 200, description = "The user was a member: the role is set, and `identity.member_role_changed` written in the same transaction unless the role was already this one (then nothing was written).", body = Membership),
        (status = 201, description = "The user was not a member: the membership was created, and `identity.member_role_changed` written in the same transaction.", body = Membership),
        (status = 400, description = "The tenant ID is not a UUID, the `Content-Type` is not `application/json`, or the body is over 4096 bytes or not a JSON object with a string `email` and a string `role` (`invalid_request`).", body = ErrorBody),
        (status = 401, description = "No valid session (`unauthenticated`).", body = ErrorBody),
        (status = 403, description = "Not a platform administrator, or not the administrator's own request: another service exchanged the token (`permission_denied`).", body = ErrorBody),
        (status = 404, description = "No tenant has the ID; nothing was written (`not_found`).", body = ErrorBody),
        (status = 422, description = "The email is not an address (`/email`, `member.email_invalid`), no user has it (`/email`, `member.email_unknown`; no user is created), or the role is not `viewer`, `editor` or `admin` (`/role`, `member.role_invalid`) (`validation_failed`). Nothing was written.", body = ErrorBody),
        (status = 503, description = "The database failed; nothing was written (`unavailable`).", body = ErrorBody),
    )
)]
async fn set_member_role(
    State(state): State<MemberRoutes>,
    user: UserIdentity,
    tenant: Result<Path<String>, PathRejection>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    // Decided from the token alone, before the body is parsed or the database read.
    if !is_own_platform_admin(&user) {
        tracing::info!(
            user_id = %user.user_id(),
            exchanged = user.claims().act.is_some(),
            "refused to set a member's role: not a platform administrator, or not the \
             administrator's own request"
        );
        return error(ErrorBody::new(ErrorCode::PermissionDenied));
    }
    let Some(tenant) = tenant
        .ok()
        .and_then(|Path(tenant)| tenant.parse::<TenantId>().ok())
    else {
        return error(ErrorBody::new(ErrorCode::InvalidRequest));
    };
    // Checked and parsed by hand so a refusal answers the M1 format, not axum's plain text. The
    // JSON type keeps pages of other origins out (README, "租戶").
    if !is_json(&headers) {
        return error(ErrorBody::new(ErrorCode::InvalidRequest));
    }
    let Ok(body) = axum::body::to_bytes(body, MAX_BODY_BYTES).await else {
        return error(ErrorBody::new(ErrorCode::InvalidRequest));
    };
    let Ok(request) = serde_json::from_slice::<SetMemberRole>(&body) else {
        return error(ErrorBody::new(ErrorCode::InvalidRequest));
    };
    let (email, role) = match parse_body(&request) {
        Ok(parsed) => parsed,
        Err(refused) => return error(refused),
    };
    match state.store.set_role(&user, tenant, &email, role).await {
        Ok((membership, outcome)) => {
            let status = if outcome == Outcome::Created {
                StatusCode::CREATED
            } else {
                StatusCode::OK
            };
            (
                status,
                [(CACHE_CONTROL, "no-store")],
                axum::Json(membership),
            )
                .into_response()
        }
        Err(SetRoleError::NotAllowed) => error(ErrorBody::new(ErrorCode::PermissionDenied)),
        Err(SetRoleError::UnknownTenant) => error(ErrorBody::new(ErrorCode::NotFound)),
        Err(SetRoleError::UnknownUser) => error(
            ErrorBody::new(ErrorCode::ValidationFailed)
                .with_field_error(FieldError::new("/email", EMAIL_UNKNOWN)),
        ),
        Err(failure) => {
            tracing::error!(
                user_id = %user.user_id(),
                tenant_id = %tenant,
                error_kind = failure.log_kind(),
                "could not set a member's role"
            );
            error(ErrorBody::new(ErrorCode::Unavailable))
        }
    }
}

#[cfg(test)]
mod tests {
    use contract::{InternalClaims, ServiceName};
    use contract::{TenantGrant, identity::Actor as TokenActor};
    use testkit::identity::TestIssuer;

    use super::*;

    #[test]
    fn roles_are_exactly_the_three_tenant_roles() {
        for role in [MemberRole::Viewer, MemberRole::Editor, MemberRole::Admin] {
            assert_eq!(MemberRole::parse(role.as_str()), Some(role));
            // The same spelling as the token's and the events' role, and the column's CHECK.
            assert_eq!(
                serde_json::to_value(role.tenant_role()).expect("serialize"),
                serde_json::Value::String(role.as_str().to_owned())
            );
            assert_eq!(
                serde_json::to_value(role).expect("serialize"),
                serde_json::Value::String(role.as_str().to_owned())
            );
        }
        // `unrecognized` is how a newer role arrives in a token; it is never a role to give.
        for text in [
            "",
            "Admin",
            "ADMIN",
            " admin",
            "owner",
            "unrecognized",
            "platform_admin",
        ] {
            assert_eq!(MemberRole::parse(text), None, "{text:?}");
        }
    }

    fn request(email: &str, role: &str) -> SetMemberRole {
        SetMemberRole {
            email: email.to_owned(),
            role: role.to_owned(),
        }
    }

    #[test]
    fn the_body_reports_every_invalid_field() {
        let (email, role) =
            parse_body(&request("  Member@Example.TEST ", "editor")).expect("a valid body");
        assert_eq!(
            email.as_str(),
            "member@example.test",
            "normalized as logging in does"
        );
        assert_eq!(role, MemberRole::Editor);

        let fields = |body: ErrorBody| -> Vec<(String, String)> {
            let value = serde_json::to_value(body).expect("serialize");
            assert_eq!(value["code"], "validation_failed");
            value["field_errors"]
                .as_array()
                .expect("field errors")
                .iter()
                .map(|error| {
                    (
                        error["field"].as_str().expect("field").to_owned(),
                        error["message_key"].as_str().expect("key").to_owned(),
                    )
                })
                .collect()
        };
        let both = parse_body(&request("not an address", "owner")).expect_err("invalid");
        assert_eq!(
            fields(both),
            [
                ("/email".to_owned(), "member.email_invalid".to_owned()),
                ("/role".to_owned(), "member.role_invalid".to_owned()),
            ]
        );
        let role_only = parse_body(&request("member@example.test", "")).expect_err("invalid");
        assert_eq!(
            fields(role_only),
            [("/role".to_owned(), "member.role_invalid".to_owned())]
        );
    }

    #[test]
    fn the_body_debug_leaves_the_address_out() {
        let shown = format!("{:?}", request("Member@Example.TEST", "editor"));
        assert!(
            !shown.to_ascii_lowercase().contains("member@example.test"),
            "{shown}"
        );
        assert!(shown.contains("editor"), "{shown}");
    }

    fn claims(platform_admin: bool, act: Option<ServiceName>) -> InternalClaims {
        let mut claims = TestIssuer::claims(ServiceName::Identity, None);
        claims.platform_admin = platform_admin;
        claims.act = act.map(|sub| TokenActor { sub });
        claims
    }

    #[test]
    fn only_a_platform_administrators_own_request_may_set_roles() {
        let issuer = TestIssuer::new();
        let user = |claims: &InternalClaims| issuer.user(ServiceName::Identity, claims);
        assert!(is_own_platform_admin(&user(&claims(true, None))));
        // Exchanged by another service on the administrator's behalf.
        for service in [ServiceName::Assistant, ServiceName::Assets] {
            let mut exchanged = claims(true, Some(service));
            exchanged.sid = None;
            assert!(!is_own_platform_admin(&user(&exchanged)), "{service:?}");
        }
        assert!(!is_own_platform_admin(&user(&claims(false, None))));
        // A tenant administrator is not a platform administrator.
        let mut tenant_admin = claims(false, None);
        tenant_admin.tenant = Some(TenantGrant {
            id: TenantId::generate(),
            role: TenantRole::Admin,
        });
        assert!(!is_own_platform_admin(&user(&tenant_admin)));
    }

    #[test]
    fn the_spec_lists_the_route_and_its_answers() {
        let spec = serde_json::to_value(crate::openapi()).expect("the spec");
        let operation = &spec["paths"][PLATFORM_MEMBERS_PATH]["put"];
        assert_eq!(operation["operationId"], "setPlatformTenantMemberRole");
        for status in ["200", "201", "400", "401", "403", "404", "422", "503"] {
            assert!(
                operation["responses"][status].is_object(),
                "{status} is documented"
            );
        }
        let roles = &spec["components"]["schemas"]["MemberRole"]["enum"];
        assert_eq!(roles, &serde_json::json!(["viewer", "editor", "admin"]));
    }
}
