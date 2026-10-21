//! Device models (Laysi/trustiot-v2#99, rebuild checklist C1 and F5; services/assets/README.md,
//! "設備型號"): a tenant's register maps, which devices of one type share and the gateway
//! polls.
//!
//! | Request | Role | Success |
//! | --- | --- | --- |
//! | `GET /api/assets/v1/device-models` | viewer | 200 [`DeviceModelList`] |
//! | `POST /api/assets/v1/device-models` | editor | 201 [`DeviceModel`], `assets.device_model_created` |
//! | `POST /api/assets/v1/device-models/import` | editor | 201 [`DeviceModel`], `assets.device_model_created` |
//! | `GET /api/assets/v1/device-models/{model_id}` | viewer | 200 [`DeviceModel`] |
//! | `GET /api/assets/v1/device-models/{model_id}/export` | viewer | 200 [`DeviceModelDocument`], a download |
//! | `PUT /api/assets/v1/device-models/{model_id}` | editor | 200 [`DeviceModel`], `assets.device_model_updated` |
//! | `DELETE /api/assets/v1/device-models/{model_id}` | editor | 204, `assets.device_model_deleted` |
//!
//! Every write entry, the import included, parses its body with [`document`] and passes the
//! same rules before anything is written; the store ([`store`]) writes the change and its event
//! in one transaction of the caller's tenant. Errors follow M1:
//!
//! | Request | Response |
//! | --- | --- |
//! | No valid internal identity token | 401 `unauthenticated` |
//! | No tenant, or a role below the one the request needs | 403 `permission_denied` |
//! | A `model_id` that is not a UUID, or no model of the caller's tenant | 404 `not_found` |
//! | A write whose `Content-Type` is not `application/json` (parameters allowed) | 400 `invalid_request` |
//! | A body over [`MAX_BODY_BYTES`] | 400 `invalid_request`, key `device_model.body_too_large`, `{"max_bytes"}` |
//! | A body that is not a JSON object | 400 `invalid_request`, key `device_model.body_invalid` |
//! | A field or point rule broken | 422 `validation_failed`, one field error per problem (JSON Pointer and key) |
//! | An update with another name than the model's | 422, field `/name`, key `device_model.name_immutable` |
//! | A name the tenant already has | 409 `conflict`, key `device_model.name_taken` |
//! | An update whose `revision` is not the current one | 409 `conflict`, key `device_model.revision_mismatch`, `{"current"}` |
//! | Deleting a model a device uses | 409 `conflict`, key `device_model.in_use` |
//! | The tenant's `identity.tenant_created` not processed yet | 503 `unavailable`, key `device_model.tenant_not_ready` |
//! | The database failed | 503 `unavailable`; nothing was written |
//!
//! Every response carries `cache-control: no-store`. A write that changed something logs one
//! INFO line with the user, tenant, model and revision, never names, keys or descriptions.

pub mod document;
pub mod store;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::rejection::PathRejection;
use axum::extract::{FromRef, Path, State};
use axum::http::header::{CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_TYPE, LOCATION};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use chrono::{DateTime, Utc};
use contract::{DeviceModelId, ErrorBody, ErrorCode, FieldError, MessageKey, TenantRole};
use protocol::PointKey;
use protocol::config::{ByteOrder, DataType, FunctionCode};
use serde::Serialize;
use service_kit::identity::{IdentityVerifier, UserIdentity};
use service_kit::tenant::TenantDb;
use utoipa::ToSchema;
use utoipa::openapi::schema::{Object, ObjectBuilder, Type};

use self::document::{
    BYTE_ORDERS, DATA_TYPES, DOCUMENT_FORMAT, DOCUMENT_FORMAT_VERSION, NAME_IMMUTABLE, Point,
    wire_name,
};
use self::store::{StoreError, StoredModel, Summary};

/// The collection, as the ingress gateway forwards it (no prefix is stripped).
pub const MODELS_PATH: &str = "/api/assets/v1/device-models";
/// One model.
pub const MODEL_PATH: &str = "/api/assets/v1/device-models/{model_id}";
/// One model as a document to download.
pub const EXPORT_PATH: &str = "/api/assets/v1/device-models/{model_id}/export";
/// A downloaded document, uploaded as a new model.
pub const IMPORT_PATH: &str = "/api/assets/v1/device-models/import";

/// The largest body a write accepts: a model of the most points with long descriptions fits.
pub const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;

/// The body is larger than [`MAX_BODY_BYTES`] (400); `details.max_bytes`.
pub const BODY_TOO_LARGE: MessageKey = MessageKey::new("device_model.body_too_large");
/// The tenant has a model of the name (409).
pub const NAME_TAKEN: MessageKey = MessageKey::new("device_model.name_taken");
/// The model changed since the revision the update expected (409); `details.current`.
pub const REVISION_MISMATCH: MessageKey = MessageKey::new("device_model.revision_mismatch");
/// A device uses the model, so it cannot be deleted (409).
pub const IN_USE: MessageKey = MessageKey::new("device_model.in_use");
/// The service has not processed the tenant's creation yet (503); retrying later helps.
pub const TENANT_NOT_READY: MessageKey = MessageKey::new("device_model.tenant_not_ready");

/// A data point of a device model: how the gateway reads one value, and what the value means.
#[derive(Clone, Debug, PartialEq, Serialize, ToSchema)]
pub struct DeviceModelPoint {
    /// The key telemetry uses, unique within the model: 1-64 ASCII letters, digits, `.`, `_`
    /// or `-`, starting with a letter or digit.
    #[schema(
        value_type = String,
        pattern = "^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$",
        max_length = 64,
        example = "voltage"
    )]
    pub key: PointKey,
    /// The Modbus read function: 1 coils, 2 discrete inputs, 3 holding registers, 4 input
    /// registers.
    #[schema(value_type = u8, minimum = 1, maximum = 4)]
    pub function_code: FunctionCode,
    /// The 0-based protocol address of the first coil or register (40001 in a device manual is
    /// address 0). A point's registers end at 65535 at most and do not overlap another point's
    /// of the same function code.
    #[schema(minimum = 0, maximum = 65535)]
    pub address: u16,
    /// How the registers decode. Function codes 1 and 2 read `bool` only.
    #[schema(schema_with = data_type_schema)]
    pub data_type: DataType,
    /// The order of the bytes of a multi-byte value; `abcd` when absent in a request. Single
    /// bits have none; strings only `abcd` or `badc`.
    #[schema(schema_with = byte_order_schema, required = false)]
    pub byte_order: ByteOrder,
    /// For `string` only, which requires it: the number of registers, 2 characters each.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(minimum = 1, maximum = 125)]
    pub length: Option<u16>,
    /// Multiplier applied to the raw number (rebuild checklist F5); finite and not 0. Numeric
    /// types only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scale: Option<f64>,
    /// Added after `scale`; finite. Numeric types only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offset: Option<f64>,
    /// What the value means, for people and for the AI features: up to 512 characters, without
    /// control characters other than line feed and tab, format characters, or line and
    /// paragraph separators. Trimmed; empty means none.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(max_length = 512)]
    pub description: Option<String>,
}

impl From<&Point> for DeviceModelPoint {
    fn from(point: &Point) -> Self {
        let Point { point, description } = point;
        Self {
            key: point.key.clone(),
            function_code: point.function_code,
            address: point.address,
            data_type: point.data_type,
            byte_order: point.byte_order,
            length: point.length,
            scale: point.scale,
            offset: point.offset,
            description: description.clone(),
        }
    }
}

/// A device model.
#[derive(Clone, Debug, PartialEq, Serialize, ToSchema)]
pub struct DeviceModel {
    /// The model's ID.
    pub model_id: DeviceModelId,
    /// The name, unique in the tenant, never changed after creation: 1-40 lowercase letters or
    /// digits in groups joined by single hyphens, starting with a letter.
    #[schema(
        pattern = "^[a-z][a-z0-9]*(-[a-z0-9]+)*$",
        max_length = 40,
        example = "power-meter"
    )]
    pub name: String,
    /// 1 at creation, one more with every update; send it back with an update to refuse
    /// overwriting someone else's change.
    pub revision: u64,
    /// When the model was created, by the database clock.
    pub created_at: DateTime<Utc>,
    /// When the model was last changed.
    pub updated_at: DateTime<Utc>,
    /// The data points, in order.
    pub points: Vec<DeviceModelPoint>,
}

impl From<&StoredModel> for DeviceModel {
    fn from(model: &StoredModel) -> Self {
        Self {
            model_id: model.model_id,
            name: model.name.as_str().to_owned(),
            revision: model.revision,
            created_at: model.created_at,
            updated_at: model.updated_at,
            points: model.points.iter().map(DeviceModelPoint::from).collect(),
        }
    }
}

/// A device model in the tenant's list.
#[derive(Clone, Debug, PartialEq, Serialize, ToSchema)]
pub struct DeviceModelSummary {
    /// The model's ID.
    pub model_id: DeviceModelId,
    /// The name.
    pub name: String,
    /// The current revision.
    pub revision: u64,
    /// How many data points it has.
    pub point_count: u64,
    /// When the model was created.
    pub created_at: DateTime<Utc>,
    /// When the model was last changed.
    pub updated_at: DateTime<Utc>,
}

impl From<&Summary> for DeviceModelSummary {
    fn from(summary: &Summary) -> Self {
        Self {
            model_id: summary.model_id,
            name: summary.name.as_str().to_owned(),
            revision: summary.revision,
            point_count: summary.point_count,
            created_at: summary.created_at,
            updated_at: summary.updated_at,
        }
    }
}

/// The tenant's device models, by name.
#[derive(Clone, Debug, PartialEq, Serialize, ToSchema)]
pub struct DeviceModelList {
    /// Every model of the tenant.
    pub models: Vec<DeviceModelSummary>,
}

/// A device model as a file: what the export downloads and the import uploads. It holds no
/// IDs, revisions or times, so it can be imported in another tenant or deployment.
#[derive(Clone, Debug, PartialEq, Serialize, ToSchema)]
pub struct DeviceModelDocument {
    /// Always `trustiot.device_model`.
    #[schema(example = "trustiot.device_model")]
    pub format: String,
    /// The document's version; this release writes and reads 1.
    #[schema(minimum = 1, maximum = 1)]
    pub format_version: u64,
    /// The model's name, under the naming rule of [`DeviceModel::name`].
    #[schema(pattern = "^[a-z][a-z0-9]*(-[a-z0-9]+)*$", max_length = 40)]
    pub name: String,
    /// The data points, in order.
    pub points: Vec<DeviceModelPoint>,
}

impl From<&StoredModel> for DeviceModelDocument {
    fn from(model: &StoredModel) -> Self {
        Self {
            format: DOCUMENT_FORMAT.to_owned(),
            format_version: DOCUMENT_FORMAT_VERSION,
            name: model.name.as_str().to_owned(),
            points: model.points.iter().map(DeviceModelPoint::from).collect(),
        }
    }
}

/// The body of `POST /api/assets/v1/device-models`. Unknown fields are refused.
#[derive(ToSchema)]
#[allow(
    dead_code,
    reason = "documents the request body; the handler reads the JSON by hand (document.rs)"
)]
pub struct CreateDeviceModel {
    /// The new model's name, unique in the tenant (see [`DeviceModel::name`]).
    #[schema(pattern = "^[a-z][a-z0-9]*(-[a-z0-9]+)*$", max_length = 40)]
    name: String,
    /// The data points, at most 1000, in order; may be empty.
    #[schema(max_items = 1000)]
    points: Vec<DeviceModelPoint>,
}

/// The body of `PUT /api/assets/v1/device-models/{model_id}`: the new data points, which replace
/// all of the model's. Unknown fields are refused.
#[derive(ToSchema)]
#[allow(
    dead_code,
    reason = "documents the request body; the handler reads the JSON by hand (document.rs)"
)]
pub struct UpdateDeviceModel {
    /// The model's name, if sent, must be its current name: names never change.
    name: Option<String>,
    /// The revision the client last read, if sent: the update is refused with 409 when the model
    /// changed since.
    #[schema(minimum = 1)]
    revision: Option<u64>,
    /// The data points, at most 1000, in order; may be empty.
    #[schema(max_items = 1000)]
    points: Vec<DeviceModelPoint>,
}

fn enum_schema(description: &str, names: Vec<String>) -> Object {
    ObjectBuilder::new()
        .schema_type(Type::String)
        .description(Some(description))
        .enum_values(Some(names))
        .build()
}

/// The data types, by crates/protocol's names.
fn data_type_schema() -> Object {
    enum_schema(
        "How the registers decode: `bool` (a bit, or a register that is not 0), `u16`/`i16` (1 \
         register), `u32`/`i32`/`f32` (2), `u64`/`i64`/`f64` (4), `string` (`length` registers \
         of ASCII). Function codes 1 and 2 read `bool` only.",
        DATA_TYPES.map(wire_name).to_vec(),
    )
}

/// The byte orders, by crates/protocol's names.
fn byte_order_schema() -> Object {
    enum_schema(
        "Where the bytes of the 32-bit value 0xAABBCCDD end up in the registers: `abcd` \
         big-endian (the default), `badc` bytes swapped in each register, `cdab` registers \
         reversed, `dcba` both.",
        BYTE_ORDERS.map(wire_name).to_vec(),
    )
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

/// The device model routes.
pub fn routes(verifier: IdentityVerifier, db: TenantDb) -> Router {
    Router::new()
        .route(MODELS_PATH, get(list_models).post(create_model))
        .route(IMPORT_PATH, axum::routing::post(import_model))
        .route(
            MODEL_PATH,
            get(get_model).put(update_model).delete(delete_model),
        )
        .route(EXPORT_PATH, get(export_model))
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

/// The tenant's device models.
#[utoipa::path(
    get,
    path = "/api/assets/v1/device-models",
    tag = "assets",
    operation_id = "listDeviceModels",
    responses(
        (status = 200, description = "The caller's tenant's models, by name.", body = DeviceModelList),
        (status = 401, description = "No valid session (`unauthenticated`).", body = ErrorBody),
        (status = 403, description = "No tenant or no role in it (`permission_denied`).", body = ErrorBody),
        (status = 503, description = "The database failed (`unavailable`).", body = ErrorBody),
    )
)]
async fn list_models(State(state): State<Routes>, user: UserIdentity) -> Response {
    if let Some(refused) = refuse(&user, TenantRole::Viewer, "list device models") {
        return refused;
    }
    match store::list(&state.db, &user).await {
        Ok(models) => ok(
            StatusCode::OK,
            &DeviceModelList {
                models: models.iter().map(DeviceModelSummary::from).collect(),
            },
        ),
        Err(failure) => store_error(&user, None, &failure, "could not list device models"),
    }
}

/// One device model with its data points.
#[utoipa::path(
    get,
    path = "/api/assets/v1/device-models/{model_id}",
    tag = "assets",
    operation_id = "getDeviceModel",
    params(("model_id" = DeviceModelId, Path, description = "The model's ID.")),
    responses(
        (status = 200, description = "The model.", body = DeviceModel),
        (status = 401, description = "No valid session (`unauthenticated`).", body = ErrorBody),
        (status = 403, description = "No tenant or no role in it (`permission_denied`).", body = ErrorBody),
        (status = 404, description = "No such model in the caller's tenant (`not_found`).", body = ErrorBody),
        (status = 503, description = "The database failed (`unavailable`).", body = ErrorBody),
    )
)]
async fn get_model(
    State(state): State<Routes>,
    user: UserIdentity,
    model_id: Result<Path<String>, PathRejection>,
) -> Response {
    if let Some(refused) = refuse(&user, TenantRole::Viewer, "read a device model") {
        return refused;
    }
    let Some(model_id) = parse_id(model_id) else {
        return error(ErrorBody::new(ErrorCode::NotFound));
    };
    match store::get(&state.db, &user, model_id).await {
        Ok(model) => ok(StatusCode::OK, &DeviceModel::from(&model)),
        Err(failure) => store_error(
            &user,
            Some(model_id),
            &failure,
            "could not read a device model",
        ),
    }
}

/// One device model as a document to download, which the import accepts.
#[utoipa::path(
    get,
    path = "/api/assets/v1/device-models/{model_id}/export",
    tag = "assets",
    operation_id = "exportDeviceModel",
    params(("model_id" = DeviceModelId, Path, description = "The model's ID.")),
    responses(
        (status = 200, description = "The model as a document, with `Content-Disposition: attachment` and the model's name as the file name.", body = DeviceModelDocument),
        (status = 401, description = "No valid session (`unauthenticated`).", body = ErrorBody),
        (status = 403, description = "No tenant or no role in it (`permission_denied`).", body = ErrorBody),
        (status = 404, description = "No such model in the caller's tenant (`not_found`).", body = ErrorBody),
        (status = 503, description = "The database failed (`unavailable`).", body = ErrorBody),
    )
)]
async fn export_model(
    State(state): State<Routes>,
    user: UserIdentity,
    model_id: Result<Path<String>, PathRejection>,
) -> Response {
    if let Some(refused) = refuse(&user, TenantRole::Viewer, "export a device model") {
        return refused;
    }
    let Some(model_id) = parse_id(model_id) else {
        return error(ErrorBody::new(ErrorCode::NotFound));
    };
    match store::get(&state.db, &user, model_id).await {
        Ok(model) => {
            let mut response = ok(StatusCode::OK, &DeviceModelDocument::from(&model));
            // The name is lowercase letters, digits and hyphens only, so it needs no quoting
            // beyond the quotes.
            let disposition = format!("attachment; filename=\"{}.device-model.json\"", model.name);
            if let Ok(value) = HeaderValue::from_str(&disposition) {
                response.headers_mut().insert(CONTENT_DISPOSITION, value);
            }
            response
        }
        Err(failure) => store_error(
            &user,
            Some(model_id),
            &failure,
            "could not export a device model",
        ),
    }
}

/// Creates a device model; `assets.device_model_created` is written in the same transaction.
#[utoipa::path(
    post,
    path = "/api/assets/v1/device-models",
    tag = "assets",
    operation_id = "createDeviceModel",
    request_body = CreateDeviceModel,
    responses(
        (status = 201, description = "The model was created; `Location` is its URL.", body = DeviceModel),
        (status = 400, description = "The `Content-Type` is not `application/json`, the body is larger than 2 MiB (`device_model.body_too_large`), or it is not a JSON object (`device_model.body_invalid`) (`invalid_request`).", body = ErrorBody),
        (status = 401, description = "No valid session (`unauthenticated`).", body = ErrorBody),
        (status = 403, description = "No tenant, or a role below editor (`permission_denied`).", body = ErrorBody),
        (status = 409, description = "The tenant has a model of this name (`conflict`, `device_model.name_taken`).", body = ErrorBody),
        (status = 422, description = "A naming or data point rule is broken (`validation_failed`); `field_errors` names each problem with a JSON Pointer and a `device_model.*` key. Nothing was written.", body = ErrorBody),
        (status = 503, description = "The service does not know the tenant yet (`device_model.tenant_not_ready`), or the database failed (`unavailable`). Nothing was written.", body = ErrorBody),
    )
)]
async fn create_model(
    State(state): State<Routes>,
    user: UserIdentity,
    headers: HeaderMap,
    body: Body,
) -> Response {
    create_from(state, user, &headers, body, Entry::Create).await
}

/// Creates a device model from an exported document, under the same rules as a create.
#[utoipa::path(
    post,
    path = "/api/assets/v1/device-models/import",
    tag = "assets",
    operation_id = "importDeviceModel",
    request_body = DeviceModelDocument,
    responses(
        (status = 201, description = "The model was created; `Location` is its URL.", body = DeviceModel),
        (status = 400, description = "The `Content-Type` is not `application/json`, the body is larger than 2 MiB (`device_model.body_too_large`), or it is not a JSON object (`device_model.body_invalid`) (`invalid_request`).", body = ErrorBody),
        (status = 401, description = "No valid session (`unauthenticated`).", body = ErrorBody),
        (status = 403, description = "No tenant, or a role below editor (`permission_denied`).", body = ErrorBody),
        (status = 409, description = "The tenant has a model of this name (`conflict`, `device_model.name_taken`).", body = ErrorBody),
        (status = 422, description = "The document is not `trustiot.device_model` version 1, or a naming or data point rule is broken (`validation_failed`). Nothing was written.", body = ErrorBody),
        (status = 503, description = "The service does not know the tenant yet (`device_model.tenant_not_ready`), or the database failed (`unavailable`). Nothing was written.", body = ErrorBody),
    )
)]
async fn import_model(
    State(state): State<Routes>,
    user: UserIdentity,
    headers: HeaderMap,
    body: Body,
) -> Response {
    create_from(state, user, &headers, body, Entry::Import).await
}

/// How an entry that creates a model reads its body.
type ParseNew = fn(&[u8]) -> Result<document::NewModel, ErrorBody>;

/// The two entries that create a model.
#[derive(Clone, Copy, Debug)]
enum Entry {
    Create,
    Import,
}

async fn create_from(
    state: Routes,
    user: UserIdentity,
    headers: &HeaderMap,
    body: Body,
    entry: Entry,
) -> Response {
    let (action, parse): (&str, ParseNew) = match entry {
        Entry::Create => ("create a device model", document::parse_create),
        Entry::Import => ("import a device model", document::parse_import),
    };
    if let Some(refused) = refuse(&user, TenantRole::Editor, action) {
        return refused;
    }
    let body = match read_json(headers, body).await {
        Ok(body) => body,
        Err(refused) => return error(refused),
    };
    let model = match parse(&body) {
        Ok(model) => model,
        Err(refused) => return error(refused),
    };
    match store::create(&state.db, &user, model).await {
        Ok(model) => {
            tracing::info!(
                user_id = %user.user_id(),
                tenant_id = %tenant_of(&user),
                model_id = %model.model_id,
                revision = model.revision,
                via = match entry {
                    Entry::Create => "create",
                    Entry::Import => "import",
                },
                "created a device model"
            );
            let mut response = ok(StatusCode::CREATED, &DeviceModel::from(&model));
            if let Ok(location) =
                HeaderValue::from_str(&format!("{MODELS_PATH}/{}", model.model_id))
            {
                response.headers_mut().insert(LOCATION, location);
            }
            response
        }
        Err(failure) => store_error(&user, None, &failure, "could not create a device model"),
    }
}

/// Replaces the data points of a device model; `assets.device_model_updated` is written in the
/// same transaction.
#[utoipa::path(
    put,
    path = "/api/assets/v1/device-models/{model_id}",
    tag = "assets",
    operation_id = "updateDeviceModel",
    params(("model_id" = DeviceModelId, Path, description = "The model's ID.")),
    request_body = UpdateDeviceModel,
    responses(
        (status = 200, description = "The model after the update, at its next revision.", body = DeviceModel),
        (status = 400, description = "The `Content-Type` is not `application/json`, the body is larger than 2 MiB (`device_model.body_too_large`), or it is not a JSON object (`device_model.body_invalid`) (`invalid_request`).", body = ErrorBody),
        (status = 401, description = "No valid session (`unauthenticated`).", body = ErrorBody),
        (status = 403, description = "No tenant, or a role below editor (`permission_denied`).", body = ErrorBody),
        (status = 404, description = "No such model in the caller's tenant (`not_found`).", body = ErrorBody),
        (status = 409, description = "The model is no longer at the `revision` sent (`conflict`, `device_model.revision_mismatch`, `details.current`).", body = ErrorBody),
        (status = 422, description = "A data point rule is broken, or `name` is not the model's (`device_model.name_immutable`) (`validation_failed`). Nothing was written.", body = ErrorBody),
        (status = 503, description = "The database failed (`unavailable`). Nothing was written.", body = ErrorBody),
    )
)]
async fn update_model(
    State(state): State<Routes>,
    user: UserIdentity,
    model_id: Result<Path<String>, PathRejection>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    if let Some(refused) = refuse(&user, TenantRole::Editor, "update a device model") {
        return refused;
    }
    let Some(model_id) = parse_id(model_id) else {
        return error(ErrorBody::new(ErrorCode::NotFound));
    };
    let body = match read_json(&headers, body).await {
        Ok(body) => body,
        Err(refused) => return error(refused),
    };
    let replacement = match document::parse_update(&body) {
        Ok(replacement) => replacement,
        Err(refused) => return error(refused),
    };
    match store::replace(&state.db, &user, model_id, replacement).await {
        Ok(model) => {
            tracing::info!(
                user_id = %user.user_id(),
                tenant_id = %tenant_of(&user),
                model_id = %model.model_id,
                revision = model.revision,
                "updated a device model"
            );
            ok(StatusCode::OK, &DeviceModel::from(&model))
        }
        Err(failure) => store_error(
            &user,
            Some(model_id),
            &failure,
            "could not update a device model",
        ),
    }
}

/// Deletes a device model that no device uses; `assets.device_model_deleted` is written in the
/// same transaction.
#[utoipa::path(
    delete,
    path = "/api/assets/v1/device-models/{model_id}",
    tag = "assets",
    operation_id = "deleteDeviceModel",
    params(("model_id" = DeviceModelId, Path, description = "The model's ID.")),
    responses(
        (status = 204, description = "The model and its data points were deleted."),
        (status = 401, description = "No valid session (`unauthenticated`).", body = ErrorBody),
        (status = 403, description = "No tenant, or a role below editor (`permission_denied`).", body = ErrorBody),
        (status = 404, description = "No such model in the caller's tenant (`not_found`).", body = ErrorBody),
        (status = 409, description = "A device uses the model (`conflict`, `device_model.in_use`). Nothing was deleted.", body = ErrorBody),
        (status = 503, description = "The database failed (`unavailable`). Nothing was deleted.", body = ErrorBody),
    )
)]
async fn delete_model(
    State(state): State<Routes>,
    user: UserIdentity,
    model_id: Result<Path<String>, PathRejection>,
) -> Response {
    if let Some(refused) = refuse(&user, TenantRole::Editor, "delete a device model") {
        return refused;
    }
    let Some(model_id) = parse_id(model_id) else {
        return error(ErrorBody::new(ErrorCode::NotFound));
    };
    match store::delete(&state.db, &user, model_id).await {
        Ok((_, revision)) => {
            tracing::info!(
                user_id = %user.user_id(),
                tenant_id = %tenant_of(&user),
                model_id = %model_id,
                revision,
                "deleted a device model"
            );
            (StatusCode::NO_CONTENT, [(CACHE_CONTROL, "no-store")]).into_response()
        }
        Err(failure) => store_error(
            &user,
            Some(model_id),
            &failure,
            "could not delete a device model",
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

/// The path's model ID; `None` (answered 404) when it is not a UUID.
fn parse_id(path: Result<Path<String>, PathRejection>) -> Option<DeviceModelId> {
    path.ok()?.0.parse().ok()
}

/// Whether the request has exactly one `Content-Type`, `application/json` with or without
/// parameters; compared without regard to ASCII case. A page of another origin cannot send that
/// type with the user's cookie without a CORS preflight, which nothing answers, so it cannot
/// change models in the user's name (as `POST /api/identity/v1/tenants`).
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
    model_id: Option<DeviceModelId>,
    failure: &StoreError,
    message: &'static str,
) -> Response {
    let body = match failure {
        StoreError::NoTenant => ErrorBody::new(ErrorCode::PermissionDenied),
        StoreError::NotFound => ErrorBody::new(ErrorCode::NotFound),
        StoreError::NameTaken => ErrorBody::new(ErrorCode::Conflict).with_message_key(NAME_TAKEN),
        StoreError::NameImmutable => ErrorBody::new(ErrorCode::ValidationFailed)
            .with_field_error(FieldError::new("/name", NAME_IMMUTABLE)),
        StoreError::RevisionMismatch { current } => ErrorBody::new(ErrorCode::Conflict)
            .with_message_key(REVISION_MISMATCH)
            .with_detail("current", *current),
        StoreError::InUse => ErrorBody::new(ErrorCode::Conflict).with_message_key(IN_USE),
        StoreError::TenantNotReady => {
            ErrorBody::new(ErrorCode::Unavailable).with_message_key(TENANT_NOT_READY)
        }
        StoreError::Corrupt
        | StoreError::Database(_)
        | StoreError::Publish(_)
        | StoreError::Aggregate(_) => {
            tracing::error!(
                user_id = %user.user_id(),
                model_id = model_id.map(|id| id.to_string()).as_deref(),
                error_kind = failure.log_kind(),
                "{message}"
            );
            ErrorBody::new(ErrorCode::Unavailable)
        }
    };
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
    use serde_json::{Value, json};

    use super::*;

    fn point_json(point: &Point) -> Value {
        serde_json::to_value(DeviceModelPoint::from(point)).unwrap()
    }

    fn content_type(values: &[&str]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for value in values {
            headers.append(CONTENT_TYPE, value.parse().unwrap());
        }
        headers
    }

    #[test]
    fn writes_need_a_json_content_type() {
        for accepted in [
            &["application/json"][..],
            &["application/json; charset=utf-8"],
            &["Application/JSON"],
        ] {
            assert!(is_json(&content_type(accepted)), "{accepted:?}");
        }
        for refused in [
            &[][..],
            &["text/plain"],
            &["application/x-www-form-urlencoded"],
            &["multipart/form-data; boundary=x"],
            &["application/json", "application/json"],
            &["application/jsonx"],
        ] {
            assert!(!is_json(&content_type(refused)), "{refused:?}");
        }
    }

    #[test]
    fn a_point_serializes_in_the_documented_shape() {
        let created = document::parse_create(
            &serde_json::to_vec(&json!({"name": "m", "points": [
                {"key": "a", "function_code": 3, "address": 1, "data_type": "f32", "scale": 2.5,
                 "description": " Flow "},
                {"key": "b", "function_code": 2, "address": 0, "data_type": "bool"},
            ]}))
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            point_json(&created.points[0]),
            json!({"key": "a", "function_code": 3, "address": 1, "data_type": "f32",
                   "byte_order": "abcd", "scale": 2.5, "description": "Flow"})
        );
        assert_eq!(
            point_json(&created.points[1]),
            json!({"key": "b", "function_code": 2, "address": 0, "data_type": "bool",
                   "byte_order": "abcd"})
        );
    }

    #[test]
    fn path_ids_must_be_uuids() {
        assert!(parse_id(Ok(Path("01928f6e-7a3c-7d4e-8f00-000000000a01".to_owned()))).is_some());
        assert!(parse_id(Ok(Path("power-meter".to_owned()))).is_none());
        assert!(parse_id(Ok(Path(String::new()))).is_none());
    }
}
