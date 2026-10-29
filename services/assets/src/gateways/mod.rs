//! The device pool and gateway binding (Laysi/trustiot-v2#101, rebuild checklist D3;
//! services/assets/README.md, "閘道器設備池與綁定"): every gateway registered at the factory
//! enters the pool ([`pool`]), and a platform administrator binds it to a tenant ([`binding`]).
//! There is no screen yet; tenant administrators' own binding is Laysi/trustiot-v2#114, and
//! unbinding and rebinding are Laysi/trustiot-v2#115.
//!
//! | Request | Success |
//! | --- | --- |
//! | `GET /api/assets/v1/platform/gateways` | 200 [`PoolGatewayList`] |
//! | `GET /api/assets/v1/platform/gateways/{hardware_id}` | 200 [`PoolGateway`] |
//! | `PUT /api/assets/v1/platform/gateways/{hardware_id}/binding`, a gateway of the pool bound to no tenant | 201 [`GatewayBinding`], `assets.gateway_bound` |
//! | The same, the gateway bound to that tenant already | 200 [`GatewayBinding`], the existing binding; nothing is written |
//!
//! Errors follow M1, checked in this order:
//!
//! | Request | Response |
//! | --- | --- |
//! | No valid internal identity token | 401 `unauthenticated` |
//! | Not a platform administrator, or a token another service exchanged (`act`) | 403 `permission_denied`, before anything is read |
//! | A `hardware_id` that breaks the hardware ID rule | 404 `not_found` |
//! | A listing parameter that is unknown or malformed | 400 `invalid_request` |
//! | A binding whose `Content-Type` is not `application/json`, a body over [`MAX_BODY_BYTES`] or not a JSON object | 400 `invalid_request` (keys `gateway_binding.body_too_large`, `gateway_binding.body_invalid`) |
//! | An unknown field, or `tenant_id` missing or not a UUID | 422 `validation_failed`, one field error per problem |
//! | A `tenant_id` the service does not know | 422, field `/tenant_id`, key `gateway_binding.tenant_unknown` |
//! | A gateway the pool does not have | 404 `not_found` |
//! | A gateway bound to another tenant | 409 `conflict`, key `gateway_binding.bound_to_another_tenant` |
//! | The database failed | 503 `unavailable`; nothing was written |
//!
//! Every response carries `cache-control: no-store`. Each binding logs one INFO line with the
//! administrator's user ID, the tenant, the hardware ID, the binding ID and whether it was new;
//! never the token.

pub mod binding;
pub mod pool;

use std::sync::Arc;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::rejection::{PathRejection, QueryRejection};
use axum::extract::{FromRef, Path, Query, State};
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, put};
use chrono::{DateTime, Utc};
use contract::{ErrorBody, ErrorCode, FieldError, GatewayBindingId, MessageKey, TenantId};
use protocol::GatewayId;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use service_kit::identity::{IdentityVerifier, Requirement, UserIdentity};
use service_kit::tenant::{PlatformActor, PlatformDb};
use utoipa::ToSchema;

use self::binding::{BindError, Binding, GatewayBinder, Outcome};
use self::pool::{CurrentBinding, Filter, PoolEntry, ReadError};

/// The pool, as the ingress gateway forwards it (no prefix is stripped).
pub const POOL_PATH: &str = "/api/assets/v1/platform/gateways";
/// One gateway of the pool.
pub const POOL_GATEWAY_PATH: &str = "/api/assets/v1/platform/gateways/{hardware_id}";
/// A gateway's binding to a tenant.
pub const BINDING_PATH: &str = "/api/assets/v1/platform/gateways/{hardware_id}/binding";

/// The largest body a binding accepts; the body is one tenant ID.
pub const MAX_BODY_BYTES: usize = 4 * 1024;

/// Gateways per page of the pool when the request does not say.
pub const DEFAULT_PAGE: u32 = 100;
/// Most gateways per page.
pub const MAX_PAGE: u32 = 500;

/// Most field errors one response lists; `details.omitted_field_errors` counts the rest.
const MAX_FIELD_ERRORS: usize = 10;
/// Longest unknown field name a field error repeats; longer or odd names point at the body.
const MAX_ECHOED_FIELD_LEN: usize = 64;

/// The body is larger than [`MAX_BODY_BYTES`] (400); `details.max_bytes`.
pub const BODY_TOO_LARGE: MessageKey = MessageKey::new("gateway_binding.body_too_large");
/// The body is not a JSON object (400).
pub const BODY_INVALID: MessageKey = MessageKey::new("gateway_binding.body_invalid");
/// The body has a field the request does not take (422).
pub const UNKNOWN_FIELD: MessageKey = MessageKey::new("gateway_binding.unknown_field");
/// `tenant_id` is missing or `null` (422).
pub const FIELD_REQUIRED: MessageKey = MessageKey::new("gateway_binding.field_required");
/// `tenant_id` is not a UUID string (422).
pub const TENANT_ID_INVALID: MessageKey = MessageKey::new("gateway_binding.tenant_id_invalid");
/// The assets service does not know the tenant (422): no such tenant, or one created a moment
/// ago whose creation has not reached the service yet.
pub const TENANT_UNKNOWN: MessageKey = MessageKey::new("gateway_binding.tenant_unknown");
/// Another tenant holds the gateway (409).
pub const BOUND_TO_ANOTHER_TENANT: MessageKey =
    MessageKey::new("gateway_binding.bound_to_another_tenant");

/// A gateway's binding to a tenant.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct GatewayBinding {
    /// The binding: a new one every time the gateway is bound.
    pub binding_id: GatewayBindingId,
    /// The gateway's hardware ID.
    #[schema(value_type = String, pattern = "^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$", max_length = 64, example = "tg-0001")]
    pub hardware_id: GatewayId,
    /// The tenant the gateway is bound to.
    pub tenant_id: TenantId,
    /// When it was bound, by the database clock.
    pub bound_at: DateTime<Utc>,
}

impl From<Binding> for GatewayBinding {
    fn from(binding: Binding) -> Self {
        Self {
            binding_id: binding.binding_id,
            hardware_id: binding.hardware_id,
            tenant_id: binding.tenant_id,
            bound_at: binding.bound_at,
        }
    }
}

/// A gateway of the device pool.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct PoolGateway {
    /// The hardware ID: the common name of its certificates and its MQTT identity.
    #[schema(value_type = String, pattern = "^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$", max_length = 64, example = "tg-0001")]
    pub hardware_id: GatewayId,
    /// When the certs service registered it at the factory.
    pub registered_at: DateTime<Utc>,
    /// The production workstation that registered it.
    pub workstation: String,
    /// When the assets service put it in the pool.
    pub pooled_at: DateTime<Utc>,
    /// Its current binding; `null` while it is bound to no tenant.
    pub binding: Option<GatewayBinding>,
}

impl From<PoolEntry> for PoolGateway {
    fn from(entry: PoolEntry) -> Self {
        let binding = entry.binding.map(
            |CurrentBinding {
                 binding_id,
                 tenant_id,
                 bound_at,
             }| GatewayBinding {
                binding_id,
                hardware_id: entry.hardware_id.clone(),
                tenant_id,
                bound_at,
            },
        );
        Self {
            hardware_id: entry.hardware_id,
            registered_at: entry.registered_at,
            workstation: entry.workstation,
            pooled_at: entry.pooled_at,
            binding,
        }
    }
}

/// A page of the device pool, by hardware ID (byte order).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct PoolGatewayList {
    /// The gateways.
    pub gateways: Vec<PoolGateway>,
    /// The `after` of the next page; `null` on the last one.
    pub next_after: Option<String>,
}

/// The body of a binding.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct BindGateway {
    /// The tenant to bind the gateway to.
    pub tenant_id: TenantId,
}

/// The listing's query parameters as they arrive; any other parameter is refused.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListParams {
    /// `state`: `bound` or `unbound`; absent for every gateway.
    pub state: Option<String>,
    /// `after`: the previous page's `next_after`.
    pub after: Option<String>,
    /// `limit`: 1 to [`MAX_PAGE`].
    pub limit: Option<String>,
}

/// A checked listing request.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ListQuery {
    filter: Filter,
    after: Option<GatewayId>,
    limit: u32,
}

impl ListQuery {
    /// `None` when a parameter breaks its rule.
    fn parse(params: &ListParams) -> Option<Self> {
        let filter = match params.state.as_deref() {
            None => Filter::All,
            Some("bound") => Filter::Bound,
            Some("unbound") => Filter::Unbound,
            Some(_) => return None,
        };
        let after = match params.after.as_deref() {
            None => None,
            Some(text) => Some(GatewayId::new(text).ok()?),
        };
        let limit = match params.limit.as_deref() {
            None => DEFAULT_PAGE,
            Some(text) => text
                .parse::<u32>()
                .ok()
                .filter(|limit| (1..=MAX_PAGE).contains(limit))?,
        };
        Some(Self {
            filter,
            after,
            limit,
        })
    }
}

/// The state of the routes.
#[derive(Clone)]
pub struct Routes {
    verifier: IdentityVerifier,
    platform: PlatformDb,
    binder: Arc<GatewayBinder>,
}

impl FromRef<Routes> for IdentityVerifier {
    fn from_ref(state: &Routes) -> Self {
        state.verifier.clone()
    }
}

/// The device pool and binding routes. `platform` is the service's platform role, which reads
/// the pool and every tenant's bindings for platform administrators.
pub fn routes(verifier: IdentityVerifier, platform: PlatformDb, binder: GatewayBinder) -> Router {
    Router::new()
        .route(POOL_PATH, get(list_pool))
        .route(POOL_GATEWAY_PATH, get(get_pool_gateway))
        .route(BINDING_PATH, put(bind_gateway))
        .route_layer(axum::middleware::map_response(no_store))
        .with_state(Routes {
            verifier,
            platform,
            binder: Arc::new(binder),
        })
}

/// Marks every response of these routes as not to be stored, including the 401 that the
/// [`UserIdentity`] extractor answers.
async fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// The device pool with each gateway's current binding. Platform administrators only.
#[utoipa::path(
    get,
    path = "/api/assets/v1/platform/gateways",
    tag = "assets",
    operation_id = "listPoolGateways",
    params(
        ("state" = Option<String>, Query, description = "`unbound` for the gateways bound to no tenant (the ones to bind), `bound` for the bound ones; absent for every gateway."),
        ("after" = Option<String>, Query, description = "The previous page's `next_after`, for the next page."),
        ("limit" = Option<u32>, Query, minimum = 1, maximum = 500, description = "Gateways per page, 1 to 500; default 100."),
    ),
    responses(
        (status = 200, description = "A page of the pool, by hardware ID.", body = PoolGatewayList),
        (status = 400, description = "An unknown or malformed parameter (`invalid_request`).", body = ErrorBody),
        (status = 401, description = "No valid session (`unauthenticated`).", body = ErrorBody),
        (status = 403, description = "Not a platform administrator, or not the administrator's own request (`permission_denied`).", body = ErrorBody),
        (status = 503, description = "The database failed (`unavailable`).", body = ErrorBody),
    )
)]
async fn list_pool(
    State(state): State<Routes>,
    user: UserIdentity,
    params: Result<Query<ListParams>, QueryRejection>,
) -> Response {
    let Some(admin) = refuse(&user, "list the device pool") else {
        return error(ErrorBody::new(ErrorCode::PermissionDenied));
    };
    let Some(query) = params
        .ok()
        .and_then(|Query(params)| ListQuery::parse(&params))
    else {
        return error(ErrorBody::new(ErrorCode::InvalidRequest));
    };
    // One more than the page, to know whether there is a next one.
    let read = pool::list(
        &state.platform,
        admin,
        query.filter,
        query.after.as_ref(),
        query.limit + 1,
    )
    .await;
    match read {
        Ok(mut entries) => {
            let page = usize::try_from(query.limit).unwrap_or(usize::MAX);
            let next_after = (entries.len() > page).then(|| {
                entries.truncate(page);
                entries
                    .last()
                    .map(|entry| entry.hardware_id.as_str().to_owned())
            });
            ok(
                StatusCode::OK,
                &PoolGatewayList {
                    gateways: entries.into_iter().map(PoolGateway::from).collect(),
                    next_after: next_after.flatten(),
                },
            )
        }
        Err(failure) => read_error(&user, &failure, "could not list the device pool"),
    }
}

/// One gateway of the device pool with its current binding. Platform administrators only.
#[utoipa::path(
    get,
    path = "/api/assets/v1/platform/gateways/{hardware_id}",
    tag = "assets",
    operation_id = "getPoolGateway",
    params(("hardware_id" = String, Path, description = "The gateway's hardware ID.")),
    responses(
        (status = 200, description = "The gateway.", body = PoolGateway),
        (status = 401, description = "No valid session (`unauthenticated`).", body = ErrorBody),
        (status = 403, description = "Not a platform administrator, or not the administrator's own request (`permission_denied`).", body = ErrorBody),
        (status = 404, description = "The pool has no such gateway (`not_found`).", body = ErrorBody),
        (status = 503, description = "The database failed (`unavailable`).", body = ErrorBody),
    )
)]
async fn get_pool_gateway(
    State(state): State<Routes>,
    user: UserIdentity,
    hardware_id: Result<Path<String>, PathRejection>,
) -> Response {
    let Some(admin) = refuse(&user, "read a gateway of the device pool") else {
        return error(ErrorBody::new(ErrorCode::PermissionDenied));
    };
    let Some(hardware_id) = parse_hardware_id(hardware_id) else {
        return error(ErrorBody::new(ErrorCode::NotFound));
    };
    match pool::get(&state.platform, admin, &hardware_id).await {
        Ok(Some(entry)) => ok(StatusCode::OK, &PoolGateway::from(entry)),
        Ok(None) => error(ErrorBody::new(ErrorCode::NotFound)),
        Err(failure) => read_error(
            &user,
            &failure,
            "could not read a gateway of the device pool",
        ),
    }
}

/// Binds a gateway of the device pool to a tenant; idempotent. Platform administrators only.
#[utoipa::path(
    put,
    path = "/api/assets/v1/platform/gateways/{hardware_id}/binding",
    tag = "assets",
    operation_id = "bindGateway",
    params(("hardware_id" = String, Path, description = "The gateway's hardware ID.")),
    request_body = BindGateway,
    responses(
        (status = 200, description = "The gateway was bound to this tenant already: the existing binding. Nothing was written, so retrying after a lost response is safe.", body = GatewayBinding),
        (status = 201, description = "The gateway is now bound to the tenant; `assets.gateway_bound` was written in the same transaction.", body = GatewayBinding),
        (status = 400, description = "The `Content-Type` is not `application/json`, the body is larger than 4 KiB (`gateway_binding.body_too_large`), or it is not a JSON object (`gateway_binding.body_invalid`) (`invalid_request`).", body = ErrorBody),
        (status = 401, description = "No valid session (`unauthenticated`).", body = ErrorBody),
        (status = 403, description = "Not a platform administrator, or not the administrator's own request (`permission_denied`).", body = ErrorBody),
        (status = 404, description = "The device pool has no such gateway (`not_found`).", body = ErrorBody),
        (status = 409, description = "Another tenant holds the gateway (`conflict`, `gateway_binding.bound_to_another_tenant`). Nothing was written.", body = ErrorBody),
        (status = 422, description = "An unknown field, `tenant_id` missing or not a UUID, or a tenant the assets service does not know (`gateway_binding.tenant_unknown`) (`validation_failed`). Nothing was written.", body = ErrorBody),
        (status = 503, description = "The database failed (`unavailable`). Nothing was written; retrying is safe.", body = ErrorBody),
    )
)]
async fn bind_gateway(
    State(state): State<Routes>,
    user: UserIdentity,
    hardware_id: Result<Path<String>, PathRejection>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    if refuse(&user, "bind a gateway").is_none() {
        return error(ErrorBody::new(ErrorCode::PermissionDenied));
    }
    let Some(hardware_id) = parse_hardware_id(hardware_id) else {
        return error(ErrorBody::new(ErrorCode::NotFound));
    };
    let body = match read_json(&headers, body).await {
        Ok(body) => body,
        Err(refused) => return error(refused),
    };
    let tenant = match parse_bind(&body) {
        Ok(tenant) => tenant,
        Err(refused) => return error(refused),
    };
    match state.binder.bind(&user, &hardware_id, tenant).await {
        Ok(outcome) => {
            let (status, binding) = match outcome {
                Outcome::Bound(binding) => (StatusCode::CREATED, binding),
                Outcome::AlreadyBound(binding) => (StatusCode::OK, binding),
            };
            tracing::info!(
                user_id = %user.user_id(),
                tenant_id = %binding.tenant_id,
                hardware_id = binding.hardware_id.as_str(),
                binding_id = %binding.binding_id,
                created = status == StatusCode::CREATED,
                "bound a gateway"
            );
            ok(status, &GatewayBinding::from(binding))
        }
        Err(failure) => bind_error(&user, &hardware_id, tenant, &failure),
    }
}

/// The platform actor of a platform administrator's own request; `None` (answered 403) for
/// anyone else, logged at INFO without the token by service-kit's shared check.
fn refuse(user: &UserIdentity, action: &'static str) -> Option<PlatformActor> {
    user.require(Requirement::OwnPlatformAdmin, action).ok()?;
    PlatformActor::admin(user)
}

/// The path's hardware ID; `None` (answered 404) when it breaks the hardware ID rule.
fn parse_hardware_id(path: Result<Path<String>, PathRejection>) -> Option<GatewayId> {
    GatewayId::new(path.ok()?.0).ok()
}

/// Whether the request has exactly one `Content-Type`, `application/json` with or without
/// parameters, compared without regard to ASCII case. A page of another origin cannot send that
/// type with the administrator's cookie without a CORS preflight, which nothing answers.
fn is_json(headers: &HeaderMap) -> bool {
    let mut values = headers.get_all(CONTENT_TYPE).iter();
    let (Some(value), None) = (values.next(), values.next()) else {
        return false;
    };
    value.to_str().is_ok_and(|value| {
        let essence = value.split(';').next().unwrap_or_default();
        essence.trim().eq_ignore_ascii_case("application/json")
    })
}

/// The body of a binding, at most [`MAX_BODY_BYTES`], after the content type check.
async fn read_json(headers: &HeaderMap, body: Body) -> Result<Bytes, ErrorBody> {
    if !is_json(headers) {
        return Err(ErrorBody::new(ErrorCode::InvalidRequest));
    }
    axum::body::to_bytes(body, MAX_BODY_BYTES)
        .await
        .map_err(|_| {
            ErrorBody::new(ErrorCode::InvalidRequest)
                .with_message_key(BODY_TOO_LARGE)
                .with_detail("max_bytes", MAX_BODY_BYTES)
        })
}

/// The tenant of a binding's body: a JSON object with exactly `tenant_id`, a UUID string.
fn parse_bind(body: &[u8]) -> Result<TenantId, ErrorBody> {
    let Ok(Value::Object(map)) = serde_json::from_slice::<Value>(body) else {
        return Err(ErrorBody::new(ErrorCode::InvalidRequest).with_message_key(BODY_INVALID));
    };
    let mut errors = unknown_fields(&map);
    let tenant = match map.get("tenant_id") {
        None | Some(Value::Null) => {
            errors.push(FieldError::new("/tenant_id", FIELD_REQUIRED));
            None
        }
        Some(Value::String(text)) => {
            let parsed = text.parse::<TenantId>().ok();
            if parsed.is_none() {
                errors.push(FieldError::new("/tenant_id", TENANT_ID_INVALID));
            }
            parsed
        }
        Some(_) => {
            errors.push(FieldError::new("/tenant_id", TENANT_ID_INVALID));
            None
        }
    };
    match tenant {
        Some(tenant) if errors.is_empty() => Ok(tenant),
        _ => Err(validation_failed(errors)),
    }
}

/// A field error for every field but `tenant_id`.
fn unknown_fields(map: &Map<String, Value>) -> Vec<FieldError> {
    map.keys()
        .filter(|field| field.as_str() != "tenant_id")
        .map(|field| {
            let echo = field.len() <= MAX_ECHOED_FIELD_LEN
                && field.bytes().all(|byte| byte.is_ascii_graphic());
            let pointer = if echo {
                format!("/{}", field.replace('~', "~0").replace('/', "~1"))
            } else {
                String::new()
            };
            FieldError::new(pointer, UNKNOWN_FIELD)
        })
        .collect()
}

/// 422 with at most [`MAX_FIELD_ERRORS`] field errors; `details.omitted_field_errors` counts the
/// rest.
fn validation_failed(errors: Vec<FieldError>) -> ErrorBody {
    let omitted = errors.len().saturating_sub(MAX_FIELD_ERRORS);
    let mut body = ErrorBody::new(ErrorCode::ValidationFailed);
    for field_error in errors.into_iter().take(MAX_FIELD_ERRORS) {
        body = body.with_field_error(field_error);
    }
    if omitted > 0 {
        body = body.with_detail("omitted_field_errors", omitted);
    }
    body
}

/// The response for a failed read; logged at ERROR with its kind only.
fn read_error(user: &UserIdentity, failure: &ReadError, message: &'static str) -> Response {
    tracing::error!(
        user_id = %user.user_id(),
        error_kind = failure.log_kind(),
        "{message}"
    );
    error(ErrorBody::new(ErrorCode::Unavailable))
}

/// The response for a failed binding; failures that are not the request's fault are logged at
/// ERROR with their kind only, refusals at INFO.
fn bind_error(
    user: &UserIdentity,
    hardware_id: &GatewayId,
    tenant: TenantId,
    failure: &BindError,
) -> Response {
    let body = match failure {
        BindError::NotPlatformAdmin => ErrorBody::new(ErrorCode::PermissionDenied),
        BindError::TenantUnknown => ErrorBody::new(ErrorCode::ValidationFailed)
            .with_field_error(FieldError::new("/tenant_id", TENANT_UNKNOWN)),
        BindError::NotInPool => ErrorBody::new(ErrorCode::NotFound),
        BindError::BoundElsewhere => {
            ErrorBody::new(ErrorCode::Conflict).with_message_key(BOUND_TO_ANOTHER_TENANT)
        }
        BindError::Corrupt
        | BindError::Database(_)
        | BindError::Publish(_)
        | BindError::Aggregate(_) => {
            tracing::error!(
                user_id = %user.user_id(),
                tenant_id = %tenant,
                hardware_id = hardware_id.as_str(),
                error_kind = failure.log_kind(),
                "could not bind a gateway"
            );
            return error(ErrorBody::new(ErrorCode::Unavailable));
        }
    };
    tracing::info!(
        user_id = %user.user_id(),
        tenant_id = %tenant,
        hardware_id = hardware_id.as_str(),
        reason = failure.log_kind(),
        "refused to bind a gateway"
    );
    error(body)
}

fn ok<T: Serialize>(status: StatusCode, body: &T) -> Response {
    (status, [(CACHE_CONTROL, "no-store")], axum::Json(body)).into_response()
}

fn error(body: ErrorBody) -> Response {
    (
        body.http_status(),
        [(CACHE_CONTROL, "no-store")],
        axum::Json(body),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn field_errors(body: &ErrorBody) -> Vec<(String, String)> {
        let value = serde_json::to_value(body).unwrap();
        value["field_errors"]
            .as_array()
            .map(|errors| {
                errors
                    .iter()
                    .map(|error| {
                        (
                            error["field"].as_str().unwrap().to_owned(),
                            error["message_key"].as_str().unwrap().to_owned(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn parse(value: &Value) -> Result<TenantId, ErrorBody> {
        parse_bind(&serde_json::to_vec(value).unwrap())
    }

    #[test]
    fn a_binding_names_exactly_one_tenant() {
        let tenant = TenantId::generate();
        assert_eq!(
            parse(&json!({"tenant_id": tenant.to_string()})).unwrap(),
            tenant
        );

        let invalid = TENANT_ID_INVALID.as_str().to_owned();
        let required = FIELD_REQUIRED.as_str().to_owned();
        let unknown = UNKNOWN_FIELD.as_str().to_owned();
        for (body, expected) in [
            (json!({}), vec![("/tenant_id".to_owned(), required.clone())]),
            (
                json!({"tenant_id": null}),
                vec![("/tenant_id".to_owned(), required)],
            ),
            (
                json!({"tenant_id": "acme"}),
                vec![("/tenant_id".to_owned(), invalid.clone())],
            ),
            (
                json!({"tenant_id": 7}),
                vec![("/tenant_id".to_owned(), invalid)],
            ),
            (
                json!({"tenant_id": tenant.to_string(), "hardware_id": "tg-1", "a/b~c": 1}),
                vec![
                    ("/a~1b~0c".to_owned(), unknown.clone()),
                    ("/hardware_id".to_owned(), unknown.clone()),
                ],
            ),
            (
                json!({"tenant_id": tenant.to_string(), "x".repeat(65): 1}),
                vec![(String::new(), unknown)],
            ),
        ] {
            let refused = parse(&body).unwrap_err();
            assert_eq!(
                refused.http_status(),
                StatusCode::UNPROCESSABLE_ENTITY,
                "{body}"
            );
            assert_eq!(field_errors(&refused), expected, "{body}");
        }
        for body in [&b"[]"[..], b"\"tenant\"", b"{", b""] {
            let refused = parse_bind(body).unwrap_err();
            assert_eq!(refused.http_status(), StatusCode::BAD_REQUEST);
            assert_eq!(
                serde_json::to_value(&refused).unwrap()["message_key"],
                BODY_INVALID.as_str()
            );
        }
    }

    #[test]
    fn many_unknown_fields_are_counted_not_listed() {
        let mut body = Map::new();
        body.insert(
            "tenant_id".to_owned(),
            json!(TenantId::generate().to_string()),
        );
        for index in 0..25 {
            body.insert(format!("f{index}"), json!(index));
        }
        let refused = parse(&Value::Object(body)).unwrap_err();
        assert_eq!(field_errors(&refused).len(), MAX_FIELD_ERRORS);
        assert_eq!(
            serde_json::to_value(&refused).unwrap()["details"]["omitted_field_errors"],
            15
        );
    }

    #[test]
    fn listing_parameters_are_checked() {
        let params = |state: Option<&str>, after: Option<&str>, limit: Option<&str>| ListParams {
            state: state.map(str::to_owned),
            after: after.map(str::to_owned),
            limit: limit.map(str::to_owned),
        };
        assert_eq!(
            ListQuery::parse(&params(None, None, None)),
            Some(ListQuery {
                filter: Filter::All,
                after: None,
                limit: DEFAULT_PAGE
            })
        );
        assert_eq!(
            ListQuery::parse(&params(Some("unbound"), Some("tg-0001"), Some("500"))),
            Some(ListQuery {
                filter: Filter::Unbound,
                after: Some(GatewayId::new("tg-0001").unwrap()),
                limit: MAX_PAGE
            })
        );
        assert_eq!(
            ListQuery::parse(&params(Some("bound"), None, Some("1")))
                .unwrap()
                .filter,
            Filter::Bound
        );
        for refused in [
            params(Some("all"), None, None),
            params(None, Some("../x"), None),
            params(None, Some(""), None),
            params(None, None, Some("0")),
            params(None, None, Some("501")),
            params(None, None, Some("ten")),
        ] {
            assert_eq!(ListQuery::parse(&refused), None, "{refused:?}");
        }
    }

    #[test]
    fn path_hardware_ids_follow_the_rule() {
        assert!(parse_hardware_id(Ok(Path("tg-0001".to_owned()))).is_some());
        assert!(parse_hardware_id(Ok(Path("..".to_owned()))).is_none());
        assert!(parse_hardware_id(Ok(Path("a/b".to_owned()))).is_none());
        assert!(parse_hardware_id(Ok(Path(String::new()))).is_none());
        assert!(parse_hardware_id(Ok(Path("x".repeat(65)))).is_none());
    }
}
