//! AI model roles (Laysi/trustiot-v2#608; services/identity/README.md, "AI 模型角色"): platform
//! administrators read the AI gateway's model roles, change a role's models and reset a role to
//! its configured models. The identity service decides who may (a platform administrator's own
//! request, after the cookie check), then forwards the request to the AI gateway's admin API,
//! which nobody else can reach (Laysi/trustiot-v2#606, Laysi/trustiot-v2#607) and which decides
//! whether the change itself is valid. After a change took effect, the service writes
//! `identity.ai_model_role_changed` to its outbox, so the platform's operation log shows it.
//!
//! | Request | Response |
//! | --- | --- |
//! | No valid internal identity token | 401 `unauthenticated` |
//! | Not a platform administrator (a tenant `admin` neither), or a token another service exchanged (`act`) | 403 `permission_denied` |
//! | `PUT`, `DELETE`: cross-site ([`crate::csrf::cross_site`], checked again after the cookie check) | 403 `permission_denied`, message key `security.cross_site_request` |
//! | `PUT`, `DELETE`: a `Content-Type` other than `application/json` (parameters allowed) | 400 `invalid_request` |
//! | `TRUSTIOT_AI_GATEWAY_ADMIN_URL` not set or unusable | 503 `unavailable` |
//! | A role that is not a role name ([`ai_gateway_api::is_role_name`]) | 404 `not_found`, message key `ai_model_role.not_found` |
//! | `PUT`: a body over [`MAX_BODY_BYTES`], or not a JSON object with a string array `models` and an integer `expected_version` of at least 0; `DELETE`: no such `expected_version` query parameter | 400 `invalid_request` |
//! | `PUT`: not 1 to [`MAX_MODELS`] models | 422 `validation_failed`, field `/models`, key `ai_model_role.invalid_models` |
//! | Then from the AI gateway: | |
//! | 404 `role_not_found` | 404 `not_found`, key `ai_model_role.not_found` |
//! | 409 `version_conflict` | 409 `conflict`, key `ai_model_role.version_conflict`, `details.current_version` |
//! | 422 `invalid_models`, `unknown_provider`, `protocol_mismatch` | 422 `validation_failed`, field `/models`, key `ai_model_role.<code>` |
//! | Not reached, too many calls in flight, or a refusal that means a bug (400, 413, an unknown route) | 503 `unavailable`; nothing was changed |
//! | 503, 504, any other answer whose body or code this build cannot read, a 2xx it cannot read, a timeout, or a connection lost after sending | 503 `unavailable`; for `PUT` and `DELETE` key `ai_model_role.outcome_unknown`: the change may have taken effect, reload the roles |
//! | Success | 200 with [`ModelRoles`] (`GET`) or the role after the change ([`ModelRole`]) |
//!
//! The checks run in that order, and nothing is sent to the gateway before all of the service's
//! own checks passed. A change is never sent twice: an unclear outcome is the caller's to
//! resolve by reading the roles again. Every response of the handlers carries
//! `cache-control: no-store`.
//!
//! The service sends the gateway the administrator as `ai-gateway-actor: user:<user ID>`, which
//! it builds from the verified token; a header of that name in the user's request is never
//! forwarded, nor is any other header of it (cookie, token). It logs the operation, the role,
//! the user ID, the gateway's status and error code, never a body or a header.

pub mod admin_client;

use std::sync::Arc;
use std::time::Duration;

use ai_gateway_api as api;
use axum::Router;
use axum::body::Body;
use axum::extract::rejection::{PathRejection, QueryRejection};
use axum::extract::{FromRef, Path, Query, State};
use axum::http::header::CACHE_CONTROL;
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use chrono::{DateTime, Utc};
use contract::event::Actor;
use contract::events::identity::{AI_MODEL_ROLE_AGGREGATE, AiModelRoleAction, AiModelRoleChanged};
use contract::{ErrorBody, ErrorCode, FieldError, MessageKey, UserId};
use outbox::Aggregate;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use service_kit::identity::{IdentityVerifier, UserIdentity};
use service_kit::tenant::{PlatformActor, PlatformDb, TenantDbError};
use utoipa::ToSchema;

use crate::csrf::{CROSS_SITE_REQUEST, cross_site};
use crate::current_session::json_object;
use crate::members::is_own_platform_admin;
use crate::tenants::{error, is_json};

pub use admin_client::AdminClient;
use admin_client::{Call, Exchange};

/// Every model role, and the configured providers.
pub const MODEL_ROLES_PATH: &str = "/api/identity/v1/platform/ai/model-roles";

/// One model role: `PUT` changes its models, `DELETE` resets it.
pub const MODEL_ROLE_PATH: &str = "/api/identity/v1/platform/ai/model-roles/{role}";

/// The largest `PUT` body read, the AI gateway's own limit. It is read only after the
/// permission check, so others cannot make the service buffer it.
pub const MAX_BODY_BYTES: usize = 16 << 10;

/// The most models a role may have, as the AI gateway allows.
pub const MAX_MODELS: usize = 8;

/// How long writing `identity.ai_model_role_changed` may take, waiting for a connection of the
/// platform role's pool included.
pub const EVENT_TIMEOUT: Duration = Duration::from_secs(5);

/// The message key of a role the AI gateway's configuration does not have.
pub const ROLE_NOT_FOUND: MessageKey = MessageKey::new("ai_model_role.not_found");

/// The message key of a change or reset that expected another version than the role has now;
/// `details.current_version` is the role's version when the gateway told it.
pub const VERSION_CONFLICT: MessageKey = MessageKey::new("ai_model_role.version_conflict");

/// The message key of a model list that is not 1 to 8 distinct, well-formed
/// `<provider>/<model>`.
pub const INVALID_MODELS: MessageKey = MessageKey::new("ai_model_role.invalid_models");

/// The message key of a model whose provider the AI gateway's configuration does not have.
pub const UNKNOWN_PROVIDER: MessageKey = MessageKey::new("ai_model_role.unknown_provider");

/// The message key of a model whose provider speaks another protocol than the role.
pub const PROTOCOL_MISMATCH: MessageKey = MessageKey::new("ai_model_role.protocol_mismatch");

/// The message key of a change or reset whose outcome is unknown: it may have taken effect, so
/// the page reads the roles again instead of repeating it.
pub const OUTCOME_UNKNOWN: MessageKey = MessageKey::new("ai_model_role.outcome_unknown");

/// Names the platform role's transaction in its INFO log line (`platform database access`).
const EVENT_PURPOSE: &str = "ai-model-role-changed";

/// Where a role's models in effect come from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ModelSource {
    /// The configuration (the chart's `ai.roles`): never changed, reset, or the stored list is
    /// ignored.
    Configuration,
    /// A list a platform administrator stored.
    Database,
}

/// A stored model list the AI gateway does not use because it no longer fits the
/// configuration; the role uses its configured models.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct IgnoredModels {
    /// The stored models.
    pub models: Vec<String>,
    /// Why, in English, as the AI gateway explains it (for operators; not translated).
    pub reason: String,
}

/// A model role of the AI gateway.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ModelRole {
    /// The role's name, such as `main`.
    pub role: String,
    /// The role's protocol, which only the configuration sets: `anthropic-messages` or
    /// `openai-decisions` today.
    pub protocol: String,
    /// The models in effect, `<provider>/<model>` in fallback order.
    pub models: Vec<String>,
    /// The models the configuration gives the role, which a reset goes back to.
    pub configured_models: Vec<String>,
    /// Where `models` come from.
    pub source: ModelSource,
    /// The version a change or reset must send as `expected_version`; 0 when the role was never
    /// changed.
    pub version: i64,
    /// A stored list that is not in use; `null` when there is none.
    #[schema(required = true)]
    pub ignored: Option<IgnoredModels>,
    /// When the role was last changed or reset; `null` when it never was, or the AI gateway
    /// could not say.
    #[schema(required = true)]
    pub updated_at: Option<DateTime<Utc>>,
    /// Who last changed or reset the role, as the AI gateway recorded it (`user:<user ID>` for
    /// changes made here); `null` when nobody did.
    #[schema(required = true)]
    pub updated_by: Option<String>,
}

/// A provider configured in the AI gateway: a role's models may name it when its protocol is
/// the role's.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ModelProvider {
    /// The provider's name, the `<provider>` of a model.
    pub name: String,
    /// `anthropic-messages` or `openai-decisions` today.
    pub protocol: String,
}

/// Every model role and every configured provider, by name.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ModelRoles {
    /// Every configured role.
    pub roles: Vec<ModelRole>,
    /// Every configured provider.
    pub providers: Vec<ModelProvider>,
}

/// The body of `PUT /api/identity/v1/platform/ai/model-roles/{role}`. Unknown fields are
/// ignored.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, ToSchema)]
pub struct SetRoleModels {
    /// 1 to 8 distinct `<provider>/<model>` in fallback order, first choice first, of configured
    /// providers whose protocol is the role's.
    pub models: Vec<String>,
    /// The role's `version` as the caller read it; the change is refused with 409 when the role
    /// has another version by then.
    pub expected_version: i64,
}

/// The query of `DELETE /api/identity/v1/platform/ai/model-roles/{role}`.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct ResetRole {
    /// The role's `version` as the caller read it; the reset is refused with 409 when the role
    /// has another version by then.
    pub expected_version: i64,
}

impl From<api::Role> for ModelRole {
    fn from(role: api::Role) -> Self {
        Self {
            role: role.role,
            protocol: role.protocol,
            models: role.models,
            configured_models: role.configured_models,
            source: match role.source {
                api::Source::Configuration => ModelSource::Configuration,
                api::Source::Database => ModelSource::Database,
            },
            version: role.version,
            ignored: role.ignored.map(|ignored| IgnoredModels {
                models: ignored.models,
                reason: ignored.reason,
            }),
            updated_at: role
                .updated_at
                .and_then(|time| DateTime::parse_from_rfc3339(&time).ok())
                .map(|time| time.with_timezone(&Utc)),
            updated_by: role.updated_by,
        }
    }
}

impl From<api::Roles> for ModelRoles {
    fn from(roles: api::Roles) -> Self {
        Self {
            roles: roles.roles.into_iter().map(ModelRole::from).collect(),
            providers: roles
                .providers
                .into_iter()
                .map(|provider| ModelProvider {
                    name: provider.name,
                    protocol: provider.protocol,
                })
                .collect(),
        }
    }
}

/// What the route asked the gateway to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Operation {
    List,
    Set,
    Reset,
}

impl Operation {
    const fn as_str(self) -> &'static str {
        match self {
            Self::List => "list",
            Self::Set => "set",
            Self::Reset => "reset",
        }
    }

    const fn changes(self) -> bool {
        !matches!(self, Self::List)
    }
}

/// Why a call to the gateway did not give the answer the route wanted.
#[derive(Debug, PartialEq, Eq)]
enum Failure {
    /// Nothing was sent.
    NotSent(&'static str),
    /// Sent, but no whole answer came back.
    Lost(&'static str),
    /// The gateway refused with an error code this build knows, and the status that code has.
    Refused {
        status: u16,
        code: api::ErrorCode,
        current_version: Option<i64>,
    },
    /// A non-2xx answer without a readable error body, with an unknown code, or with a code
    /// whose status is another one.
    Unrecognized { status: u16 },
    /// A 2xx answer that is not what the route expected.
    UnreadableSuccess { status: u16 },
}

/// How a failure is logged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Severity {
    /// The administrator's own mistake or a race (404, 409, 422): INFO.
    Expected,
    /// The gateway or its database is down or busy: WARN.
    Unavailable,
    /// Something the service cannot explain or a bug: ERROR.
    Fault,
}

impl Failure {
    /// The M1 answer for `operation`.
    fn body(&self, operation: Operation) -> ErrorBody {
        let unclear = || {
            let body = ErrorBody::new(ErrorCode::Unavailable);
            if operation.changes() {
                body.with_message_key(OUTCOME_UNKNOWN)
            } else {
                body
            }
        };
        let models = |key| {
            ErrorBody::new(ErrorCode::ValidationFailed)
                .with_field_error(FieldError::new("/models", key))
        };
        match self {
            Self::NotSent(_) => ErrorBody::new(ErrorCode::Unavailable),
            Self::Lost(_) | Self::Unrecognized { .. } | Self::UnreadableSuccess { .. } => unclear(),
            Self::Refused {
                code,
                current_version,
                ..
            } => match code {
                api::ErrorCode::RoleNotFound => {
                    ErrorBody::new(ErrorCode::NotFound).with_message_key(ROLE_NOT_FOUND)
                }
                api::ErrorCode::VersionConflict => {
                    let body =
                        ErrorBody::new(ErrorCode::Conflict).with_message_key(VERSION_CONFLICT);
                    match current_version {
                        Some(version) => body.with_detail("current_version", *version),
                        None => body,
                    }
                }
                api::ErrorCode::InvalidModels => models(INVALID_MODELS),
                api::ErrorCode::UnknownProvider => models(UNKNOWN_PROVIDER),
                api::ErrorCode::ProtocolMismatch => models(PROTOCOL_MISMATCH),
                // The gateway's database failed: a change may have committed before it did
                // (Laysi/trustiot-v2#704), so the outcome is unclear.
                api::ErrorCode::Unavailable => unclear(),
                // Refusals of a request the service checked and built itself: a bug or a
                // mismatched gateway, and the gateway changed nothing.
                api::ErrorCode::InvalidRequest
                | api::ErrorCode::InvalidActor
                | api::ErrorCode::RequestTooLarge
                | api::ErrorCode::NotFound
                | api::ErrorCode::Unknown => ErrorBody::new(ErrorCode::Unavailable),
            },
        }
    }

    fn severity(&self) -> Severity {
        match self {
            Self::Refused {
                code:
                    api::ErrorCode::RoleNotFound
                    | api::ErrorCode::VersionConflict
                    | api::ErrorCode::InvalidModels
                    | api::ErrorCode::UnknownProvider
                    | api::ErrorCode::ProtocolMismatch,
                ..
            } => Severity::Expected,
            Self::Refused {
                code: api::ErrorCode::Unavailable,
                ..
            }
            | Self::NotSent("too many calls in flight") => Severity::Unavailable,
            _ => Severity::Fault,
        }
    }

    /// The failure's kind, for the log: never a body.
    fn kind(&self) -> &'static str {
        match self {
            Self::NotSent(why) | Self::Lost(why) => why,
            Self::Refused { code, .. } => code.as_str(),
            Self::Unrecognized { .. } => "an answer this build cannot read",
            Self::UnreadableSuccess { .. } => "a success this build cannot read",
        }
    }

    fn status(&self) -> Option<u16> {
        match self {
            Self::NotSent(_) | Self::Lost(_) => None,
            Self::Refused { status, .. }
            | Self::Unrecognized { status }
            | Self::UnreadableSuccess { status } => Some(*status),
        }
    }
}

/// The gateway's answer as `T`: the body of a 2xx, or why not. Only the HTTP status decides
/// between success and failure; a non-2xx is a failure whatever its body says.
fn read_answer<T: DeserializeOwned>(exchange: Exchange) -> Result<T, Failure> {
    let (status, body) = match exchange {
        Exchange::Answered { status, body } => (status, body),
        Exchange::NotSent(why) => return Err(Failure::NotSent(why)),
        Exchange::Lost(why) => return Err(Failure::Lost(why)),
    };
    let code = status.as_u16();
    if status.is_success() {
        return serde_json::from_slice(&body)
            .map_err(|_| Failure::UnreadableSuccess { status: code });
    }
    match serde_json::from_slice::<api::ErrorBody>(&body) {
        Ok(api::ErrorBody { error })
            if error.code != api::ErrorCode::Unknown && error.code.status() == code =>
        {
            Err(Failure::Refused {
                status: code,
                code: error.code,
                current_version: error.current_version,
            })
        }
        _ => Err(Failure::Unrecognized { status: code }),
    }
}

/// The state of the model role routes.
#[derive(Clone)]
pub struct ModelRoleRoutes {
    verifier: IdentityVerifier,
    gateway: Option<Arc<AdminClient>>,
    events: Option<PlatformDb>,
}

impl FromRef<ModelRoleRoutes> for IdentityVerifier {
    fn from_ref(state: &ModelRoleRoutes) -> Self {
        state.verifier.clone()
    }
}

/// The admin API's client for `url` (`TRUSTIOT_AI_GATEWAY_ADMIN_URL`); `None`, logged, when it
/// is not set (WARN) or not usable (ERROR). The routes answer 503 then; nothing else depends on
/// it.
#[must_use]
pub fn admin_client(url: Option<&str>) -> Option<AdminClient> {
    let Some(url) = url else {
        tracing::warn!(
            "TRUSTIOT_AI_GATEWAY_ADMIN_URL is not set (the chart sets it while the AI gateway \
             runs): the AI model role API answers 503"
        );
        return None;
    };
    match AdminClient::new(url) {
        Ok(client) => {
            tracing::info!(
                ?client,
                "calling the AI gateway's admin API for model roles"
            );
            Some(client)
        }
        Err(rule) => {
            tracing::error!(
                rule,
                "TRUSTIOT_AI_GATEWAY_ADMIN_URL cannot be used: the AI model role API answers 503 \
                 until it is fixed and the service restarts"
            );
            None
        }
    }
}

/// The model role routes: [`MODEL_ROLES_PATH`] and [`MODEL_ROLE_PATH`]. `gateway` is
/// [`admin_client`]'s; `events` is the platform role's pool, which writes
/// `identity.ai_model_role_changed` (`None` without the platform role's credentials: changes
/// then work, and each logs an ERROR that the operation log misses it).
pub fn routes(
    verifier: IdentityVerifier,
    gateway: Option<AdminClient>,
    events: Option<PlatformDb>,
) -> Router {
    Router::new()
        .route(MODEL_ROLES_PATH, get(list_model_roles))
        .route(
            MODEL_ROLE_PATH,
            axum::routing::put(set_model_role).delete(reset_model_role),
        )
        .with_state(ModelRoleRoutes {
            verifier,
            gateway: gateway.map(Arc::new),
            events,
        })
}

/// Refuses every caller but a platform administrator's own request, from the token alone.
fn refuse_caller(user: &UserIdentity, operation: Operation) -> Option<Response> {
    if is_own_platform_admin(user) {
        return None;
    }
    tracing::info!(
        user_id = %user.user_id(),
        exchanged = user.claims().act.is_some(),
        operation = operation.as_str(),
        "refused an AI model role request: not a platform administrator, or not the \
         administrator's own request"
    );
    Some(error(ErrorBody::new(ErrorCode::PermissionDenied)))
}

/// Refuses a change that is not a same-site JSON request, before its body is read: a
/// cross-site request, checked again although the cookie check refused it already, in case the
/// gateway stops passing it the headers (README, "跨站請求"); and a `Content-Type` other than
/// JSON (README, "租戶").
fn refuse_write(
    method: &Method,
    headers: &HeaderMap,
    user: &UserIdentity,
    operation: Operation,
) -> Option<Response> {
    if let Some(header) = cross_site(method, headers) {
        tracing::info!(
            user_id = %user.user_id(),
            reason = "cross_site_request",
            header,
            operation = operation.as_str(),
            "refused an AI model role change"
        );
        return Some(error(
            ErrorBody::new(ErrorCode::PermissionDenied).with_message_key(CROSS_SITE_REQUEST),
        ));
    }
    if !is_json(headers) {
        return Some(error(ErrorBody::new(ErrorCode::InvalidRequest)));
    }
    None
}

/// The configured client, or the 503 without one (boxed: a `Response` is large).
fn gateway(state: &ModelRoleRoutes) -> Result<Arc<AdminClient>, Box<Response>> {
    state
        .gateway
        .clone()
        .ok_or_else(|| Box::new(error(ErrorBody::new(ErrorCode::Unavailable))))
}

/// The checked role of the path, or the 404 for a name no role can have.
fn role_name(role: Result<Path<String>, PathRejection>) -> Result<String, Box<Response>> {
    match role {
        Ok(Path(role)) if api::is_role_name(&role) => Ok(role),
        _ => Err(Box::new(error(
            ErrorBody::new(ErrorCode::NotFound).with_message_key(ROLE_NOT_FOUND),
        ))),
    }
}

/// The answer for `failure`, logged.
fn failed(
    failure: &Failure,
    user: &UserIdentity,
    operation: Operation,
    role: Option<&str>,
) -> Response {
    let user_id = user.user_id();
    let operation_name = operation.as_str();
    let status = failure.status();
    let kind = failure.kind();
    match failure.severity() {
        Severity::Expected => tracing::info!(
            %user_id,
            operation = operation_name,
            role,
            status,
            kind,
            "the AI gateway refused a model role request"
        ),
        Severity::Unavailable => tracing::warn!(
            %user_id,
            operation = operation_name,
            role,
            status,
            kind,
            "the AI gateway could not serve a model role request"
        ),
        Severity::Fault => tracing::error!(
            %user_id,
            operation = operation_name,
            role,
            status,
            kind,
            unclear_change = operation.changes(),
            "a model role request to the AI gateway failed; a change may have taken effect when \
             unclear_change is true"
        ),
    }
    error(failure.body(operation))
}

/// The actor the gateway records for `user`.
fn actor(user: &UserIdentity) -> String {
    format!("user:{}", user.user_id())
}

/// A successful answer, never cached.
fn ok<T: Serialize>(body: T) -> Response {
    (
        StatusCode::OK,
        [(CACHE_CONTROL, "no-store")],
        axum::Json(body),
    )
        .into_response()
}

/// Lists the AI gateway's model roles and providers. Platform administrators only, in their own
/// request.
#[utoipa::path(
    get,
    path = "/api/identity/v1/platform/ai/model-roles",
    tag = "identity",
    operation_id = "listPlatformAiModelRoles",
    responses(
        (status = 200, description = "Every model role of the AI gateway, with the models in effect, the configured ones and where they come from, and every configured provider.", body = ModelRoles),
        (status = 401, description = "No valid session (`unauthenticated`).", body = ErrorBody),
        (status = 403, description = "Not a platform administrator, or not the administrator's own request: another service exchanged the token (`permission_denied`).", body = ErrorBody),
        (status = 503, description = "The AI gateway's admin API is not configured, not reachable, busy or failed (`unavailable`).", body = ErrorBody),
    )
)]
async fn list_model_roles(State(state): State<ModelRoleRoutes>, user: UserIdentity) -> Response {
    if let Some(refusal) = refuse_caller(&user, Operation::List) {
        return refusal;
    }
    let gateway = match gateway(&state) {
        Ok(gateway) => gateway,
        Err(unavailable) => return *unavailable,
    };
    let actor = actor(&user);
    let exchange = gateway
        .call(Call {
            method: Method::GET,
            path: api::ROLES_PATH,
            expected_version: None,
            actor: &actor,
            json: None,
        })
        .await;
    match read_answer::<api::Roles>(exchange) {
        Ok(roles) => ok(ModelRoles::from(roles)),
        Err(failure) => failed(&failure, &user, Operation::List, None),
    }
}

/// Changes a model role's models. Platform administrators only, in their own request.
#[utoipa::path(
    put,
    path = "/api/identity/v1/platform/ai/model-roles/{role}",
    tag = "identity",
    operation_id = "setPlatformAiModelRole",
    params(
        ("role" = String, Path, description = "The role's name, such as `main`."),
    ),
    request_body = SetRoleModels,
    responses(
        (status = 200, description = "The models were changed: the role as it is now, with its new `version`; `identity.ai_model_role_changed` is in the platform's operation log.", body = ModelRole),
        (status = 400, description = "The `Content-Type` is not `application/json`, or the body is over 16 KiB or not a JSON object with a string array `models` and an integer `expected_version` of at least 0 (`invalid_request`); nothing was sent.", body = ErrorBody),
        (status = 401, description = "No valid session (`unauthenticated`).", body = ErrorBody),
        (status = 403, description = "Not a platform administrator, not the administrator's own request (`permission_denied`), or a cross-site request (`permission_denied`, message key `security.cross_site_request`); nothing was sent.", body = ErrorBody),
        (status = 404, description = "The AI gateway has no such role (`not_found`, message key `ai_model_role.not_found`).", body = ErrorBody),
        (status = 409, description = "The role is no longer at `expected_version` (`conflict`, message key `ai_model_role.version_conflict`, `details.current_version` when known); nothing was changed.", body = ErrorBody),
        (status = 422, description = "The models were refused (`validation_failed`, field `/models`): not 1 to 8 distinct, well-formed `<provider>/<model>` (`ai_model_role.invalid_models`), a provider that is not configured (`ai_model_role.unknown_provider`), or a provider of another protocol than the role (`ai_model_role.protocol_mismatch`); nothing was changed.", body = ErrorBody),
        (status = 503, description = "The AI gateway's admin API is not configured, not reachable or busy (`unavailable`; nothing was changed), or the outcome is unknown (`unavailable`, message key `ai_model_role.outcome_unknown`): the change may have taken effect, so read the roles again before retrying.", body = ErrorBody),
    )
)]
async fn set_model_role(
    State(state): State<ModelRoleRoutes>,
    user: UserIdentity,
    role: Result<Path<String>, PathRejection>,
    method: Method,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let operation = Operation::Set;
    if let Some(refusal) = refuse_caller(&user, operation)
        .or_else(|| refuse_write(&method, &headers, &user, operation))
    {
        return refusal;
    }
    let gateway = match gateway(&state) {
        Ok(gateway) => gateway,
        Err(unavailable) => return *unavailable,
    };
    let role = match role_name(role) {
        Ok(role) => role,
        Err(not_found) => return *not_found,
    };
    let Ok(body) = axum::body::to_bytes(body, MAX_BODY_BYTES).await else {
        return error(ErrorBody::new(ErrorCode::InvalidRequest));
    };
    let Some(request) = json_object::<SetRoleModels>(&body) else {
        return error(ErrorBody::new(ErrorCode::InvalidRequest));
    };
    if request.expected_version < 0 {
        return error(ErrorBody::new(ErrorCode::InvalidRequest));
    }
    if !(1..=MAX_MODELS).contains(&request.models.len()) {
        return error(
            ErrorBody::new(ErrorCode::ValidationFailed)
                .with_field_error(FieldError::new("/models", INVALID_MODELS)),
        );
    }
    let expected_version = request.expected_version;
    // The service's own body, never the user's bytes: only these two fields reach the gateway.
    let Ok(json) = serde_json::to_vec(&api::SetModels {
        models: request.models,
        expected_version,
    }) else {
        return error(ErrorBody::new(ErrorCode::Internal));
    };
    let path = api::role_path(&role).unwrap_or_default();
    let actor = actor(&user);
    let exchange = gateway
        .call(Call {
            method: Method::PUT,
            path: &path,
            expected_version: None,
            actor: &actor,
            json: Some(json),
        })
        .await;
    changed(&state, &user, operation, &role, expected_version, exchange).await
}

/// Resets a model role to the models the configuration gives it. Platform administrators only,
/// in their own request.
#[utoipa::path(
    delete,
    path = "/api/identity/v1/platform/ai/model-roles/{role}",
    tag = "identity",
    operation_id = "resetPlatformAiModelRole",
    params(
        ("role" = String, Path, description = "The role's name, such as `main`."),
        ("expected_version" = i64, Query, minimum = 0, description = "The role's `version` as the caller read it; the reset is refused with 409 when the role has another version by then."),
    ),
    responses(
        (status = 200, description = "The role uses its configured models: the role as it is now. A role that had no stored models is unchanged, keeps its `version` and writes no event; otherwise `identity.ai_model_role_changed` is in the platform's operation log.", body = ModelRole),
        (status = 400, description = "The `Content-Type` is not `application/json` (send it without a body), or the query has no integer `expected_version` of at least 0 (`invalid_request`); nothing was sent.", body = ErrorBody),
        (status = 401, description = "No valid session (`unauthenticated`).", body = ErrorBody),
        (status = 403, description = "Not a platform administrator, not the administrator's own request (`permission_denied`), or a cross-site request (`permission_denied`, message key `security.cross_site_request`); nothing was sent.", body = ErrorBody),
        (status = 404, description = "The AI gateway has no such role (`not_found`, message key `ai_model_role.not_found`).", body = ErrorBody),
        (status = 409, description = "The role is no longer at `expected_version` (`conflict`, message key `ai_model_role.version_conflict`, `details.current_version` when known); nothing was changed.", body = ErrorBody),
        (status = 503, description = "The AI gateway's admin API is not configured, not reachable or busy (`unavailable`; nothing was changed), or the outcome is unknown (`unavailable`, message key `ai_model_role.outcome_unknown`): the reset may have taken effect, so read the roles again before retrying.", body = ErrorBody),
    )
)]
async fn reset_model_role(
    State(state): State<ModelRoleRoutes>,
    user: UserIdentity,
    role: Result<Path<String>, PathRejection>,
    query: Result<Query<ResetRole>, QueryRejection>,
    method: Method,
    headers: HeaderMap,
) -> Response {
    let operation = Operation::Reset;
    if let Some(refusal) = refuse_caller(&user, operation)
        .or_else(|| refuse_write(&method, &headers, &user, operation))
    {
        return refusal;
    }
    let gateway = match gateway(&state) {
        Ok(gateway) => gateway,
        Err(unavailable) => return *unavailable,
    };
    let role = match role_name(role) {
        Ok(role) => role,
        Err(not_found) => return *not_found,
    };
    let Ok(Query(ResetRole { expected_version })) = query else {
        return error(ErrorBody::new(ErrorCode::InvalidRequest));
    };
    if expected_version < 0 {
        return error(ErrorBody::new(ErrorCode::InvalidRequest));
    }
    let path = api::role_path(&role).unwrap_or_default();
    let actor = actor(&user);
    let exchange = gateway
        .call(Call {
            method: Method::DELETE,
            path: &path,
            expected_version: Some(expected_version),
            actor: &actor,
            json: None,
        })
        .await;
    changed(&state, &user, operation, &role, expected_version, exchange).await
}

/// The answer to a change or reset, after recording it when it changed the role.
async fn changed(
    state: &ModelRoleRoutes,
    user: &UserIdentity,
    operation: Operation,
    role: &str,
    expected_version: i64,
    exchange: Exchange,
) -> Response {
    let answer = match read_answer::<api::Role>(exchange) {
        // An answer about another role is not an answer to this request.
        Ok(answer) if answer.role == role => answer,
        Ok(_) => {
            return failed(
                &Failure::UnreadableSuccess { status: 200 },
                user,
                operation,
                Some(role),
            );
        }
        Err(failure) => return failed(&failure, user, operation, Some(role)),
    };
    let action = if operation == Operation::Reset {
        AiModelRoleAction::Reset
    } else {
        AiModelRoleAction::Set
    };
    // A reset of a role without stored models changes nothing and keeps the version.
    if answer.version != expected_version {
        tracing::info!(
            user_id = %user.user_id(),
            operation = operation.as_str(),
            role,
            version = answer.version,
            "changed an AI model role"
        );
        let event = AiModelRoleChanged {
            role: role.to_owned(),
            action,
            version: answer.version,
            models: answer.models.clone(),
        };
        record(state.events.clone(), user, event).await;
    }
    ok(ModelRole::from(answer))
}

/// Why the event was not written.
#[derive(Debug, thiserror::Error)]
enum RecordError {
    #[error("the event log is off: the platform role has no credentials")]
    Off,
    #[error("not a platform administrator")]
    NotPlatformAdmin,
    #[error("timed out")]
    TimedOut,
    #[error("the task writing it failed")]
    Task,
    #[error(transparent)]
    Database(#[from] TenantDbError),
    #[error(transparent)]
    Publish(#[from] outbox::PublishError),
    #[error(transparent)]
    Aggregate(#[from] outbox::InvalidAggregate),
}

impl RecordError {
    /// A short description for the log: the SQLSTATE or the kind, never data.
    fn log_kind(&self) -> String {
        match self {
            Self::Database(TenantDbError::Database(error)) => crate::session::error_kind(error),
            // `PublishError` prints only the SQLSTATE or kind, `InvalidAggregate` only the rule.
            _ => self.to_string(),
        }
    }
}

/// Writes `event` for `user` as the platform role, in a task of its own so it completes even
/// when the request is dropped; logs an ERROR when it cannot. The change already took effect and
/// the AI gateway's change log has it, so a failure here never changes the answer.
async fn record(events: Option<PlatformDb>, user: &UserIdentity, event: AiModelRoleChanged) {
    let user_id = user.user_id();
    let role = event.role.clone();
    let version = event.version;
    // `refuse_caller` let only platform administrators through.
    let actor = PlatformActor::admin(user);
    let written = tokio::spawn(async move {
        let (Some(events), Some(actor)) = (events, actor) else {
            return Err(if actor.is_none() {
                RecordError::NotPlatformAdmin
            } else {
                RecordError::Off
            });
        };
        tokio::time::timeout(EVENT_TIMEOUT, write_event(&events, actor, user_id, &event))
            .await
            .unwrap_or(Err(RecordError::TimedOut))
    })
    .await
    .unwrap_or(Err(RecordError::Task));
    if let Err(failure) = written {
        tracing::error!(
            %user_id,
            role,
            version,
            error_kind = failure.log_kind(),
            "could not write identity.ai_model_role_changed: the operation log misses this change \
             of an AI model role; the AI gateway's change log has it"
        );
    }
}

async fn write_event(
    events: &PlatformDb,
    actor: PlatformActor,
    user_id: UserId,
    event: &AiModelRoleChanged,
) -> Result<(), RecordError> {
    let aggregate = Aggregate::new(AI_MODEL_ROLE_AGGREGATE, event.role.as_str())?;
    let mut tx = events.begin(actor, EVENT_PURPOSE).await?;
    outbox::publish_as(
        tx.transaction(),
        &aggregate,
        event,
        None,
        Actor::User { user_id },
    )
    .await?;
    tx.commit().await.map_err(TenantDbError::Database)?;
    Ok(())
}

#[cfg(test)]
mod tests;
