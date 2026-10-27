//! The admin API (Laysi/trustiot-v2#606; README.md, 「管理 API」 and 「驗證方式」): read the model
//! roles, replace a role's models, reset a role to its configured models. It is served on its own
//! port (`admin.listen_addr`, [`crate::start`]) and nowhere else, and that port serves nothing
//! else: the forwarding routes are on the API port only.
//!
//! Trust boundary. The gateway authenticates nobody here, as on its API port (decisions 27 and
//! 28: no internal identity check, no cookie routes, nothing that knows what a platform
//! administrator is). Who may reach this port is decided by the mesh, which admits the identity
//! service alone (the chart's Service and policies, Laysi/trustiot-v2#607); whether the person
//! behind a request is a platform administrator is decided by the identity service before it
//! calls (Laysi/trustiot-v2#608). The gateway itself checks only what it knows: that the role
//! exists, that the models fit the configuration and the role's protocol, and that the caller read
//! the role's current version. [`ai_gateway_api::ACTOR_HEADER`] names who asked; it is recorded
//! in the change log and the logs as the caller sent it and is never verified.
//!
//! Every change goes through [`crate::store::Store`]: a refused change writes nothing, and a
//! written one is in the change log in the same transaction. Errors are
//! [`ai_gateway_api::ErrorBody`] with `cache-control: no-store`; their messages never quote the
//! request.

use std::sync::Arc;

use ai_gateway_api::{self as api, ErrorCode};
use axum::Router;
use axum::body::Body;
use axum::extract::rejection::PathRejection;
use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, put};
use bytes::Bytes;
use http_body_util::{BodyExt, LengthLimitError, Limited};
use serde::Serialize;
use tokio::time::Instant;

use crate::config::ModelsError;
use crate::gateway::{Gateway, Snapshot};
use crate::store::{Changed, StoreError};

/// The largest request body read: a change lists at most 8 models of at most 290 characters.
pub const MAX_BODY_BYTES: usize = 16 << 10;

/// The admin routes on top of `gateway`, for the admin port only. Any other path answers 404
/// [`ErrorCode::NotFound`].
pub fn routes(gateway: Arc<Gateway>) -> Router {
    Router::new()
        .route(api::ROLES_PATH, get(list))
        .route(api::ROLE_PATH, put(set).delete(reset))
        .fallback(not_found)
        .with_state(gateway)
}

/// An error answer.
#[derive(Debug)]
struct Refusal {
    code: ErrorCode,
    message: String,
    current_version: Option<i64>,
}

impl Refusal {
    fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            current_version: None,
        }
    }

    fn role_not_found() -> Self {
        Self::new(ErrorCode::RoleNotFound, "no such model role")
    }

    fn unavailable() -> Self {
        Self::new(
            ErrorCode::Unavailable,
            "the model role settings could not be read or written; nothing was changed; try \
             again later",
        )
    }

    /// A refusal of the store. Every message is the gateway's own: the model errors name an
    /// index and the configuration's protocols, never a model the caller sent.
    fn from_store(error: &StoreError) -> Self {
        match error {
            StoreError::UnknownRole | StoreError::InvalidRole => Self::role_not_found(),
            StoreError::InvalidOperator => Self::invalid_actor(),
            StoreError::InvalidVersion => Self::new(
                ErrorCode::InvalidRequest,
                "the expected version must not be negative",
            ),
            StoreError::InvalidModels(models) => {
                let code = match models {
                    ModelsError::UnknownProvider { .. } => ErrorCode::UnknownProvider,
                    ModelsError::MixedProtocols | ModelsError::WrongProtocol { .. } => {
                        ErrorCode::ProtocolMismatch
                    }
                    ModelsError::Count
                    | ModelsError::Format { .. }
                    | ModelsError::ModelId { .. }
                    | ModelsError::Duplicate { .. } => ErrorCode::InvalidModels,
                };
                Self::new(code, models.to_string())
            }
            StoreError::Conflict { current, .. } => Self {
                code: ErrorCode::VersionConflict,
                message: "the model role was changed since it was read; read it again".to_owned(),
                current_version: Some(*current),
            },
            StoreError::Database(_) => Self::unavailable(),
        }
    }

    fn invalid_actor() -> Self {
        Self::new(
            ErrorCode::InvalidActor,
            format!(
                "the {} header is required once: 1 to {} printable ASCII characters",
                api::ACTOR_HEADER,
                api::MAX_ACTOR_CHARS
            ),
        )
    }
}

impl IntoResponse for Refusal {
    fn into_response(self) -> Response {
        let status =
            StatusCode::from_u16(self.code.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        json(
            status,
            &api::ErrorBody {
                error: api::ErrorDetail {
                    code: self.code,
                    message: self.message,
                    current_version: self.current_version,
                },
            },
        )
    }
}

/// A JSON answer that no cache keeps.
fn json<T: Serialize>(status: StatusCode, value: &T) -> Response {
    let (status, body) = match serde_json::to_vec(value) {
        Ok(body) => (status, body),
        // Plain structs of strings and numbers always serialize; never reached.
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            br#"{"error":{"code":"unknown","message":"the answer could not be written"}}"#.to_vec(),
        ),
    };
    let mut response = (status, body).into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

async fn not_found() -> Refusal {
    Refusal::new(ErrorCode::NotFound, "no such admin route")
}

/// The actor header: exactly one, [`api::is_valid_actor`].
fn actor(headers: &HeaderMap) -> Result<String, Refusal> {
    let mut values = headers.get_all(api::ACTOR_HEADER).iter();
    match (values.next(), values.next()) {
        (Some(value), None) => value
            .to_str()
            .ok()
            .filter(|text| api::is_valid_actor(text))
            .map(str::to_owned)
            .ok_or_else(Refusal::invalid_actor),
        _ => Err(Refusal::invalid_actor()),
    }
}

/// The role in the path, when the configuration has it. A role the configuration does not have
/// is not found, whatever the database holds for it.
fn configured_role(
    gateway: &Gateway,
    path: Result<Path<String>, PathRejection>,
) -> Result<String, Refusal> {
    match path {
        Ok(Path(role)) if gateway.rules().roles.contains_key(&role) => Ok(role),
        _ => Err(Refusal::role_not_found()),
    }
}

async fn read_body(body: Body) -> Result<Bytes, Refusal> {
    match Limited::new(body, MAX_BODY_BYTES).collect().await {
        Ok(collected) => Ok(collected.to_bytes()),
        Err(error) if error.downcast_ref::<LengthLimitError>().is_some() => Err(Refusal::new(
            ErrorCode::RequestTooLarge,
            format!("the request body is over {MAX_BODY_BYTES} bytes"),
        )),
        Err(_) => Err(Refusal::new(
            ErrorCode::InvalidRequest,
            "the request body could not be read",
        )),
    }
}

/// `expected_version=<n>` and nothing else.
fn expected_version(query: Option<&str>) -> Result<i64, Refusal> {
    let invalid = || {
        Refusal::new(
            ErrorCode::InvalidRequest,
            format!(
                "the query must be {}=<the version read>, a whole number from 0",
                api::EXPECTED_VERSION_PARAM
            ),
        )
    };
    let (name, value) = query
        .and_then(|query| query.split_once('='))
        .ok_or_else(invalid)?;
    if name != api::EXPECTED_VERSION_PARAM
        || value.is_empty()
        || !value.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(invalid());
    }
    value.parse::<i64>().map_err(|_| invalid())
}

/// The settings as the database has them now: the snapshot every forwarded request uses, loaded
/// again when the settings generation changed.
async fn snapshot(gateway: &Gateway) -> Result<Arc<Snapshot>, Refusal> {
    gateway
        .snapshot(Instant::now() + gateway.budget())
        .await
        .map_err(|error| {
            tracing::warn!(
                error = %error,
                "an admin request could not read the model role settings"
            );
            Refusal::unavailable()
        })
}

/// A configured role as `snapshot` has it.
fn role_view(gateway: &Gateway, snapshot: &Snapshot, role: &str) -> Option<api::Role> {
    let route = snapshot.route(role)?;
    let stored = snapshot.stored().iter().find(|row| row.role == role);
    Some(api::Role {
        role: role.to_owned(),
        protocol: route.protocol.name().to_owned(),
        models: route
            .targets
            .iter()
            .map(|target| target.label.clone())
            .collect(),
        configured_models: gateway
            .configured_models(role)
            .map(<[String]>::to_vec)
            .unwrap_or_default(),
        source: if route.stored_version.is_some() {
            api::Source::Database
        } else {
            api::Source::Configuration
        },
        version: stored.map_or(0, |row| row.version),
        ignored: snapshot.ignored().get(role).map(|ignored| api::Ignored {
            models: stored
                .and_then(|row| row.models.clone())
                .unwrap_or_default(),
            reason: ignored.reason.clone(),
        }),
        updated_at: stored.map(|row| row.updated_at.clone()),
        updated_by: stored.map(|row| row.updated_by.clone()),
    })
}

/// `GET /internal/admin/v1/roles`.
async fn list(
    State(gateway): State<Arc<Gateway>>,
    headers: HeaderMap,
) -> Result<Response, Refusal> {
    actor(&headers)?;
    let snapshot = snapshot(&gateway).await?;
    let roles = snapshot
        .routes()
        .filter_map(|(role, _)| role_view(&gateway, &snapshot, role))
        .collect();
    let providers = gateway
        .rules()
        .providers
        .iter()
        .map(|(name, protocol)| api::Provider {
            name: name.clone(),
            protocol: protocol.name().to_owned(),
        })
        .collect();
    Ok(json(StatusCode::OK, &api::Roles { roles, providers }))
}

/// `PUT /internal/admin/v1/roles/{role}`: [`api::SetModels`].
async fn set(
    State(gateway): State<Arc<Gateway>>,
    path: Result<Path<String>, PathRejection>,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, Refusal> {
    let actor = actor(&headers)?;
    let role = configured_role(&gateway, path)?;
    let body = read_body(body).await?;
    let request: api::SetModels = serde_json::from_slice(&body).map_err(|_| {
        Refusal::new(
            ErrorCode::InvalidRequest,
            "the body must be a JSON object with `models` (a list of strings) and \
             `expected_version` (a whole number) and nothing else",
        )
    })?;
    let store = gateway.store().ok_or_else(Refusal::unavailable)?;
    let changed = store
        .set_models(
            gateway.rules(),
            &role,
            &request.models,
            request.expected_version,
            &actor,
        )
        .await
        .map_err(|error| refused(&role, &actor, &error))?;
    let written = log_written(&role, &actor, "set", &request.models, changed);
    let after = After {
        models: request.models,
        source: api::Source::Database,
        version: written.unwrap_or(request.expected_version),
        actor: written.map(|_| actor),
    };
    Ok(answer_change(&gateway, &role, after).await)
}

/// `DELETE /internal/admin/v1/roles/{role}?expected_version=<n>`: back to the configured models.
async fn reset(
    State(gateway): State<Arc<Gateway>>,
    path: Result<Path<String>, PathRejection>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Result<Response, Refusal> {
    let actor = actor(&headers)?;
    let role = configured_role(&gateway, path)?;
    let expected = expected_version(query.as_deref())?;
    let store = gateway.store().ok_or_else(Refusal::unavailable)?;
    let changed = store
        .reset(&role, expected, &actor)
        .await
        .map_err(|error| refused(&role, &actor, &error))?;
    let configured = gateway
        .configured_models(&role)
        .map(<[String]>::to_vec)
        .unwrap_or_default();
    let written = log_written(&role, &actor, "reset", &configured, changed);
    let after = After {
        models: configured,
        source: api::Source::Configuration,
        version: written.unwrap_or(expected),
        actor: written.map(|_| actor),
    };
    Ok(answer_change(&gateway, &role, after).await)
}

/// Logs a change the store refused, and answers it.
fn refused(role: &str, actor: &str, error: &StoreError) -> Refusal {
    let refusal = Refusal::from_store(error);
    match error {
        StoreError::Database(error) => tracing::warn!(
            role,
            actor,
            error = %error,
            "a model role change failed in the database; nothing was changed"
        ),
        _ => tracing::info!(
            role,
            actor,
            code = refusal.code.as_str(),
            "refused a model role change"
        ),
    }
    refusal
}

/// Logs a written change; its version, or `None` when nothing was written.
fn log_written(
    role: &str,
    actor: &str,
    action: &str,
    models: &[String],
    changed: Changed,
) -> Option<i64> {
    match changed {
        Changed::Written { version } => {
            tracing::info!(
                role,
                actor,
                action,
                version,
                models = %models.join(","),
                "model role changed"
            );
            Some(version)
        }
        Changed::Unchanged => None,
    }
}

/// What a change left the role with, as far as the change itself knows.
struct After {
    /// The models in effect.
    models: Vec<String>,
    source: api::Source,
    /// The role's version.
    version: i64,
    /// Who wrote it; `None` when nothing was written (a reset of a role without stored models).
    actor: Option<String>,
}

/// The role after a change: read again, so this replica uses it at once (a later change by
/// someone else may show too). The change is already committed, so a failed read does not make
/// it an error: the answer then says only what the change wrote (without `updated_at`).
async fn answer_change(gateway: &Gateway, role: &str, after: After) -> Response {
    let reread = match snapshot(gateway).await {
        Ok(snapshot) => role_view(gateway, &snapshot, role),
        Err(_) => None,
    };
    let view = reread.unwrap_or_else(|| api::Role {
        role: role.to_owned(),
        protocol: gateway
            .rules()
            .roles
            .get(role)
            .map(|protocol| protocol.name().to_owned())
            .unwrap_or_default(),
        models: after.models,
        configured_models: gateway
            .configured_models(role)
            .map(<[String]>::to_vec)
            .unwrap_or_default(),
        source: after.source,
        version: after.version,
        ignored: None,
        updated_at: None,
        updated_by: after.actor,
    });
    json(StatusCode::OK, &view)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_reset_version_is_the_one_parameter_of_the_query() {
        assert_eq!(expected_version(Some("expected_version=0")).ok(), Some(0));
        assert_eq!(expected_version(Some("expected_version=42")).ok(), Some(42));
        for invalid in [
            None,
            Some(""),
            Some("expected_version="),
            Some("expected_version=-1"),
            Some("expected_version=+1"),
            Some("expected_version=1&other=2"),
            Some("version=1"),
            Some("expected_version=99999999999999999999"),
        ] {
            let refusal = expected_version(invalid).unwrap_err();
            assert_eq!(refusal.code, ErrorCode::InvalidRequest, "{invalid:?}");
        }
    }

    #[test]
    fn model_errors_map_to_their_codes() {
        for (error, code) in [
            (ModelsError::Count, ErrorCode::InvalidModels),
            (ModelsError::Format { index: 0 }, ErrorCode::InvalidModels),
            (ModelsError::ModelId { index: 0 }, ErrorCode::InvalidModels),
            (
                ModelsError::Duplicate { index: 1 },
                ErrorCode::InvalidModels,
            ),
            (
                ModelsError::UnknownProvider { index: 0 },
                ErrorCode::UnknownProvider,
            ),
            (ModelsError::MixedProtocols, ErrorCode::ProtocolMismatch),
            (
                ModelsError::WrongProtocol {
                    expected: "anthropic-messages",
                    found: "openai-decisions",
                },
                ErrorCode::ProtocolMismatch,
            ),
        ] {
            assert_eq!(
                Refusal::from_store(&StoreError::InvalidModels(error)).code,
                code
            );
        }
        let conflict = Refusal::from_store(&StoreError::Conflict {
            expected: 1,
            current: 3,
        });
        assert_eq!(conflict.code, ErrorCode::VersionConflict);
        assert_eq!(conflict.current_version, Some(3));
    }

    #[test]
    fn the_actor_header_is_required_once_and_printable() {
        let mut headers = HeaderMap::new();
        assert!(actor(&headers).is_err());
        headers.insert(api::ACTOR_HEADER, HeaderValue::from_static("user:1"));
        assert_eq!(actor(&headers).ok().as_deref(), Some("user:1"));
        headers.append(api::ACTOR_HEADER, HeaderValue::from_static("user:2"));
        assert!(actor(&headers).is_err());
        headers.insert(
            api::ACTOR_HEADER,
            HeaderValue::from_bytes(b"caf\xc3\xa9").unwrap(),
        );
        assert!(actor(&headers).is_err());
        headers.insert(
            api::ACTOR_HEADER,
            HeaderValue::from_str(&"a".repeat(api::MAX_ACTOR_CHARS + 1)).unwrap(),
        );
        assert!(actor(&headers).is_err());
    }
}
