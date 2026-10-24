//! Devices (Laysi/trustiot-v2#100, rebuild checklist C2; services/assets/README.md, "設備"): the
//! Modbus endpoints a tenant's gateways poll, each read with one of the tenant's device models.
//!
//! | Request | Role | Success |
//! | --- | --- | --- |
//! | `GET /api/assets/v1/devices` | viewer | 200 [`DeviceList`] |
//! | `POST /api/assets/v1/devices` | editor | 201 [`Device`], `assets.device_created` |
//! | `GET /api/assets/v1/devices/{device_id}` | viewer | 200 [`Device`] |
//! | `PUT /api/assets/v1/devices/{device_id}` | editor | 200 [`Device`], `assets.device_updated` |
//! | `DELETE /api/assets/v1/devices/{device_id}` | editor | 204, `assets.device_deleted` |
//!
//! Both write entries parse their body with [`document`] and pass every field rule before the
//! store ([`store`]) checks the gateway, the model and the gateway's other devices and writes the
//! change with its event in one transaction of the caller's tenant. Errors follow M1:
//!
//! | Request | Response |
//! | --- | --- |
//! | No valid internal identity token | 401 `unauthenticated` |
//! | No tenant, or a role below the one the request needs | 403 `permission_denied` |
//! | A `device_id` that is not a UUID, or no device of the caller's tenant | 404 `not_found` |
//! | A write whose `Content-Type` is not `application/json` (parameters allowed) | 400 `invalid_request` |
//! | A body over [`MAX_BODY_BYTES`] | 400 `invalid_request`, key `device.body_too_large`, `{"max_bytes"}` |
//! | A body that is not a JSON object | 400 `invalid_request`, key `device.body_invalid` |
//! | A field rule broken | 422 `validation_failed`, one field error per problem (JSON Pointer and key) |
//! | A gateway not bound to the caller's tenant | 422, field `/hardware_id`, key `device.gateway_not_bound` |
//! | A model the caller's tenant does not have | 422, field `/model_id`, key `device.model_not_found` |
//! | An update with another name, gateway or model | 422, one field error each, keys `device.*_immutable` |
//! | A name the tenant already has | 409 `conflict`, key `device.name_taken` |
//! | Another device of the gateway at the endpoint with the other transport | 409 `conflict`, key `device.transport_mismatch`, `{"device_id", "transport"}` |
//! | Another device of the gateway at the endpoint with the unit ID | 409 `conflict`, key `device.unit_taken`, `{"device_id"}` |
//! | An update whose `revision` is not the current one | 409 `conflict`, key `device.revision_mismatch`, `{"current"}` |
//! | An update of a device whose gateway binding has ended | 409 `conflict`, key `device.binding_ended`; the device can still be deleted |
//! | The database failed | 503 `unavailable`; nothing was written |
//!
//! Every response carries `cache-control: no-store`. A write that changed something logs one
//! INFO line with the user, tenant, device and revision, never names, hosts or ports.

pub mod document;
pub mod store;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::rejection::PathRejection;
use axum::extract::{FromRef, Path, State};
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE, LOCATION};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use chrono::{DateTime, Utc};
use contract::{DeviceId, DeviceModelId, ErrorBody, ErrorCode, FieldError, MessageKey, TenantRole};
use serde::Serialize;
use service_kit::identity::{IdentityVerifier, UserIdentity};
use service_kit::tenant::TenantDb;
use utoipa::ToSchema;
use utoipa::openapi::schema::{Object, ObjectBuilder, Type};

use self::document::TRANSPORTS;
use self::store::{StoreError, StoredDevice};
use crate::device_models::document::wire_name;

/// The collection, as the ingress gateway forwards it (no prefix is stripped).
pub const DEVICES_PATH: &str = "/api/assets/v1/devices";
/// One device.
pub const DEVICE_PATH: &str = "/api/assets/v1/devices/{device_id}";

/// The largest body a write accepts: a device's fields with the longest host fit many times.
pub const MAX_BODY_BYTES: usize = 16 * 1024;

/// The body is larger than [`MAX_BODY_BYTES`] (400); `details.max_bytes`.
pub const BODY_TOO_LARGE: MessageKey = MessageKey::new("device.body_too_large");
/// The tenant has a device of the name (409).
pub const NAME_TAKEN: MessageKey = MessageKey::new("device.name_taken");
/// The gateway is not bound to the caller's tenant (422, field `/hardware_id`). Whether it is
/// bound to another tenant, not in the pool or unknown is not told apart.
pub const GATEWAY_NOT_BOUND: MessageKey = MessageKey::new("device.gateway_not_bound");
/// The caller's tenant has no model of the ID (422, field `/model_id`).
pub const MODEL_NOT_FOUND: MessageKey = MessageKey::new("device.model_not_found");
/// An update names the device differently: the name never changes (422, field `/name`).
pub const NAME_IMMUTABLE: MessageKey = MessageKey::new("device.name_immutable");
/// An update names another gateway: a device stays on its gateway (422, field `/hardware_id`).
pub const GATEWAY_IMMUTABLE: MessageKey = MessageKey::new("device.gateway_immutable");
/// An update names another model: a device keeps its model (422, field `/model_id`).
pub const MODEL_IMMUTABLE: MessageKey = MessageKey::new("device.model_immutable");
/// Another device of the gateway uses the same host and port with the other transport (409);
/// `details.device_id` and `details.transport` are that device's.
pub const TRANSPORT_MISMATCH: MessageKey = MessageKey::new("device.transport_mismatch");
/// Another device of the gateway has the unit ID at the same host and port (409);
/// `details.device_id` is that device.
pub const UNIT_TAKEN: MessageKey = MessageKey::new("device.unit_taken");
/// The device changed since the revision the update expected (409); `details.current`.
pub const REVISION_MISMATCH: MessageKey = MessageKey::new("device.revision_mismatch");
/// The device's gateway binding is no longer the gateway's current one (409): the gateway was
/// unbound, and perhaps bound again, so the device can only be deleted.
pub const BINDING_ENDED: MessageKey = MessageKey::new("device.binding_ended");

/// A device: a Modbus endpoint one of the tenant's gateways polls with one of its models.
#[derive(Clone, Debug, PartialEq, Serialize, ToSchema)]
pub struct Device {
    /// The device's ID; telemetry names the device by it.
    pub device_id: DeviceId,
    /// The name, unique in the tenant, never changed after creation: 1-40 lowercase letters or
    /// digits in groups joined by single hyphens, starting with a letter (the naming rule of
    /// models).
    #[schema(
        pattern = "^[a-z][a-z0-9]*(-[a-z0-9]+)*$",
        max_length = 40,
        example = "line-1-meter"
    )]
    pub name: String,
    /// The hardware ID of the gateway that polls the device, bound to the tenant; never
    /// changed after creation.
    #[schema(
        pattern = "^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$",
        max_length = 64,
        example = "tg-0001"
    )]
    pub hardware_id: String,
    /// The device model the gateway reads the device with; never changed after creation.
    pub model_id: DeviceModelId,
    /// How Modbus frames reach the device.
    #[schema(schema_with = transport_schema)]
    pub transport: String,
    /// The IP address or DNS name of the device, or of the converter it is behind; never the
    /// gateway itself (a loopback address or `localhost`).
    #[schema(max_length = 253, example = "192.0.2.10")]
    pub host: String,
    /// The TCP port. Converters use many ports (502, 4196, 8886, 8899, ...); no default.
    #[schema(minimum = 1, maximum = 65535, example = 502)]
    pub port: u16,
    /// The Modbus unit ID (station number): 0-255 for Modbus TCP, 1-247 for RTU over TCP; 1
    /// when a request leaves it out.
    #[schema(minimum = 0, maximum = 255, example = 1)]
    pub unit_id: u8,
    /// How often the gateway polls the device, in milliseconds: 1000 (1 s) to 86400000
    /// (1 day); 60000 (1 minute) when a request leaves it out. A plan may set a longer minimum.
    #[schema(minimum = 1000, maximum = 86_400_000, example = 60000)]
    pub poll_interval_ms: u32,
    /// 1 at creation, one more with every update; send it back with an update to refuse
    /// overwriting someone else's change.
    pub revision: u64,
    /// When the device was created, by the database clock.
    pub created_at: DateTime<Utc>,
    /// When the device was last changed.
    pub updated_at: DateTime<Utc>,
}

impl From<&StoredDevice> for Device {
    fn from(device: &StoredDevice) -> Self {
        Self {
            device_id: device.device_id,
            name: device.name.as_str().to_owned(),
            hardware_id: device.hardware_id.as_str().to_owned(),
            model_id: device.model_id,
            transport: wire_name(device.settings.transport),
            host: device.settings.host.as_str().to_owned(),
            port: device.settings.port,
            unit_id: device.settings.unit_id,
            poll_interval_ms: device.settings.poll_interval_ms,
            revision: device.revision,
            created_at: device.created_at,
            updated_at: device.updated_at,
        }
    }
}

/// The tenant's devices, by name.
#[derive(Clone, Debug, PartialEq, Serialize, ToSchema)]
pub struct DeviceList {
    /// Every device of the tenant.
    pub devices: Vec<Device>,
}

/// The body of `POST /api/assets/v1/devices`. Unknown fields are refused.
#[derive(ToSchema)]
// Read by utoipa only: the schema says `additionalProperties: false`, as document.rs enforces.
#[serde(deny_unknown_fields)]
#[allow(
    dead_code,
    reason = "documents the request body; the handler reads the JSON by hand (document.rs)"
)]
pub struct CreateDevice {
    /// The new device's name, unique in the tenant (see [`Device::name`]).
    #[schema(pattern = "^[a-z][a-z0-9]*(-[a-z0-9]+)*$", max_length = 40)]
    name: String,
    /// The gateway that polls it: one currently bound to the caller's tenant.
    #[schema(pattern = "^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$", max_length = 64)]
    hardware_id: String,
    /// One of the caller's tenant's device models.
    model_id: DeviceModelId,
    /// See [`Device::transport`].
    #[schema(schema_with = transport_schema)]
    transport: String,
    /// See [`Device::host`].
    #[schema(max_length = 253)]
    host: String,
    /// See [`Device::port`].
    #[schema(minimum = 1, maximum = 65535)]
    port: u16,
    /// See [`Device::unit_id`]; 1 when absent.
    #[schema(minimum = 0, maximum = 255)]
    unit_id: Option<u8>,
    /// See [`Device::poll_interval_ms`]; 60000 when absent.
    #[schema(minimum = 1000, maximum = 86_400_000)]
    poll_interval_ms: Option<u32>,
}

/// The body of `PUT /api/assets/v1/devices/{device_id}`: the device's settings, which replace
/// all of the current ones (a field left out takes its default). Unknown fields are refused.
#[derive(ToSchema)]
#[serde(deny_unknown_fields)]
#[allow(
    dead_code,
    reason = "documents the request body; the handler reads the JSON by hand (document.rs)"
)]
pub struct UpdateDevice {
    /// The revision the client last read (`revision` of the device it read): the update is
    /// refused with 409 when the device changed since. Required.
    #[schema(minimum = 1)]
    revision: u64,
    /// If sent, the device's current name: names never change.
    name: Option<String>,
    /// If sent, the device's current gateway: a device stays on its gateway.
    hardware_id: Option<String>,
    /// If sent, the device's current model: a device keeps its model.
    model_id: Option<DeviceModelId>,
    /// See [`Device::transport`].
    #[schema(schema_with = transport_schema)]
    transport: String,
    /// See [`Device::host`].
    #[schema(max_length = 253)]
    host: String,
    /// See [`Device::port`].
    #[schema(minimum = 1, maximum = 65535)]
    port: u16,
    /// See [`Device::unit_id`]; 1 when absent.
    #[schema(minimum = 0, maximum = 255)]
    unit_id: Option<u8>,
    /// See [`Device::poll_interval_ms`]; 60000 when absent.
    #[schema(minimum = 1000, maximum = 86_400_000)]
    poll_interval_ms: Option<u32>,
}

/// The transports, by crates/protocol's names.
fn transport_schema() -> Object {
    ObjectBuilder::new()
        .schema_type(Type::String)
        .description(Some(
            "`modbus_tcp`: Modbus TCP to the device. `rtu_over_tcp`: RTU frames over TCP to an \
             RS485 converter; the devices behind one converter (the same host and port on one \
             gateway) share its connection, so they all use this transport and each has its own \
             unit ID.",
        ))
        .enum_values(Some(TRANSPORTS.map(wire_name).to_vec()))
        .build()
}

/// The state of the routes.
#[derive(Clone)]
pub struct Routes {
    verifier: IdentityVerifier,
    db: TenantDb,
}

impl FromRef<Routes> for IdentityVerifier {
    fn from_ref(state: &Routes) -> Self {
        state.verifier.clone()
    }
}

/// The device routes.
pub fn routes(verifier: IdentityVerifier, db: TenantDb) -> Router {
    Router::new()
        .route(DEVICES_PATH, get(list_devices).post(create_device))
        .route(
            DEVICE_PATH,
            get(get_device).put(update_device).delete(delete_device),
        )
        .route_layer(axum::middleware::map_response(no_store))
        .with_state(Routes { verifier, db })
}

/// Marks every response of these routes as not to be stored, including the ones the handlers
/// never build: a 401 that the [`UserIdentity`] extractor answers carries no `cache-control`
/// of its own.
async fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// The tenant's devices.
#[utoipa::path(
    get,
    path = "/api/assets/v1/devices",
    tag = "assets",
    operation_id = "listDevices",
    responses(
        (status = 200, description = "The caller's tenant's devices, by name.", body = DeviceList),
        (status = 401, description = "No valid session (`unauthenticated`).", body = ErrorBody),
        (status = 403, description = "No tenant or no role in it (`permission_denied`).", body = ErrorBody),
        (status = 503, description = "The database failed (`unavailable`).", body = ErrorBody),
    )
)]
async fn list_devices(State(state): State<Routes>, user: UserIdentity) -> Response {
    if let Some(refused) = refuse(&user, TenantRole::Viewer, "list devices") {
        return refused;
    }
    match store::list(&state.db, &user).await {
        Ok(devices) => ok(
            StatusCode::OK,
            &DeviceList {
                devices: devices.iter().map(Device::from).collect(),
            },
        ),
        Err(failure) => store_error(&user, None, &failure, "could not list devices"),
    }
}

/// One device.
#[utoipa::path(
    get,
    path = "/api/assets/v1/devices/{device_id}",
    tag = "assets",
    operation_id = "getDevice",
    params(("device_id" = DeviceId, Path, description = "The device's ID.")),
    responses(
        (status = 200, description = "The device.", body = Device),
        (status = 401, description = "No valid session (`unauthenticated`).", body = ErrorBody),
        (status = 403, description = "No tenant or no role in it (`permission_denied`).", body = ErrorBody),
        (status = 404, description = "No such device in the caller's tenant (`not_found`).", body = ErrorBody),
        (status = 503, description = "The database failed (`unavailable`).", body = ErrorBody),
    )
)]
async fn get_device(
    State(state): State<Routes>,
    user: UserIdentity,
    device_id: Result<Path<String>, PathRejection>,
) -> Response {
    if let Some(refused) = refuse(&user, TenantRole::Viewer, "read a device") {
        return refused;
    }
    let Some(device_id) = parse_id(device_id) else {
        return error(ErrorBody::new(ErrorCode::NotFound));
    };
    match store::get(&state.db, &user, device_id).await {
        Ok(device) => ok(StatusCode::OK, &Device::from(&device)),
        Err(failure) => store_error(&user, Some(device_id), &failure, "could not read a device"),
    }
}

/// Creates a device on a gateway bound to the caller's tenant; `assets.device_created` is
/// written in the same transaction.
#[utoipa::path(
    post,
    path = "/api/assets/v1/devices",
    tag = "assets",
    operation_id = "createDevice",
    request_body = CreateDevice,
    responses(
        (status = 201, description = "The device was created; `Location` is its URL.", body = Device),
        (status = 400, description = "The `Content-Type` is not `application/json`, the body is larger than 16 KiB (`device.body_too_large`), or it is not a JSON object (`device.body_invalid`) (`invalid_request`).", body = ErrorBody),
        (status = 401, description = "No valid session (`unauthenticated`).", body = ErrorBody),
        (status = 403, description = "No tenant, or a role below editor (`permission_denied`).", body = ErrorBody),
        (status = 409, description = "The tenant has a device of this name (`device.name_taken`), or another device of the gateway at the same host and port uses the other transport (`device.transport_mismatch`) or the same unit ID (`device.unit_taken`) (`conflict`). Nothing was written.", body = ErrorBody),
        (status = 422, description = "A field rule is broken, the gateway is not bound to the caller's tenant (`device.gateway_not_bound`), or the tenant has no such model (`device.model_not_found`) (`validation_failed`); `field_errors` names each problem with a JSON Pointer and a `device.*` key. Nothing was written.", body = ErrorBody),
        (status = 503, description = "The database failed (`unavailable`). Nothing was written.", body = ErrorBody),
    )
)]
async fn create_device(
    State(state): State<Routes>,
    user: UserIdentity,
    headers: HeaderMap,
    body: Body,
) -> Response {
    if let Some(refused) = refuse(&user, TenantRole::Editor, "create a device") {
        return refused;
    }
    let body = match read_json(&headers, body).await {
        Ok(body) => body,
        Err(refused) => return error(refused),
    };
    let device = match document::parse_create(&body) {
        Ok(device) => device,
        Err(refused) => return error(refused),
    };
    match store::create(&state.db, &user, device).await {
        Ok(device) => {
            tracing::info!(
                user_id = %user.user_id(),
                tenant_id = %tenant_of(&user),
                device_id = %device.device_id,
                revision = device.revision,
                "created a device"
            );
            let mut response = ok(StatusCode::CREATED, &Device::from(&device));
            if let Ok(location) =
                HeaderValue::from_str(&format!("{DEVICES_PATH}/{}", device.device_id))
            {
                response.headers_mut().insert(LOCATION, location);
            }
            response
        }
        Err(failure) => store_error(&user, None, &failure, "could not create a device"),
    }
}

/// Replaces a device's connection settings and poll interval; `assets.device_updated` is
/// written in the same transaction.
#[utoipa::path(
    put,
    path = "/api/assets/v1/devices/{device_id}",
    tag = "assets",
    operation_id = "updateDevice",
    params(("device_id" = DeviceId, Path, description = "The device's ID.")),
    request_body = UpdateDevice,
    responses(
        (status = 200, description = "The device after the update, at its next revision.", body = Device),
        (status = 400, description = "The `Content-Type` is not `application/json`, the body is larger than 16 KiB (`device.body_too_large`), or it is not a JSON object (`device.body_invalid`) (`invalid_request`).", body = ErrorBody),
        (status = 401, description = "No valid session (`unauthenticated`).", body = ErrorBody),
        (status = 403, description = "No tenant, or a role below editor (`permission_denied`).", body = ErrorBody),
        (status = 404, description = "No such device in the caller's tenant (`not_found`).", body = ErrorBody),
        (status = 409, description = "The device's gateway binding has ended, so it can only be deleted (`device.binding_ended`), the device is no longer at the `revision` sent (`device.revision_mismatch`, `details.current`), or another device of the gateway at the same host and port uses the other transport (`device.transport_mismatch`) or the same unit ID (`device.unit_taken`) (`conflict`). Nothing was written.", body = ErrorBody),
        (status = 422, description = "A field rule is broken, `revision` is missing or not a positive integer, or `name`, `hardware_id` or `model_id` is not the device's (`device.name_immutable`, `device.gateway_immutable`, `device.model_immutable`) (`validation_failed`). Nothing was written.", body = ErrorBody),
        (status = 503, description = "The database failed (`unavailable`). Nothing was written.", body = ErrorBody),
    )
)]
async fn update_device(
    State(state): State<Routes>,
    user: UserIdentity,
    device_id: Result<Path<String>, PathRejection>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    if let Some(refused) = refuse(&user, TenantRole::Editor, "update a device") {
        return refused;
    }
    let Some(device_id) = parse_id(device_id) else {
        return error(ErrorBody::new(ErrorCode::NotFound));
    };
    let body = match read_json(&headers, body).await {
        Ok(body) => body,
        Err(refused) => return error(refused),
    };
    let change = match document::parse_update(&body) {
        Ok(change) => change,
        Err(refused) => return error(refused),
    };
    match store::replace(&state.db, &user, device_id, change).await {
        Ok(device) => {
            tracing::info!(
                user_id = %user.user_id(),
                tenant_id = %tenant_of(&user),
                device_id = %device.device_id,
                revision = device.revision,
                "updated a device"
            );
            ok(StatusCode::OK, &Device::from(&device))
        }
        Err(failure) => store_error(
            &user,
            Some(device_id),
            &failure,
            "could not update a device",
        ),
    }
}

/// Deletes a device; `assets.device_deleted` is written in the same transaction.
#[utoipa::path(
    delete,
    path = "/api/assets/v1/devices/{device_id}",
    tag = "assets",
    operation_id = "deleteDevice",
    params(("device_id" = DeviceId, Path, description = "The device's ID.")),
    responses(
        (status = 204, description = "The device was deleted."),
        (status = 401, description = "No valid session (`unauthenticated`).", body = ErrorBody),
        (status = 403, description = "No tenant, or a role below editor (`permission_denied`).", body = ErrorBody),
        (status = 404, description = "No such device in the caller's tenant (`not_found`).", body = ErrorBody),
        (status = 503, description = "The database failed (`unavailable`). Nothing was deleted.", body = ErrorBody),
    )
)]
async fn delete_device(
    State(state): State<Routes>,
    user: UserIdentity,
    device_id: Result<Path<String>, PathRejection>,
) -> Response {
    if let Some(refused) = refuse(&user, TenantRole::Editor, "delete a device") {
        return refused;
    }
    let Some(device_id) = parse_id(device_id) else {
        return error(ErrorBody::new(ErrorCode::NotFound));
    };
    match store::delete(&state.db, &user, device_id).await {
        Ok(revision) => {
            tracing::info!(
                user_id = %user.user_id(),
                tenant_id = %tenant_of(&user),
                device_id = %device_id,
                revision,
                "deleted a device"
            );
            (StatusCode::NO_CONTENT, [(CACHE_CONTROL, "no-store")]).into_response()
        }
        Err(failure) => store_error(
            &user,
            Some(device_id),
            &failure,
            "could not delete a device",
        ),
    }
}

/// The 403 to answer when the user has no tenant or their role there does not include `needed`;
/// `None` when the request may go on.
fn refuse(user: &UserIdentity, needed: TenantRole, action: &str) -> Option<Response> {
    match user.tenant() {
        Some(grant) if grant.role.includes(needed) => None,
        _ => {
            tracing::info!(
                user_id = %user.user_id(),
                action,
                "refused: no tenant or not enough of a role in it"
            );
            Some(error(ErrorBody::new(ErrorCode::PermissionDenied)))
        }
    }
}

/// The tenant of an authorized user, for the logs.
fn tenant_of(user: &UserIdentity) -> String {
    user.tenant_id()
        .map(|tenant| tenant.to_string())
        .unwrap_or_default()
}

/// The path's device ID; `None` (answered 404) when it is not a UUID.
fn parse_id(path: Result<Path<String>, PathRejection>) -> Option<DeviceId> {
    path.ok()?.0.parse().ok()
}

/// Whether the request has exactly one `Content-Type`, `application/json` with or without
/// parameters; compared without regard to ASCII case. A page of another origin cannot send that
/// type with the user's cookie without a CORS preflight, which nothing answers, so it cannot
/// change devices in the user's name (as the device model API).
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

/// The body of a write, at most [`MAX_BODY_BYTES`], after the content type check.
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

/// The response for a store failure; failures that are not the request's fault are logged at
/// ERROR with their kind only.
fn store_error(
    user: &UserIdentity,
    device_id: Option<DeviceId>,
    failure: &StoreError,
    message: &'static str,
) -> Response {
    if let Some(body) = refusal(failure) {
        return error(body);
    }
    tracing::error!(
        user_id = %user.user_id(),
        device_id = device_id.map(|id| id.to_string()).as_deref(),
        error_kind = failure.log_kind(),
        "{message}"
    );
    error(ErrorBody::new(ErrorCode::Unavailable))
}

/// The answer to a failure that is the request's own (a refusal); `None` for a failure of the
/// service or the database.
fn refusal(failure: &StoreError) -> Option<ErrorBody> {
    let body = match failure {
        StoreError::NoTenant => ErrorBody::new(ErrorCode::PermissionDenied),
        StoreError::NotFound => ErrorBody::new(ErrorCode::NotFound),
        StoreError::GatewayNotBound => ErrorBody::new(ErrorCode::ValidationFailed)
            .with_field_error(FieldError::new("/hardware_id", GATEWAY_NOT_BOUND)),
        StoreError::ModelNotFound => ErrorBody::new(ErrorCode::ValidationFailed)
            .with_field_error(FieldError::new("/model_id", MODEL_NOT_FOUND)),
        StoreError::NameTaken => ErrorBody::new(ErrorCode::Conflict).with_message_key(NAME_TAKEN),
        StoreError::TransportMismatch {
            device_id,
            transport,
        } => {
            let mut body = ErrorBody::new(ErrorCode::Conflict).with_message_key(TRANSPORT_MISMATCH);
            if let Some(other) = device_id {
                body = body.with_detail("device_id", other.to_string());
            }
            if let Some(transport) = transport {
                body = body.with_detail("transport", wire_name(*transport));
            }
            body
        }
        StoreError::UnitTaken { device_id } => {
            let mut body = ErrorBody::new(ErrorCode::Conflict).with_message_key(UNIT_TAKEN);
            if let Some(other) = device_id {
                body = body.with_detail("device_id", other.to_string());
            }
            body
        }
        StoreError::Immutable(changed) => {
            let mut body = ErrorBody::new(ErrorCode::ValidationFailed);
            for (sent, field, key) in [
                (changed.name, "/name", NAME_IMMUTABLE),
                (changed.hardware_id, "/hardware_id", GATEWAY_IMMUTABLE),
                (changed.model_id, "/model_id", MODEL_IMMUTABLE),
            ] {
                if sent {
                    body = body.with_field_error(FieldError::new(field, key));
                }
            }
            body
        }
        StoreError::BindingEnded => {
            ErrorBody::new(ErrorCode::Conflict).with_message_key(BINDING_ENDED)
        }
        StoreError::RevisionMismatch { current } => ErrorBody::new(ErrorCode::Conflict)
            .with_message_key(REVISION_MISMATCH)
            .with_detail("current", *current),
        StoreError::Corrupt
        | StoreError::Database(_)
        | StoreError::Publish(_)
        | StoreError::Aggregate(_) => return None,
    };
    Some(body)
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
    use protocol::config::MAX_POLL_INTERVAL_MS;
    use serde_json::json;

    use super::document::{DEFAULT_POLL_INTERVAL_MS, DEFAULT_UNIT_ID, MIN_POLL_INTERVAL_MS};
    use super::*;

    /// The OpenAPI document states these numbers in attributes, which cannot name the constants.
    #[test]
    fn the_documented_defaults_and_ranges_are_the_rules() {
        assert_eq!(DEFAULT_POLL_INTERVAL_MS, 60_000);
        assert_eq!(MIN_POLL_INTERVAL_MS, 1_000);
        assert_eq!(MAX_POLL_INTERVAL_MS, 86_400_000);
        assert_eq!(DEFAULT_UNIT_ID, 1);
        assert_eq!(MAX_BODY_BYTES, 16 * 1024);
    }

    #[test]
    fn path_ids_must_be_uuids() {
        assert!(parse_id(Ok(Path("01928f6e-7a3c-7d4e-8f00-000000000d01".to_owned()))).is_some());
        assert!(parse_id(Ok(Path("line-1-meter".to_owned()))).is_none());
        assert!(parse_id(Ok(Path(String::new()))).is_none());
    }

    #[test]
    fn writes_need_a_json_content_type() {
        let mut headers = HeaderMap::new();
        assert!(!is_json(&headers));
        headers.insert(
            CONTENT_TYPE,
            "application/json; charset=utf-8".parse().unwrap(),
        );
        assert!(is_json(&headers));
        headers.append(CONTENT_TYPE, "application/json".parse().unwrap());
        assert!(!is_json(&headers), "two content types");
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, "text/plain".parse().unwrap());
        assert!(!is_json(&headers));
    }

    #[test]
    fn conflicts_name_the_other_device() {
        let other: DeviceId = "01928f6e-7a3c-7d4e-8f00-000000000d02".parse().unwrap();
        let body = |failure: StoreError| -> serde_json::Value {
            serde_json::to_value(refusal(&failure).expect("a refusal")).unwrap()
        };
        let mismatch = body(StoreError::TransportMismatch {
            device_id: Some(other),
            transport: Some(protocol::config::Transport::RtuOverTcp),
        });
        assert_eq!(mismatch["code"], "conflict");
        assert_eq!(mismatch["message_key"], "device.transport_mismatch");
        assert_eq!(
            mismatch["details"],
            json!({"device_id": other.to_string(), "transport": "rtu_over_tcp"})
        );
        let taken = body(StoreError::UnitTaken {
            device_id: Some(other),
        });
        assert_eq!(taken["message_key"], "device.unit_taken");
        assert_eq!(taken["details"], json!({"device_id": other.to_string()}));
        let immutable = body(StoreError::Immutable(store::Changed {
            name: true,
            hardware_id: false,
            model_id: true,
        }));
        assert_eq!(immutable["code"], "validation_failed");
        assert!(refusal(&StoreError::Corrupt).is_none());
        assert_eq!(
            immutable["field_errors"],
            json!([
                {"field": "/name", "message_key": "device.name_immutable"},
                {"field": "/model_id", "message_key": "device.model_immutable"},
            ])
        );
    }
}
