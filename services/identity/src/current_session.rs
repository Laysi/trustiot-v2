//! The current session (Laysi/trustiot-v2#497; services/identity/README.md, "目前的 session 與切換
//! 租戶"): who is signed in, the tenant the session acts in with the user's role there, and the
//! tenants the user belongs to; and switching the session to one of them. Both come through the
//! ingress's cookie check like any other API call.
//!
//! | Request | Response |
//! | --- | --- |
//! | No valid internal identity token | 401 `unauthenticated` |
//! | Either, with a token another service exchanged (`act`) | 403 `permission_denied`: the session, its tenants and the user's address are for the user's own requests only |
//! | `GET /api/identity/v1/sessions/current` | 200 [`CurrentSession`] |
//! | `PUT /api/identity/v1/sessions/current/tenant`, cross-site ([`crate::csrf::cross_site`], checked again after the cookie check) | 403 `permission_denied`, message key `security.cross_site_request` |
//! | `PUT …/tenant` with a `Content-Type` other than `application/json`, a body over [`MAX_BODY_BYTES`], or a body that is not `{"tenant_id": "<uuid>"}` | 400 `invalid_request` |
//! | `PUT …/tenant` with a token that names no session (`sid`), without a usable session cookie, or when the cookie's session is not the token's session, is not the token user's, or has ended | 401 `unauthenticated`; nothing changed |
//! | `PUT …/tenant` naming a tenant the user is not a member of, existing or not | 403 `permission_denied`, message key `session.tenant_not_member`; the session keeps its tenant |
//! | `PUT …/tenant` naming one of the user's tenants, suspended ones included | 200 [`CurrentSession`] as it is now: the session's next request acts in that tenant with the user's role there (in a suspended tenant: only at this service, see `crate::ext_authz`) |
//! | The database failed, more membership reads are waiting than [`MEMBERSHIP_READS`] serve within [`ACQUIRE_TIMEOUT`], or the platform role has no credentials | 503 `unavailable`; nothing changed |
//!
//! Where each part comes from:
//!
//! - The user, the platform administrator flag, and the tenant with its role: the request's token,
//!   which the cookie check signed from the session and the memberships a moment ago. The
//!   tenant's name and state are read in a transaction of that tenant (row-level security).
//! - Each tenant's state (Laysi/trustiot-v2#777): `active`, `suspended` or `deleted`, read with
//!   the memberships, so the app can show the suspension page and offer the user's other
//!   tenants. A suspended tenant stays in the list and can be switched to: the session then
//!   reaches this service only, which the app needs to show the page, switch again or log out.
//!   The reason is not shown; only a platform administrator reads it (`crate::tenant_state`).
//! - The user's tenants: every membership of the token's user, read with the platform role
//!   (memberships are tenant data, and the service role sees one tenant at a time;
//!   `0007_preferences_and_tenant_switch.sql` lets the platform role read them, nothing more),
//!   in a `READ ONLY` transaction, for the token's user only: the query's only condition comes
//!   from the verified token, never from the request's body, path or query (decision 29).
//! - Switching: the target must be one of those memberships; then the session the request
//!   came with points at it: the session of its cookie, which must also be the session the
//!   token was signed from (`sid`), the token user's, and still active. The pointer grants
//!   nothing by itself: the cookie check reads the role from `memberships` under row-level
//!   security on every request, so a membership removed after the switch, even a moment after,
//!   leaves the session in no tenant.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::{FromRef, Request, State};
use axum::http::header::CACHE_CONTROL;
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, put};
use contract::{ErrorBody, ErrorCode, MessageKey, SessionId, TenantId, UserId};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use service_kit::identity::{IdentityVerifier, PermissionDenied, Requirement, UserIdentity};
use service_kit::tenant::{PlatformReader, TenantDb, TenantDbError};
use tokio::sync::Semaphore;
use utoipa::ToSchema;

use crate::csrf::{CROSS_SITE_REQUEST, cross_site};
use crate::logout::CURRENT_SESSION_PATH;
use crate::members::MemberRole;
use crate::session::{SessionHash, error_kind, parse_role, session_secret};
use crate::tenant_state::TenantState;
use crate::tenants::{error, is_json};

/// Switches the request's session to another of the user's tenants.
pub const TENANT_PATH: &str = "/api/identity/v1/sessions/current/tenant";

/// The largest switch body: `{"tenant_id": "<uuid>"}` fits many times over.
pub const MAX_BODY_BYTES: usize = 1024;

/// Membership reads that may run at once per replica. They use the event log's platform pool
/// (services/identity/README.md, "連線池"), which has one connection for each outbox consumer
/// ([`crate::event_log::consumers`]) and two more ([`crate::event_log::platform_connections`]); these
/// reads never hold more than those two, so a burst of signed-in users never keeps a consumer
/// waiting for a connection. A test holds the sum to the pool's size.
pub const MEMBERSHIP_READS: usize = 2;

/// How long a membership read may wait for its turn ([`MEMBERSHIP_READS`]); then the request
/// answers 503. The pool's own wait for a connection comes after it.
pub const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(5);

/// Names the platform role's reads of memberships in its INFO line (`platform database reader`).
pub const MEMBERSHIPS_PURPOSE: &str = "session-tenants";

/// The message key of a switch to a tenant the user is not a member of.
pub const TENANT_NOT_MEMBER: MessageKey = MessageKey::new("session.tenant_not_member");

/// Every membership of `$1`, with its tenant's name and state when the tenant has a row, by the
/// platform role. The user condition is the only one: the platform role sees every tenant's
/// memberships.
const USER_MEMBERSHIPS: &str = "SELECT m.tenant_id::text, t.name, m.tenant_role, t.state
FROM identity.memberships AS m
LEFT JOIN identity.tenants AS t ON t.tenant_id = m.tenant_id
WHERE m.user_id = $1::uuid
ORDER BY t.name NULLS LAST, m.tenant_id";

/// The name and state of the transaction's tenant. Row-level security already limits the rows
/// to it; the condition is repeated so the query means the same without it.
const TENANT_NAME: &str = "SELECT name, state FROM identity.tenants WHERE tenant_id = $1::uuid";

/// The user's email address, if they log in by email. Users are not tenant data.
const USER_EMAIL: &str = "SELECT email FROM identity.users WHERE id = $1::uuid";

/// Points the session `$1` with the cookie hash `$2` at the tenant `$4`, if the session belongs
/// to `$3` and has not ended (by the database clock).
const SWITCH_TENANT: &str = "UPDATE identity.sessions SET active_tenant_id = $4::uuid
WHERE id = $1::uuid AND token_hash = $2 AND user_id = $3::uuid
    AND revoked_at IS NULL AND expires_at > now()";

/// A tenant the user belongs to, with their role there.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct TenantMembership {
    /// The tenant.
    pub tenant_id: TenantId,
    /// The tenant's name; `null` only for a tenant that has no record, which only data written
    /// outside the API has.
    #[schema(required = true)]
    pub name: Option<String>,
    /// The user's role in the tenant.
    pub role: MemberRole,
    /// The tenant's state (Laysi/trustiot-v2#777): while it is `suspended` or `deleted`, the
    /// session reaches this service only, and every other request in the tenant answers 403
    /// `tenant_suspended`. A tenant without a record is `active`, as the cookie check reads it.
    pub state: TenantState,
}

/// The session of the request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct CurrentSession {
    /// The signed-in user.
    pub user_id: UserId,
    /// The address the user logs in with; `null` for a user who cannot log in by email.
    #[schema(required = true)]
    pub email: Option<String>,
    /// Whether the user is a platform administrator (A2), independent of any tenant.
    pub platform_admin: bool,
    /// The tenant the session acts in, with the user's role there; `null` when it acts in none:
    /// none was picked yet, or the user is no longer a member of it.
    #[schema(required = true)]
    pub active_tenant: Option<TenantMembership>,
    /// Every tenant the user belongs to, by name; switch with
    /// `PUT /api/identity/v1/sessions/current/tenant`.
    pub tenants: Vec<TenantMembership>,
}

/// The body of `PUT /api/identity/v1/sessions/current/tenant`.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SwitchTenant {
    /// One of the user's tenants.
    pub tenant_id: TenantId,
}

/// Why the current session could not be read or switched.
#[derive(Debug, thiserror::Error)]
pub enum AccountError {
    /// The platform role has no credentials, so the user's tenants cannot be read.
    #[error("the platform role has no credentials")]
    NoPlatformRole,
    /// [`MEMBERSHIP_READS`] reads were running for all of [`ACQUIRE_TIMEOUT`].
    #[error("every membership read stayed in use")]
    Busy,
    /// The database failed or refused the session.
    #[error(transparent)]
    Database(#[from] TenantDbError),
    /// A row or the token holds a value the schema or this build does not allow.
    #[error("the data is inconsistent: {0}")]
    Inconsistent(&'static str),
}

impl From<sqlx::Error> for AccountError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(TenantDbError::Database(error))
    }
}

impl AccountError {
    /// A short description for the log: the SQLSTATE or the kind, never data.
    fn log_kind(&self) -> String {
        match self {
            Self::Database(TenantDbError::Database(error)) => error_kind(error),
            other => other.to_string(),
        }
    }
}

/// What a switch did.
#[derive(Debug)]
pub enum Switched {
    /// The session acts in the tenant from its next request on.
    Done(CurrentSession),
    /// The user is not a member of the tenant, or there is no such tenant; nothing changed.
    NotMember,
    /// The cookie's session is not the token's, not the user's, or it has ended; nothing
    /// changed.
    NoSession,
}

/// One membership row: tenant, name, role, state.
type MembershipRow = (String, Option<String>, String, Option<String>);

/// Reads the current session and switches its tenant: the service role's pool (the tenant API's,
/// shared with the preferences) and, when the platform role has credentials, a reader of the
/// event log's platform pool for memberships, at most [`MEMBERSHIP_READS`] at once.
#[derive(Clone, Debug)]
pub struct AccountStore {
    db: TenantDb,
    memberships: Option<PlatformReader>,
    reads: Arc<Semaphore>,
}

impl AccountStore {
    /// A store on the service role's pool `db` and the platform role's `memberships` reader
    /// ([`MEMBERSHIPS_PURPOSE`]); without a reader every request answers 503.
    #[must_use]
    pub fn new(db: TenantDb, memberships: Option<PlatformReader>) -> Self {
        Self {
            db,
            memberships,
            reads: Arc::new(Semaphore::new(MEMBERSHIP_READS)),
        }
    }

    /// The session of the request `user` came with.
    ///
    /// # Errors
    ///
    /// When the database fails, the platform role has no credentials, or the token names a role
    /// this build does not know.
    pub async fn current(&self, user: &UserIdentity) -> Result<CurrentSession, AccountError> {
        let tenants = self.memberships(user.user_id()).await?;
        let active_tenant = match user.tenant() {
            Some(grant) => {
                let (name, state) = self.tenant_name(user).await?;
                Some(TenantMembership {
                    tenant_id: grant.id,
                    name,
                    role: MemberRole::of(grant.role)
                        .ok_or(AccountError::Inconsistent("token role"))?,
                    state,
                })
            }
            None => None,
        };
        Ok(CurrentSession {
            user_id: user.user_id(),
            email: self.email(user.user_id()).await?,
            platform_admin: user.is_platform_admin(),
            active_tenant,
            tenants,
        })
    }

    /// Switches the session `session` with the cookie hash `cookie` to `tenant`, if `user` is
    /// a member of it and the session is theirs and active.
    ///
    /// # Errors
    ///
    /// When the database fails or the platform role has no credentials; nothing changed then.
    pub async fn switch(
        &self,
        user: &UserIdentity,
        session: SessionId,
        cookie: SessionHash,
        tenant: TenantId,
    ) -> Result<Switched, AccountError> {
        let tenants = self.memberships(user.user_id()).await?;
        let Some(target) = tenants
            .iter()
            .find(|membership| membership.tenant_id == tenant)
            .cloned()
        else {
            return Ok(Switched::NotMember);
        };
        // Before the switch, so nothing can fail after it.
        let email = self.email(user.user_id()).await?;
        let done = sqlx::query(SWITCH_TENANT)
            .bind(session.to_string())
            .bind(cookie.as_bytes().as_slice())
            .bind(user.user_id().to_string())
            .bind(tenant.to_string())
            .execute(self.db.pool())
            .await?;
        if done.rows_affected() == 0 {
            return Ok(Switched::NoSession);
        }
        Ok(Switched::Done(CurrentSession {
            user_id: user.user_id(),
            email,
            platform_admin: user.is_platform_admin(),
            active_tenant: Some(target),
            tenants,
        }))
    }

    /// Every membership of `user`, by the platform role; `user` is always the verified token's.
    async fn memberships(&self, user: UserId) -> Result<Vec<TenantMembership>, AccountError> {
        let reader = self
            .memberships
            .as_ref()
            .ok_or(AccountError::NoPlatformRole)?;
        // Held until the transaction ends, at the end of this function.
        let _turn = tokio::time::timeout(ACQUIRE_TIMEOUT, self.reads.acquire())
            .await
            .map_err(|_| AccountError::Busy)?
            .map_err(|_| AccountError::Busy)?;
        // The kit logs the purpose of every platform read; this line adds whose memberships.
        tracing::debug!(
            user_id = %user,
            purpose = MEMBERSHIPS_PURPOSE,
            "reading a user's memberships with the platform role"
        );
        let mut tx = reader.begin().await?;
        let rows: Vec<MembershipRow> = sqlx::query_as(USER_MEMBERSHIPS)
            .bind(user.to_string())
            .fetch_all(&mut *tx)
            .await?;
        tx.commit().await?;
        rows.into_iter()
            .map(|(tenant, name, role, state)| {
                let role = parse_role(&role)
                    .ok()
                    .and_then(MemberRole::of)
                    .ok_or(AccountError::Inconsistent("membership role"))?;
                Ok(TenantMembership {
                    tenant_id: tenant
                        .parse()
                        .map_err(|_| AccountError::Inconsistent("tenant id"))?,
                    name,
                    role,
                    state: tenant_state(state.as_deref())?,
                })
            })
            .collect()
    }

    /// The name and state of the token's tenant, in a transaction of that tenant; no name and
    /// `active` without a record.
    async fn tenant_name(
        &self,
        user: &UserIdentity,
    ) -> Result<(Option<String>, TenantState), AccountError> {
        let Some(tenant) = user.tenant_id() else {
            return Ok((None, TenantState::Active));
        };
        let mut tx = self.db.begin(user).await?;
        let row: Option<(String, String)> = sqlx::query_as(TENANT_NAME)
            .bind(tenant.to_string())
            .fetch_optional(&mut *tx)
            .await?;
        tx.commit().await?;
        match row {
            Some((name, state)) => Ok((Some(name), tenant_state(Some(&state))?)),
            None => Ok((None, TenantState::Active)),
        }
    }

    /// The email address of `user`; `None` without one, or when the user no longer exists.
    async fn email(&self, user: UserId) -> Result<Option<String>, AccountError> {
        let email: Option<Option<String>> = sqlx::query_scalar(USER_EMAIL)
            .bind(user.to_string())
            .fetch_optional(self.db.pool())
            .await?;
        Ok(email.flatten())
    }
}

/// A tenant's state as a row holds it: `NULL` (no tenant row, only data written outside the API
/// has one) is `active`, as the cookie check reads it (`crate::session::MEMBERSHIP`). A value
/// the schema does not allow is refused rather than guessed.
fn tenant_state(state: Option<&str>) -> Result<TenantState, AccountError> {
    match state {
        None => Ok(TenantState::Active),
        Some(state) => TenantState::parse(state).ok_or(AccountError::Inconsistent("tenant state")),
    }
}

/// Refuses any request to the user's own account (the session, its tenants, the preferences)
/// that comes with a token another service exchanged for a call on the user's behalf: these
/// APIs are for the user's own requests, through the cookie check. Logs the refusal at INFO
/// with `message`, the reason and the calling service. service-kit's shared check
/// (`Requirement::OwnRequest`) decides and answers; the log keeps this service's own messages.
pub(crate) fn exchanged_refusal(user: &UserIdentity, message: &'static str) -> Option<Response> {
    let denial = user.meets(Requirement::OwnRequest).err()?;
    tracing::info!(
        user_id = %user.user_id(),
        reason = denial.reason(),
        actor = user.claims().act.map(|actor| actor.sub.as_str()).unwrap_or_default(),
        "{message}"
    );
    Some(PermissionDenied::from(denial).into_response())
}

/// Refuses a write to the user's own account (switching the session's tenant, changing
/// preferences) that is not the user's own request, before its body is read: a cross-site
/// request, checked again here although the cookie check refused it already, in case the
/// gateway stops passing it the headers (services/identity/README.md, "跨站請求"); a token
/// another service exchanged ([`exchanged_refusal`]); and a body that is not JSON. Logs a
/// refusal at INFO with `message` and the reason, never a header's value.
pub(crate) fn own_write_refusal(
    parts: &Parts,
    user: &UserIdentity,
    message: &'static str,
) -> Option<Response> {
    if let Some(header) = cross_site(&parts.method, &parts.headers) {
        tracing::info!(
            user_id = %user.user_id(),
            reason = "cross_site_request",
            header,
            "{message}"
        );
        return Some(error(
            ErrorBody::new(ErrorCode::PermissionDenied).with_message_key(CROSS_SITE_REQUEST),
        ));
    }
    if let Some(refusal) = exchanged_refusal(user, message) {
        return Some(refusal);
    }
    if !is_json(&parts.headers) {
        return Some(error(ErrorBody::new(ErrorCode::InvalidRequest)));
    }
    None
}

/// The body as `T`, if it is one JSON object: serde would also take an array as a struct's
/// fields in order, which these APIs do not offer. A field given twice is refused too.
pub(crate) fn json_object<T: DeserializeOwned>(body: &[u8]) -> Option<T> {
    if body.trim_ascii_start().first() != Some(&b'{') {
        return None;
    }
    serde_json::from_slice(body).ok()
}

/// The state of the current session routes.
#[derive(Clone)]
pub struct SessionRoutes {
    verifier: IdentityVerifier,
    store: Arc<AccountStore>,
}

impl FromRef<SessionRoutes> for IdentityVerifier {
    fn from_ref(state: &SessionRoutes) -> Self {
        state.verifier.clone()
    }
}

/// The current session routes: `GET` [`CURRENT_SESSION_PATH`] and `PUT` [`TENANT_PATH`].
/// Logging out (`DELETE` [`CURRENT_SESSION_PATH`]) is [`crate::logout`]'s.
pub fn routes(verifier: IdentityVerifier, store: AccountStore) -> Router {
    Router::new()
        .route(CURRENT_SESSION_PATH, get(current_session))
        .route(TENANT_PATH, put(switch_tenant))
        .with_state(SessionRoutes {
            verifier,
            store: Arc::new(store),
        })
}

/// 200 with the session, never cached.
fn answer(session: &CurrentSession) -> Response {
    ([(CACHE_CONTROL, "no-store")], axum::Json(session)).into_response()
}

/// Reads the current session: the user, the tenant it acts in with the user's role there, and
/// the user's tenants.
#[utoipa::path(
    get,
    path = "/api/identity/v1/sessions/current",
    tag = "identity",
    operation_id = "getCurrentSession",
    responses(
        (status = 200, description = "The session: the user, the tenant it acts in with the user's role there (as of this request), and every tenant the user belongs to.", body = CurrentSession),
        (status = 401, description = "No valid session (`unauthenticated`).", body = ErrorBody),
        (status = 403, description = "A token another service exchanged for a call on the user's behalf (`permission_denied`): only the user's own requests read the session.", body = ErrorBody),
        (status = 503, description = "The database failed, too many reads of users' tenants were waiting, or the platform role has no credentials (`unavailable`).", body = ErrorBody),
    )
)]
async fn current_session(State(state): State<SessionRoutes>, user: UserIdentity) -> Response {
    if let Some(refusal) = exchanged_refusal(&user, "refused to read the current session") {
        return refusal;
    }
    match state.store.current(&user).await {
        Ok(session) => answer(&session),
        Err(failure) => {
            tracing::error!(
                user_id = %user.user_id(),
                error_kind = failure.log_kind(),
                "could not read the current session"
            );
            error(ErrorBody::new(ErrorCode::Unavailable))
        }
    }
}

/// Switches the session to another of the user's tenants; its next request acts there with the
/// user's role in it.
#[utoipa::path(
    put,
    path = "/api/identity/v1/sessions/current/tenant",
    tag = "identity",
    operation_id = "switchTenant",
    request_body = SwitchTenant,
    responses(
        (status = 200, description = "The session acts in the tenant from its next request on; the body is the session as it is now.", body = CurrentSession),
        (status = 400, description = "The `Content-Type` is not `application/json`, the body is over 1024 bytes, or it is not the JSON object `{\"tenant_id\": \"<uuid>\"}` (`invalid_request`).", body = ErrorBody),
        (status = 401, description = "No valid session, a token that names no session, or a session cookie that is missing, is not the session the token was signed from, or whose session has ended (`unauthenticated`); nothing changed.", body = ErrorBody),
        (status = 403, description = "The user is not a member of the tenant, or there is no such tenant (`permission_denied`, message key `session.tenant_not_member`); a cross-site request (message key `security.cross_site_request`); or a token another service exchanged (`permission_denied`). The session keeps its tenant.", body = ErrorBody),
        (status = 503, description = "The database failed, too many reads of users' tenants were waiting, or the platform role has no credentials; nothing changed (`unavailable`).", body = ErrorBody),
    )
)]
async fn switch_tenant(
    State(state): State<SessionRoutes>,
    user: UserIdentity,
    request: Request,
) -> Response {
    let (parts, body) = request.into_parts();
    if let Some(refusal) = own_write_refusal(&parts, &user, "refused to switch tenants") {
        return refusal;
    }
    let Ok(body) = axum::body::to_bytes(body, MAX_BODY_BYTES).await else {
        return error(ErrorBody::new(ErrorCode::InvalidRequest));
    };
    let Some(request) = json_object::<SwitchTenant>(&body) else {
        return error(ErrorBody::new(ErrorCode::InvalidRequest));
    };
    // The session the ext_authz check signed the token from; only its tokens name one.
    let Some(session) = user.claims().sid else {
        tracing::info!(
            user_id = %user.user_id(),
            reason = "no_session_claim",
            "refused to switch tenants"
        );
        return error(ErrorBody::new(ErrorCode::Unauthenticated));
    };
    let secret = match session_secret(&parts.headers) {
        Ok(secret) => secret,
        Err(failure) => {
            tracing::info!(
                user_id = %user.user_id(),
                reason = failure.reason(),
                "refused to switch tenants"
            );
            return error(ErrorBody::new(ErrorCode::Unauthenticated));
        }
    };
    match state
        .store
        .switch(&user, session, secret.hash(), request.tenant_id)
        .await
    {
        Ok(Switched::Done(session)) => {
            let from = user.tenant_id().map(|tenant| tenant.to_string());
            tracing::info!(
                user_id = %user.user_id(),
                from = from.as_deref(),
                tenant_id = %request.tenant_id,
                "switched the session's tenant"
            );
            answer(&session)
        }
        Ok(Switched::NotMember) => {
            tracing::info!(
                user_id = %user.user_id(),
                tenant_id = %request.tenant_id,
                reason = "not_a_member",
                "refused to switch tenants"
            );
            error(ErrorBody::new(ErrorCode::PermissionDenied).with_message_key(TENANT_NOT_MEMBER))
        }
        Ok(Switched::NoSession) => {
            tracing::info!(
                user_id = %user.user_id(),
                reason = "no_active_session",
                "refused to switch tenants"
            );
            error(ErrorBody::new(ErrorCode::Unauthenticated))
        }
        Err(failure) => {
            tracing::error!(
                user_id = %user.user_id(),
                error_kind = failure.log_kind(),
                "could not switch tenants"
            );
            error(ErrorBody::new(ErrorCode::Unavailable))
        }
    }
}

#[cfg(test)]
mod tests {
    use contract::TenantRole;
    use serde_json::json;

    use super::*;

    const TENANT: &str = "01928f6e-7a3c-7d4e-8f00-00000000000a";
    const USER: &str = "01928f6e-7a3c-7d4e-8f00-0123456789ab";

    #[test]
    fn roles_show_as_the_memberships_store_them() {
        for (role, shown) in [
            (TenantRole::Viewer, "viewer"),
            (TenantRole::Editor, "editor"),
            (TenantRole::Admin, "admin"),
        ] {
            let member = MemberRole::of(role).expect("a known role");
            assert_eq!(json!(member), json!(shown));
            assert_eq!(
                parse_role(shown).ok().and_then(MemberRole::of),
                Some(member)
            );
        }
        assert_eq!(MemberRole::of(TenantRole::Unrecognized), None);
    }

    #[test]
    fn a_tenant_without_a_record_is_active_and_an_unknown_state_is_refused() {
        assert_eq!(tenant_state(None).expect("no record"), TenantState::Active);
        for state in [
            TenantState::Active,
            TenantState::Suspended,
            TenantState::Deleted,
        ] {
            assert_eq!(tenant_state(Some(state.as_str())).expect("known"), state);
        }
        assert!(matches!(
            tenant_state(Some("paused")),
            Err(AccountError::Inconsistent("tenant state"))
        ));
    }

    #[test]
    fn the_session_serializes_with_every_field() {
        let tenant: TenantId = TENANT.parse().expect("tenant id");
        let user: UserId = USER.parse().expect("user id");
        let membership = TenantMembership {
            tenant_id: tenant,
            name: Some("Plant A".to_owned()),
            role: MemberRole::Editor,
            state: TenantState::Suspended,
        };
        let session = CurrentSession {
            user_id: user,
            email: None,
            platform_admin: false,
            active_tenant: None,
            tenants: vec![membership.clone()],
        };
        assert_eq!(
            serde_json::to_value(&session).expect("serialize"),
            json!({
                "user_id": USER,
                "email": null,
                "platform_admin": false,
                "active_tenant": null,
                "tenants": [{"tenant_id": TENANT, "name": "Plant A", "role": "editor", "state": "suspended"}],
            })
        );
        let switched = CurrentSession {
            active_tenant: Some(membership),
            ..session
        };
        assert_eq!(
            serde_json::to_value(&switched).expect("serialize")["active_tenant"],
            json!({"tenant_id": TENANT, "name": "Plant A", "role": "editor", "state": "suspended"})
        );
    }

    /// Every field of the session is in every response, `null` or not, so the API document
    /// lists each as `required` and the generated client types never make one optional.
    #[test]
    fn every_field_of_the_session_is_required_in_the_api_document() {
        let document = serde_json::to_value(crate::openapi()).expect("serialize the document");
        for name in ["CurrentSession", "TenantMembership"] {
            let schema = &document["components"]["schemas"][name];
            let mut properties: Vec<&String> = schema["properties"]
                .as_object()
                .expect("properties")
                .keys()
                .collect();
            let mut required: Vec<&str> = schema["required"]
                .as_array()
                .expect("required")
                .iter()
                .map(|field| field.as_str().expect("a field name"))
                .collect();
            properties.sort();
            required.sort_unstable();
            assert_eq!(properties, required, "{name}");
        }
        // The fields that may be null are the ones this is about.
        for (schema, field) in [
            ("CurrentSession", "email"),
            ("CurrentSession", "active_tenant"),
            ("TenantMembership", "name"),
        ] {
            assert!(
                document["components"]["schemas"][schema]["required"]
                    .as_array()
                    .is_some_and(|required| required.contains(&json!(field))),
                "{schema}.{field}"
            );
        }
    }

    /// Membership reads share the event log's platform pool and never take the connection a
    /// producer's consumer needs (services/identity/README.md, "連線池").
    #[test]
    fn membership_reads_leave_every_consumer_a_connection() {
        const { assert!(MEMBERSHIP_READS >= 1) };
        let pool = usize::try_from(crate::event_log::platform_connections()).expect("fits");
        let consumers = usize::try_from(crate::event_log::consumers()).expect("fits");
        assert!(
            MEMBERSHIP_READS + consumers <= pool,
            "{MEMBERSHIP_READS} reads and {consumers} consumers in {pool} connections"
        );
    }

    #[test]
    fn a_switch_names_one_tenant_and_nothing_else() {
        let body = json!({"tenant_id": TENANT}).to_string();
        let parsed: SwitchTenant = json_object(body.as_bytes()).expect("a tenant");
        assert_eq!(parsed.tenant_id.to_string(), TENANT);
        let spaced = format!(" \n\t{body}\n");
        assert!(json_object::<SwitchTenant>(spaced.as_bytes()).is_some());
        let twice = format!(
            "{{\"tenant_id\": \"{TENANT}\", \"tenant_id\": \"01928f6e-7a3c-7d4e-8f00-00000000000b\"}}"
        );
        for refused in [
            json!({}).to_string(),
            json!({"tenant_id": null}).to_string(),
            json!({"tenant_id": "plant-a"}).to_string(),
            json!({"tenant_id": 7}).to_string(),
            json!({"tenant_id": TENANT, "user_id": USER}).to_string(),
            json!({"tenant_id": TENANT, "role": "admin"}).to_string(),
            json!({"tenant": TENANT}).to_string(),
            // serde would read an array as the struct's fields in order.
            json!([TENANT]).to_string(),
            json!(TENANT).to_string(),
            "null".to_owned(),
            String::new(),
            twice,
            format!("{body} {body}"),
        ] {
            assert!(
                json_object::<SwitchTenant>(refused.as_bytes()).is_none(),
                "{refused}"
            );
        }
    }
}
