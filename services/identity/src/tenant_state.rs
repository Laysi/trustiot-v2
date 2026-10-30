//! A tenant's lifecycle state (Laysi/trustiot-v2#776, checklist B1; services/identity/README.md,
//! "租戶狀態"): a platform administrator suspends a tenant with a reason, changes the reason of
//! a suspension, or resumes it with `PUT /api/identity/v1/platform/tenants/{tenant_id}/state`, and
//! reads the state with `GET` on the same path. A change and the `identity.tenant_state_changed`
//! event, a snapshot of the new state with its version, are written in one transaction, in the
//! row-level security context of the tenant in the path, so the operation log has every change
//! and consumers keep the snapshot with the larger version (crates/outbox, "通知模式").
//!
//! The states are `active`, `suspended` (always with a reason: `security_or_abuse`,
//! `non_payment` or `contract_terminated`) and `deleted` (Laysi/trustiot-v2#109; this API never
//! sets it). A tenant starts active at version 1; each change of the state or the reason adds
//! one. A terminated contract and `deleted` are final.
//!
//! Every answer also carries [`TenantStatus`], what the back office shows (Laysi/trustiot-v2#779,
//! README "停權的處理中"): a suspended or deleted tenant is `processing` until the certs service
//! confirmed (`certs.tenant_state_applied`, recorded by [`crate::state_applied`]) that it applied
//! this version or a newer one, and only then `suspended` or `deleted`; an active tenant is
//! `active` at once.
//!
//! | Request | Response |
//! | --- | --- |
//! | No valid internal identity token | 401 `unauthenticated` |
//! | Not a platform administrator, or a token another service exchanged (`act`) | 403 `permission_denied`, decided from the token alone, before the body is read or the database touched |
//! | A tenant ID that is not a UUID | 400 `invalid_request` |
//! | `PUT`: a `Content-Type` other than `application/json` (parameters such as `charset` allowed) | 400 `invalid_request` |
//! | `PUT`: a body over [`MAX_BODY_BYTES`], or one that is not a JSON object with a string `state` and a string or null `reason` | 400 `invalid_request` |
//! | `PUT`: a `state` other than `active` or `suspended`; a suspension without one of the three reasons, or a resumption with a reason | 422 `validation_failed`, fields `/state` (`tenant.state_invalid`), `/reason` (`tenant.suspension_reason_invalid`) |
//! | No tenant has the ID | 404 `not_found`; nothing was written |
//! | `PUT`: the tenant's contract was terminated or it is deleted, and the request would change that | 409 `conflict`, key `tenant.state_final`; nothing was written, the version is unchanged |
//! | The database failed or refused the transaction | 503 `unavailable`; nothing was written |
//! | `GET` | 200 with [`TenantStateSnapshot`] |
//! | `PUT`: the state or the reason changed | 200 with the new [`TenantStateSnapshot`] (the version one larger), and the event |
//! | `PUT`: the tenant already had this state and reason | 200 with the [`TenantStateSnapshot`] as it was; nothing was written, no event |
//!
//! Every response of the handlers (all but the identity extractor's 401) carries
//! `cache-control: no-store`. Concurrent requests for one tenant wait for each other on the row's
//! lock (`SELECT ... FOR NO KEY UPDATE`, at most [`LOCK_TIMEOUT`]), so each decides on the state the
//! previous one left: identical concurrent suspensions write one new version and one event. A
//! change logs one INFO line with the administrator, the tenant, both states and reasons and the
//! new version.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::rejection::PathRejection;
use axum::extract::{FromRef, Path, State};
use axum::http::header::CACHE_CONTROL;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use chrono::{DateTime, Utc};
use contract::event::Actor;
use contract::events::identity as events;
use contract::{ErrorBody, ErrorCode, FieldError, MessageKey, TenantId};
use outbox::Aggregate;
use serde::{Deserialize, Serialize};
use service_kit::identity::{IdentityVerifier, Requirement, UserIdentity};
use service_kit::tenant::{BackgroundTenantDb, TenantDbError, TenantTransaction};
use utoipa::ToSchema;

use crate::tenants::{error, is_json};

/// The route, as the ingress gateway forwards it (no prefix is stripped).
pub const PLATFORM_TENANT_STATE_PATH: &str = "/api/identity/v1/platform/tenants/{tenant_id}/state";

/// The message key of a `state` this API does not set.
pub const STATE_INVALID: MessageKey = MessageKey::new("tenant.state_invalid");

/// The message key of a suspension without a known reason, or a resumption with one.
pub const REASON_INVALID: MessageKey = MessageKey::new("tenant.suspension_reason_invalid");

/// The message key of a change to a tenant whose contract was terminated, or that is deleted.
pub const STATE_FINAL: MessageKey = MessageKey::new("tenant.state_final");

/// The largest body read: a state and a reason fit many times over. The body is read only after
/// the permission check, so others cannot make the service buffer more.
pub const MAX_BODY_BYTES: usize = 4096;

/// The longest the transaction waits for the tenant's row lock (`SET LOCAL lock_timeout` at its
/// start). Requests for one tenant run one after another; past this bound the waiting one
/// answers 503 instead of holding one of the tenant pool's few connections.
pub const LOCK_TIMEOUT: Duration = Duration::from_secs(5);

/// Sets [`LOCK_TIMEOUT`] for the rest of the transaction, like `SET LOCAL`.
const SET_LOCK_TIMEOUT: &str = "SELECT set_config('lock_timeout', $1, true)";

/// Names the work in the kit's DEBUG log of the tenant transaction.
const READ_PURPOSE: &str = "read-tenant-state";
const SET_PURPOSE: &str = "set-tenant-state";

/// The tenant's state; row-level security shows no other tenant's row.
const READ_STATE: &str = "SELECT state, suspension_reason, state_version, state_changed_at,
    certs_applied_version
FROM identity.tenants
WHERE tenant_id = $1::uuid";

/// The same, locked until the transaction ends, so two requests for one tenant change it one
/// after the other and each decides on the state the previous one left. `NO KEY`: the update
/// changes no key, so the lock is the one the update takes anyway and would not hold up a row
/// that refers to the tenant.
const LOCK_STATE: &str = "SELECT state, suspension_reason, state_version, state_changed_at,
    certs_applied_version
FROM identity.tenants
WHERE tenant_id = $1::uuid
FOR NO KEY UPDATE";

/// The new state of the locked row: the version goes up by exactly one (the migration's trigger
/// refuses anything else). The trigger also sets `state_changed_at`, by the database clock at
/// the change, after the lock was granted.
const UPDATE_STATE: &str = "UPDATE identity.tenants
SET state = $2, suspension_reason = $3, state_version = state_version + 1
WHERE tenant_id = $1::uuid
RETURNING state_version, state_changed_at, certs_applied_version";

/// A tenant's lifecycle state, as the API returns it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum TenantState {
    /// The tenant works normally.
    Active,
    /// Every access is cut off; `reason` says why.
    Suspended,
    /// The contract ended and the tenant's data is being or was deleted. Final.
    Deleted,
}

impl TenantState {
    /// The state `text` names exactly; `None` otherwise.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "active" => Some(Self::Active),
            "suspended" => Some(Self::Suspended),
            "deleted" => Some(Self::Deleted),
            _ => None,
        }
    }

    /// The name, as the API and the `state` column write it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Suspended => "suspended",
            Self::Deleted => "deleted",
        }
    }

    /// The same state in the event.
    #[must_use]
    pub const fn event(self) -> events::TenantState {
        match self {
            Self::Active => events::TenantState::Active,
            Self::Suspended => events::TenantState::Suspended,
            Self::Deleted => events::TenantState::Deleted,
        }
    }
}

/// What the back office shows of a tenant's state (Laysi/trustiot-v2#779): the state, except that a
/// suspension or deletion the certs service has not yet confirmed is still processing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum TenantStatus {
    /// The tenant is active (at once after a resumption: nothing waits for a confirmation).
    Active,
    /// The tenant is suspended or deleted, but the certs service has not yet confirmed that it
    /// applied this state version (gateways disconnected, CRL in effect). Shown until it does,
    /// however long that takes.
    Processing,
    /// The tenant is suspended, and the certs service confirmed this version or a newer one.
    Suspended,
    /// The tenant is deleted, and the certs service confirmed this version or a newer one.
    Deleted,
}

impl TenantStatus {
    /// The status of a tenant in `state` at `state_version`, when the certs service confirmed
    /// `applied` (0 before any confirmation): `processing` while a suspension or deletion has a
    /// larger version than the confirmed one.
    #[must_use]
    pub const fn of(state: TenantState, state_version: i64, applied: i64) -> Self {
        match state {
            TenantState::Active => Self::Active,
            TenantState::Suspended | TenantState::Deleted if applied < state_version => {
                Self::Processing
            }
            TenantState::Suspended => Self::Suspended,
            TenantState::Deleted => Self::Deleted,
        }
    }
}

/// Why a tenant is suspended, as the API takes and returns it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum SuspensionReason {
    /// A security incident or abuse.
    SecurityOrAbuse,
    /// Unpaid invoices.
    NonPayment,
    /// The contract was terminated. Final: the tenant is never resumed.
    ContractTerminated,
}

impl SuspensionReason {
    /// The reason `text` names exactly; `None` otherwise.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "security_or_abuse" => Some(Self::SecurityOrAbuse),
            "non_payment" => Some(Self::NonPayment),
            "contract_terminated" => Some(Self::ContractTerminated),
            _ => None,
        }
    }

    /// The name, as the API and the `suspension_reason` column write it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SecurityOrAbuse => "security_or_abuse",
            Self::NonPayment => "non_payment",
            Self::ContractTerminated => "contract_terminated",
        }
    }

    /// The same reason in the event.
    #[must_use]
    pub const fn event(self) -> events::SuspensionReason {
        match self {
            Self::SecurityOrAbuse => events::SuspensionReason::SecurityOrAbuse,
            Self::NonPayment => events::SuspensionReason::NonPayment,
            Self::ContractTerminated => events::SuspensionReason::ContractTerminated,
        }
    }
}

/// The states a request may ask for, for the API spec only: `deleted` is not set through this
/// API, so the request schema does not offer it (`parse_body` reads the body's text).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RequestedTenantState {
    /// Resume the tenant.
    Active,
    /// Suspend the tenant, or change the reason of its suspension.
    Suspended,
}

/// The body of `PUT /api/identity/v1/platform/tenants/{tenant_id}/state`. Unknown fields are
/// ignored.
#[derive(Debug, Deserialize, ToSchema)]
pub struct SetTenantState {
    /// `suspended` to suspend the tenant (or change the reason of its suspension), `active` to
    /// resume it. `deleted` is not set through this API (422).
    #[schema(value_type = RequestedTenantState)]
    pub state: String,
    /// Why the tenant is suspended: required with `suspended`, absent or null with `active`.
    #[serde(default)]
    #[schema(value_type = Option<SuspensionReason>)]
    pub reason: Option<String>,
}

/// A tenant's state, its reason and its version.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct TenantStateSnapshot {
    /// The tenant.
    pub tenant_id: TenantId,
    /// The state.
    pub state: TenantState,
    /// Why the tenant is suspended; null when it is active. Only a terminated contract is
    /// deleted, so a deleted tenant keeps `contract_terminated`.
    pub reason: Option<SuspensionReason>,
    /// The version of this state: 1 when the tenant was created, one more with every change of
    /// the state or the reason. The `identity.tenant_state_changed` event of the change carries
    /// the same number.
    pub state_version: i64,
    /// When the tenant got this state (its creation time while it was never changed), by the
    /// database clock.
    pub state_changed_at: DateTime<Utc>,
    /// What the back office shows (Laysi/trustiot-v2#779): `processing` while the tenant is
    /// suspended or deleted and the certs service has not yet confirmed that it applied this
    /// version (gateways disconnected, CRL in effect); otherwise the state. Show how long a
    /// tenant has been processing from `state_changed_at`.
    pub status: TenantStatus,
}

/// The state a request asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    /// Resume the tenant.
    Active,
    /// Suspend the tenant, or change the reason of its suspension.
    Suspended(SuspensionReason),
}

impl Target {
    const fn state(self) -> TenantState {
        match self {
            Self::Active => TenantState::Active,
            Self::Suspended(_) => TenantState::Suspended,
        }
    }

    const fn reason(self) -> Option<SuspensionReason> {
        match self {
            Self::Active => None,
            Self::Suspended(reason) => Some(reason),
        }
    }
}

/// What a request did to the tenant's state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The state or the reason changed: a new version and its event.
    Changed,
    /// The tenant already had this state and reason: nothing was written, no event.
    Unchanged,
}

/// Why a tenant's state was not read or set.
#[derive(Debug, thiserror::Error)]
pub enum StateError {
    /// The caller is not a platform administrator, or another service exchanged the token.
    #[error("only a platform administrator's own request may read or change a tenant's state")]
    NotAllowed,
    /// No tenant has the ID.
    #[error("no tenant has this ID")]
    UnknownTenant,
    /// The tenant's contract was terminated, or it is deleted: the request would change a final
    /// state.
    #[error("the tenant's state is final")]
    Final,
    /// A stored row breaks a rule of its table.
    #[error("inconsistent tenant state: {0}")]
    Inconsistent(&'static str),
    /// The transaction could not start, or a statement in it failed.
    #[error(transparent)]
    Database(#[from] TenantDbError),
    /// The event could not be written; the transaction was rolled back.
    #[error(transparent)]
    Publish(#[from] outbox::PublishError),
    /// The event's aggregate is not valid; the kind and a tenant ID always are, so this does not
    /// happen.
    #[error(transparent)]
    Aggregate(#[from] outbox::InvalidAggregate),
}

impl From<sqlx::Error> for StateError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(TenantDbError::Database(error))
    }
}

impl StateError {
    /// A short description for the log: the SQLSTATE or the kind, never data.
    fn log_kind(&self) -> String {
        match self {
            Self::Database(TenantDbError::Database(error)) => crate::session::error_kind(error),
            Self::Database(error) => error.to_string(),
            // `PublishError` prints only the SQLSTATE or kind, `InvalidAggregate` only the rule.
            Self::Publish(error) => error.to_string(),
            Self::Aggregate(error) => error.to_string(),
            Self::NotAllowed | Self::UnknownTenant | Self::Final | Self::Inconsistent(_) => {
                self.to_string()
            }
        }
    }
}

/// A row of [`READ_STATE`] or [`LOCK_STATE`].
type Row = (String, Option<String>, i64, DateTime<Utc>, i64);

/// The snapshot of `tenant` from its row; [`StateError::Inconsistent`] when the row breaks the
/// table's rules (which its CHECK constraints hold).
fn snapshot(tenant: TenantId, row: Row) -> Result<TenantStateSnapshot, StateError> {
    let (state, reason, state_version, state_changed_at, applied) = row;
    let state = TenantState::parse(&state).ok_or(StateError::Inconsistent("state"))?;
    let reason = reason
        .map(|reason| {
            SuspensionReason::parse(&reason).ok_or(StateError::Inconsistent("suspension reason"))
        })
        .transpose()?;
    Ok(TenantStateSnapshot {
        tenant_id: tenant,
        state,
        reason,
        state_version,
        state_changed_at,
        status: TenantStatus::of(state, state_version, applied),
    })
}

/// Whether `current` may never change to `target`: a deleted tenant, and a tenant whose contract
/// was terminated, stay as they are (this API never deletes). Asking for the state a tenant
/// already has is not a change, so it is never refused.
fn is_final(current: &TenantStateSnapshot, target: Target) -> bool {
    let unchanged = current.state == target.state() && current.reason == target.reason();
    !unchanged
        && (current.state == TenantState::Deleted
            || current.reason == Some(SuspensionReason::ContractTerminated))
}

/// Reads and sets tenants' states: with [`crate::tenants::TenantStore`] and
/// [`crate::members::MemberStore`], code that opens a transaction for a tenant a request names
/// but its token does not. It holds the background handle privately, and both methods refuse
/// every caller but a platform administrator's own request before they open a transaction
/// (services/identity/README.md, "資料庫").
#[derive(Clone, Debug)]
pub struct TenantStateStore {
    db: BackgroundTenantDb,
}

impl TenantStateStore {
    /// A store on `db`, the background handle of the tenant store's pool
    /// ([`crate::tenants::TenantStore::states`]).
    pub(crate) fn new(db: BackgroundTenantDb) -> Self {
        Self { db }
    }

    /// The state of `tenant`, for the platform administrator `admin`.
    ///
    /// # Errors
    ///
    /// [`StateError::NotAllowed`] unless `admin` is a platform administrator's own request,
    /// before anything is read; [`StateError::UnknownTenant`]; otherwise when the database fails.
    pub async fn read(
        &self,
        admin: &UserIdentity,
        tenant: TenantId,
    ) -> Result<TenantStateSnapshot, StateError> {
        if admin.meets(Requirement::OwnPlatformAdmin).is_err() {
            return Err(StateError::NotAllowed);
        }
        let mut tx = self.db.begin(tenant, READ_PURPOSE).await?;
        let row: Option<Row> = sqlx::query_as(READ_STATE)
            .bind(tenant.to_string())
            .fetch_optional(&mut *tx)
            .await?;
        tx.rollback().await?;
        snapshot(tenant, row.ok_or(StateError::UnknownTenant)?)
    }

    /// Gives `tenant` the state `target`, for the platform administrator `admin`: in one
    /// transaction of `tenant`, the new state with the next version and the
    /// `identity.tenant_state_changed` event naming `admin`, unless the tenant already had this
    /// state and reason.
    ///
    /// # Errors
    ///
    /// [`StateError::NotAllowed`] unless `admin` is a platform administrator's own request,
    /// before anything is read; [`StateError::UnknownTenant`]; [`StateError::Final`] when the
    /// tenant's contract was terminated or it is deleted and `target` differs; otherwise when
    /// the database fails. Nothing was written then.
    pub async fn set(
        &self,
        admin: &UserIdentity,
        tenant: TenantId,
        target: Target,
    ) -> Result<(TenantStateSnapshot, Outcome), StateError> {
        if admin.meets(Requirement::OwnPlatformAdmin).is_err() {
            return Err(StateError::NotAllowed);
        }
        let mut tx = self.db.begin(tenant, SET_PURPOSE).await?;
        sqlx::query(SET_LOCK_TIMEOUT)
            .bind(format!("{}ms", LOCK_TIMEOUT.as_millis()))
            .execute(&mut *tx)
            .await?;
        let row: Option<Row> = sqlx::query_as(LOCK_STATE)
            .bind(tenant.to_string())
            .fetch_optional(&mut *tx)
            .await?;
        let current = snapshot(tenant, row.ok_or(StateError::UnknownTenant)?)?;
        if is_final(&current, target) {
            tx.rollback().await?;
            return Err(StateError::Final);
        }
        if current.state == target.state() && current.reason == target.reason() {
            // Nothing to write: no new version, no event.
            tx.rollback().await?;
            return Ok((current, Outcome::Unchanged));
        }
        let changed = change(&mut tx, admin, &current, target).await?;
        tx.commit().await?;
        tracing::info!(
            user_id = %admin.user_id(),
            tenant_id = %tenant,
            previous_state = current.state.as_str(),
            previous_reason = current.reason.map_or("none", SuspensionReason::as_str),
            state = changed.state.as_str(),
            reason = changed.reason.map_or("none", SuspensionReason::as_str),
            state_version = changed.state_version,
            "changed a tenant's state"
        );
        Ok((changed, Outcome::Changed))
    }
}

/// Writes `target` over the locked `current` state and its event, in `tx`; returns the new
/// snapshot.
async fn change(
    tx: &mut TenantTransaction,
    admin: &UserIdentity,
    current: &TenantStateSnapshot,
    target: Target,
) -> Result<TenantStateSnapshot, StateError> {
    let tenant = current.tenant_id;
    let (state_version, state_changed_at, applied): (i64, DateTime<Utc>, i64) =
        sqlx::query_as(UPDATE_STATE)
            .bind(tenant.to_string())
            .bind(target.state().as_str())
            .bind(target.reason().map(SuspensionReason::as_str))
            .fetch_one(&mut **tx)
            .await?;
    let changed = TenantStateSnapshot {
        tenant_id: tenant,
        state: target.state(),
        reason: target.reason(),
        state_version,
        state_changed_at,
        status: TenantStatus::of(target.state(), state_version, applied),
    };
    let aggregate = Aggregate::new(events::TENANT_AGGREGATE, tenant.to_string())?;
    // The event names the administrator, so the operation log shows who changed the state.
    outbox::publish_as(
        tx.transaction(),
        &aggregate,
        &events::TenantStateChanged {
            state: changed.state.event(),
            reason: changed.reason.map(SuspensionReason::event),
            state_version,
        },
        Some(tenant),
        Actor::User {
            user_id: admin.user_id(),
        },
    )
    .await?;
    Ok(changed)
}

/// The state of the tenant state routes.
#[derive(Clone)]
pub struct TenantStateRoutes {
    verifier: IdentityVerifier,
    store: Arc<TenantStateStore>,
}

impl FromRef<TenantStateRoutes> for IdentityVerifier {
    fn from_ref(state: &TenantStateRoutes) -> Self {
        state.verifier.clone()
    }
}

/// The tenant state routes: [`PLATFORM_TENANT_STATE_PATH`].
pub fn routes(verifier: IdentityVerifier, store: TenantStateStore) -> Router {
    Router::new()
        .route(
            PLATFORM_TENANT_STATE_PATH,
            get(get_tenant_state).put(set_tenant_state),
        )
        .with_state(TenantStateRoutes {
            verifier,
            store: Arc::new(store),
        })
}

/// The checked body: the state the request asks for, or the field errors.
fn parse_body(request: &SetTenantState) -> Result<Target, ErrorBody> {
    let reason = request.reason.as_deref().map(SuspensionReason::parse);
    match (request.state.as_str(), reason) {
        ("suspended", Some(Some(reason))) => Ok(Target::Suspended(reason)),
        ("active", None) => Ok(Target::Active),
        (state, reason) => {
            let mut body = ErrorBody::new(ErrorCode::ValidationFailed);
            let valid_state = matches!(state, "active" | "suspended");
            if !valid_state {
                body = body.with_field_error(FieldError::new("/state", STATE_INVALID));
            }
            // The reason is wrong for a valid state, or is not a reason at all.
            if valid_state || matches!(reason, Some(None)) {
                body = body.with_field_error(FieldError::new("/reason", REASON_INVALID));
            }
            Err(body)
        }
    }
}

/// The tenant ID of the path, or `None` when it is not a UUID.
fn tenant_of(path: Result<Path<String>, PathRejection>) -> Option<TenantId> {
    path.ok()
        .and_then(|Path(tenant)| tenant.parse::<TenantId>().ok())
}

/// A snapshot, `200` and never cached.
fn snapshot_response(snapshot: TenantStateSnapshot) -> Response {
    (
        StatusCode::OK,
        [(CACHE_CONTROL, "no-store")],
        axum::Json(snapshot),
    )
        .into_response()
}

/// The answer to a failed read or change, logging what the database did.
fn failure_response(user: &UserIdentity, tenant: TenantId, failure: &StateError) -> Response {
    match failure {
        StateError::NotAllowed => error(ErrorBody::new(ErrorCode::PermissionDenied)),
        StateError::UnknownTenant => error(ErrorBody::new(ErrorCode::NotFound)),
        StateError::Final => {
            error(ErrorBody::new(ErrorCode::Conflict).with_message_key(STATE_FINAL))
        }
        StateError::Inconsistent(_)
        | StateError::Database(_)
        | StateError::Publish(_)
        | StateError::Aggregate(_) => {
            tracing::error!(
                user_id = %user.user_id(),
                tenant_id = %tenant,
                error_kind = failure.log_kind(),
                "could not read or change a tenant's state"
            );
            error(ErrorBody::new(ErrorCode::Unavailable))
        }
    }
}

/// Reads a tenant's state, reason and version. Platform administrators only, in their own
/// request.
#[utoipa::path(
    get,
    path = "/api/identity/v1/platform/tenants/{tenant_id}/state",
    tag = "identity",
    operation_id = "getPlatformTenantState",
    params(
        ("tenant_id" = String, Path, description = "The tenant's ID."),
    ),
    responses(
        (status = 200, description = "The tenant's state, reason and version.", body = TenantStateSnapshot),
        (status = 400, description = "The tenant ID is not a UUID (`invalid_request`).", body = ErrorBody),
        (status = 401, description = "No valid session (`unauthenticated`).", body = ErrorBody),
        (status = 403, description = "Not a platform administrator, or not the administrator's own request: another service exchanged the token (`permission_denied`).", body = ErrorBody),
        (status = 404, description = "No tenant has the ID (`not_found`).", body = ErrorBody),
        (status = 503, description = "The database failed (`unavailable`).", body = ErrorBody),
    )
)]
async fn get_tenant_state(
    State(state): State<TenantStateRoutes>,
    user: UserIdentity,
    tenant: Result<Path<String>, PathRejection>,
) -> Response {
    // Decided from the token alone, before the database is read.
    if let Err(refused) = user.require(Requirement::OwnPlatformAdmin, "read a tenant's state") {
        return refused.into_response();
    }
    let Some(tenant) = tenant_of(tenant) else {
        return error(ErrorBody::new(ErrorCode::InvalidRequest));
    };
    match state.store.read(&user, tenant).await {
        Ok(snapshot) => snapshot_response(snapshot),
        Err(failure) => failure_response(&user, tenant, &failure),
    }
}

/// Suspends a tenant with a reason, changes the reason of its suspension, or resumes it.
/// Platform administrators only, in their own request.
#[utoipa::path(
    put,
    path = "/api/identity/v1/platform/tenants/{tenant_id}/state",
    tag = "identity",
    operation_id = "setPlatformTenantState",
    params(
        ("tenant_id" = String, Path, description = "The tenant's ID."),
    ),
    request_body = SetTenantState,
    responses(
        (status = 200, description = "The tenant has the state: when the state or the reason changed, the version went up by one and `identity.tenant_state_changed` was written in the same transaction; when the tenant already had them, nothing was written.", body = TenantStateSnapshot),
        (status = 400, description = "The tenant ID is not a UUID, the `Content-Type` is not `application/json`, or the body is over 4096 bytes or not a JSON object with a string `state` and a string or null `reason` (`invalid_request`).", body = ErrorBody),
        (status = 401, description = "No valid session (`unauthenticated`).", body = ErrorBody),
        (status = 403, description = "Not a platform administrator, or not the administrator's own request: another service exchanged the token (`permission_denied`).", body = ErrorBody),
        (status = 404, description = "No tenant has the ID; nothing was written (`not_found`).", body = ErrorBody),
        (status = 409, description = "The tenant's contract was terminated or it is deleted, which is final (`conflict`, `tenant.state_final`); nothing was written and the version is unchanged.", body = ErrorBody),
        (status = 422, description = "The state is not `active` or `suspended` (`/state`, `tenant.state_invalid`), or a suspension has no known reason or a resumption has one (`/reason`, `tenant.suspension_reason_invalid`) (`validation_failed`). Nothing was written.", body = ErrorBody),
        (status = 503, description = "The database failed; nothing was written (`unavailable`).", body = ErrorBody),
    )
)]
async fn set_tenant_state(
    State(state): State<TenantStateRoutes>,
    user: UserIdentity,
    tenant: Result<Path<String>, PathRejection>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    // Decided from the token alone, before the body is read or the database touched.
    if let Err(refused) = user.require(Requirement::OwnPlatformAdmin, "change a tenant's state") {
        return refused.into_response();
    }
    let Some(tenant) = tenant_of(tenant) else {
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
    let Ok(request) = serde_json::from_slice::<SetTenantState>(&body) else {
        return error(ErrorBody::new(ErrorCode::InvalidRequest));
    };
    let target = match parse_body(&request) {
        Ok(target) => target,
        Err(refused) => return error(refused),
    };
    match state.store.set(&user, tenant, target).await {
        Ok((snapshot, _)) => snapshot_response(snapshot),
        Err(failure) => failure_response(&user, tenant, &failure),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn states_and_reasons_match_the_event_and_the_columns() {
        for state in [
            TenantState::Active,
            TenantState::Suspended,
            TenantState::Deleted,
        ] {
            assert_eq!(TenantState::parse(state.as_str()), Some(state));
            // The same spelling as the event's and the column's CHECK.
            assert_eq!(
                serde_json::to_value(state.event()).expect("serialize"),
                serde_json::Value::String(state.as_str().to_owned())
            );
            assert_eq!(
                serde_json::to_value(state).expect("serialize"),
                serde_json::Value::String(state.as_str().to_owned())
            );
        }
        for reason in [
            SuspensionReason::SecurityOrAbuse,
            SuspensionReason::NonPayment,
            SuspensionReason::ContractTerminated,
        ] {
            assert_eq!(SuspensionReason::parse(reason.as_str()), Some(reason));
            assert_eq!(
                serde_json::to_value(reason.event()).expect("serialize"),
                serde_json::Value::String(reason.as_str().to_owned())
            );
            assert_eq!(
                serde_json::to_value(reason).expect("serialize"),
                serde_json::Value::String(reason.as_str().to_owned())
            );
        }
        // `unrecognized` is how a newer value arrives in an event; it is never one to set.
        for text in ["", "Active", "ACTIVE", " active", "unrecognized", "paused"] {
            assert_eq!(TenantState::parse(text), None, "{text:?}");
        }
        for text in ["", "NonPayment", "non-payment", "unrecognized", "other"] {
            assert_eq!(SuspensionReason::parse(text), None, "{text:?}");
        }
        // The migration's CHECK lists exactly these names.
        let migration = include_str!("../migrations/postgres/0008_tenant_state.sql");
        assert!(migration.contains("CHECK (state IN ('active', 'suspended', 'deleted'))"));
        assert!(migration.contains(
            "CHECK (suspension_reason IN ('security_or_abuse', 'non_payment', 'contract_terminated'))"
        ));
    }

    fn request(state: &str, reason: Option<&str>) -> SetTenantState {
        SetTenantState {
            state: state.to_owned(),
            reason: reason.map(str::to_owned),
        }
    }

    fn fields(body: ErrorBody) -> Vec<(String, String)> {
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
    }

    #[test]
    fn the_body_is_a_suspension_with_a_reason_or_a_resumption_without_one() {
        assert_eq!(
            parse_body(&request("suspended", Some("non_payment"))).expect("valid"),
            Target::Suspended(SuspensionReason::NonPayment)
        );
        assert_eq!(
            parse_body(&request("suspended", Some("contract_terminated"))).expect("valid"),
            Target::Suspended(SuspensionReason::ContractTerminated)
        );
        assert_eq!(
            parse_body(&request("active", None)).expect("valid"),
            Target::Active
        );

        let state = ("/state".to_owned(), "tenant.state_invalid".to_owned());
        let reason = (
            "/reason".to_owned(),
            "tenant.suspension_reason_invalid".to_owned(),
        );
        for (body, expected) in [
            // A suspension needs a known reason.
            (request("suspended", None), vec![reason.clone()]),
            (request("suspended", Some("")), vec![reason.clone()]),
            (
                request("suspended", Some("unrecognized")),
                vec![reason.clone()],
            ),
            // A resumption takes none, not even a valid one.
            (request("active", Some("non_payment")), vec![reason.clone()]),
            (request("active", Some("other")), vec![reason.clone()]),
            // Deleting is not this API's, nor are unknown states.
            (request("deleted", None), vec![state.clone()]),
            (
                request("deleted", Some("contract_terminated")),
                vec![state.clone()],
            ),
            (
                request("Suspended", Some("non_payment")),
                vec![state.clone()],
            ),
            (request("unrecognized", None), vec![state.clone()]),
            // Both wrong: both listed.
            (
                request("paused", Some("whim")),
                vec![state.clone(), reason.clone()],
            ),
        ] {
            let shown = format!("{body:?}");
            assert_eq!(
                fields(parse_body(&body).expect_err(&shown)),
                expected,
                "{shown}"
            );
        }
    }

    fn current(state: TenantState, reason: Option<SuspensionReason>) -> TenantStateSnapshot {
        TenantStateSnapshot {
            tenant_id: TenantId::generate(),
            state,
            reason,
            state_version: 3,
            state_changed_at: DateTime::UNIX_EPOCH,
            status: TenantStatus::of(state, 3, 0),
        }
    }

    #[test]
    fn a_terminated_contract_and_a_deleted_tenant_are_final() {
        use SuspensionReason::{ContractTerminated, NonPayment, SecurityOrAbuse};
        let targets = [
            Target::Active,
            Target::Suspended(SecurityOrAbuse),
            Target::Suspended(NonPayment),
            Target::Suspended(ContractTerminated),
        ];
        // Active and other suspensions change freely.
        for from in [
            current(TenantState::Active, None),
            current(TenantState::Suspended, Some(SecurityOrAbuse)),
            current(TenantState::Suspended, Some(NonPayment)),
        ] {
            for target in targets {
                assert!(!is_final(&from, target), "{from:?} -> {target:?}");
            }
        }
        // A terminated contract only "changes" to itself, which is no change.
        let terminated = current(TenantState::Suspended, Some(ContractTerminated));
        assert!(!is_final(
            &terminated,
            Target::Suspended(ContractTerminated)
        ));
        for target in [
            Target::Active,
            Target::Suspended(SecurityOrAbuse),
            Target::Suspended(NonPayment),
        ] {
            assert!(is_final(&terminated, target), "{target:?}");
        }
        // A deleted tenant changes to nothing this API sets.
        for from in [
            current(TenantState::Deleted, None),
            current(TenantState::Deleted, Some(ContractTerminated)),
        ] {
            for target in targets {
                assert!(is_final(&from, target), "{from:?} -> {target:?}");
            }
        }
    }

    #[test]
    fn the_spec_lists_both_routes_and_their_answers() {
        let spec = serde_json::to_value(crate::openapi()).expect("the spec");
        let read = &spec["paths"][PLATFORM_TENANT_STATE_PATH]["get"];
        assert_eq!(read["operationId"], "getPlatformTenantState");
        for status in ["200", "400", "401", "403", "404", "503"] {
            assert!(read["responses"][status].is_object(), "GET {status}");
        }
        let set = &spec["paths"][PLATFORM_TENANT_STATE_PATH]["put"];
        assert_eq!(set["operationId"], "setPlatformTenantState");
        for status in ["200", "400", "401", "403", "404", "409", "422", "503"] {
            assert!(set["responses"][status].is_object(), "PUT {status}");
        }
        let schemas = &spec["components"]["schemas"];
        // A request offers only the states the API sets; a response may show any.
        assert_eq!(
            schemas["SetTenantState"]["properties"]["state"]["$ref"],
            "#/components/schemas/RequestedTenantState"
        );
        assert_eq!(
            schemas["RequestedTenantState"]["enum"],
            serde_json::json!(["active", "suspended"])
        );
        assert_eq!(
            schemas["TenantState"]["enum"],
            serde_json::json!(["active", "suspended", "deleted"])
        );
        assert_eq!(
            schemas["SuspensionReason"]["enum"],
            serde_json::json!(["security_or_abuse", "non_payment", "contract_terminated"])
        );
        assert_eq!(
            schemas["TenantStatus"]["enum"],
            serde_json::json!(["active", "processing", "suspended", "deleted"])
        );
        assert_eq!(
            schemas["TenantStateSnapshot"]["properties"]["status"]["$ref"],
            "#/components/schemas/TenantStatus"
        );
    }

    #[test]
    fn a_cut_off_is_processing_until_certs_confirmed_its_version() {
        use TenantState::{Active, Deleted, Suspended};
        use TenantStatus as S;
        // `(state, state_version, confirmed version)` and what the back office shows.
        for (state, version, applied, expected) in [
            // Active at once, confirmed or not: a resumption waits for nothing.
            (Active, 1, 0, S::Active),
            (Active, 4, 0, S::Active),
            (Active, 4, 3, S::Active),
            (Active, 4, 4, S::Active),
            // A suspension: processing until its own version or a newer one is confirmed.
            (Suspended, 2, 0, S::Processing),
            (Suspended, 2, 1, S::Processing),
            (Suspended, 2, 2, S::Suspended),
            // An older confirmation (of the suspension before a resumption, say) is not enough.
            (Suspended, 5, 2, S::Processing),
            (Suspended, 5, 4, S::Processing),
            (Suspended, 5, 5, S::Suspended),
            // The database never stores a confirmation ahead of the state; were one read, it
            // would cover the state.
            (Suspended, 5, 6, S::Suspended),
            (Deleted, 7, 6, S::Processing),
            (Deleted, 7, 7, S::Deleted),
        ] {
            assert_eq!(
                TenantStatus::of(state, version, applied),
                expected,
                "{state:?} at {version}, confirmed {applied}"
            );
        }
        // The same names as the state where they overlap.
        for (status, name) in [
            (S::Active, "active"),
            (S::Processing, "processing"),
            (S::Suspended, "suspended"),
            (S::Deleted, "deleted"),
        ] {
            assert_eq!(
                serde_json::to_value(status).expect("serialize"),
                serde_json::Value::String(name.to_owned())
            );
        }
    }
}
