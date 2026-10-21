//! The device model API (Laysi/trustiot-v2#99) against the real binary on the product's schema,
//! applied by the migration Job (`testkit::product`):
//!
//! - every write entry (create, update, import) refuses each naming and data point rule with
//!   a 422 naming the field, and writes nothing, not even an event (acceptance 1);
//! - every change writes its `assets.device_model_*` event in the model's tenant (acceptance 2);
//! - a duplicate name, renaming a model and deleting a model a device uses are refused
//!   (acceptance 4); a stale revision too;
//! - export and import round-trip every data type, byte order, length, scale, offset and
//!   description; roles decide who reads and who writes.
//!
//! The cross-tenant tests are `tests/cross_tenant.rs` (acceptance 3). Starts a throwaway
//! Postgres with TLS (podman or docker); skipped without either unless `CI` is set.

mod support;

use contract::events::assets::{
    DEVICE_MODEL_AGGREGATE, DeviceModelCreated, DeviceModelDeleted, DeviceModelUpdated,
};
use contract::{EventEnvelope, ServiceName, TenantId, TenantRole};
use reqwest::Method;
use serde_json::{Value, json};
use sqlx::PgConnection;
use support::{Reply, Service};
use testkit::identity::TestIssuer;
use testkit::product::{ProductPostgres, product_postgres};

const MODELS: &str = "/api/assets/v1/device-models";
const IMPORT: &str = "/api/assets/v1/device-models/import";

/// The tenant's row, as the consumer of `identity.tenant_created` writes it.
const INSERT_TENANT: &str = "INSERT INTO assets.tenants (tenant_id) VALUES ($1::uuid)";
const MODEL_ROWS: &str = "SELECT count(*) FROM assets.device_models WHERE tenant_id = $1::uuid";
const POINT_ROWS: &str =
    "SELECT count(*) FROM assets.device_model_points WHERE tenant_id = $1::uuid";
/// The tenant's events in the order consumers receive them.
const EVENTS: &str = "SELECT event_type, aggregate_type, aggregate_id, aggregate_seq,
    tenant_id::text, envelope::text
FROM assets.outbox_event WHERE tenant_id = $1::uuid ORDER BY tx_order, id";

/// A stand-in for the device table of Laysi/trustiot-v2#100, which will reference models the
/// same way: a device keeps its model from being deleted.
const CREATE_DEVICES: &str = "CREATE TABLE IF NOT EXISTS public.device_model_users (
    tenant_id uuid NOT NULL,
    device_id uuid NOT NULL,
    model_id uuid NOT NULL,
    PRIMARY KEY (tenant_id, device_id),
    FOREIGN KEY (tenant_id, model_id)
        REFERENCES assets.device_models (tenant_id, model_id) ON DELETE RESTRICT
)";
const INSERT_DEVICE: &str = "INSERT INTO public.device_model_users (tenant_id, device_id, model_id)
VALUES ($1::uuid, gen_random_uuid(), $2::uuid)";
const DELETE_DEVICES: &str = "DELETE FROM public.device_model_users WHERE tenant_id = $1::uuid";

/// One stored event.
#[derive(Debug)]
struct StoredEvent {
    event_type: String,
    aggregate_type: String,
    aggregate_id: String,
    aggregate_seq: i64,
    tenant_id: Option<String>,
    envelope: EventEnvelope,
    text: String,
}

async fn count(admin: &mut PgConnection, sql: &'static str, tenant: TenantId) -> i64 {
    sqlx::query_scalar(sql)
        .bind(tenant.to_string())
        .fetch_one(admin)
        .await
        .unwrap_or_else(|error| panic!("{sql}: {error}"))
}

async fn events(admin: &mut PgConnection, tenant: TenantId) -> Vec<StoredEvent> {
    let rows: Vec<(String, String, String, i64, Option<String>, String)> = sqlx::query_as(EVENTS)
        .bind(tenant.to_string())
        .fetch_all(admin)
        .await
        .expect("read the events");
    rows.into_iter()
        .map(
            |(event_type, aggregate_type, aggregate_id, aggregate_seq, tenant_id, text)| {
                StoredEvent {
                    event_type,
                    aggregate_type,
                    aggregate_id,
                    aggregate_seq,
                    tenant_id,
                    envelope: serde_json::from_str(&text).expect("an envelope"),
                    text,
                }
            },
        )
        .collect()
}

/// A tenant the service knows (its `identity.tenant_created` processed).
async fn known_tenant(admin: &mut PgConnection) -> TenantId {
    let tenant = TenantId::generate();
    sqlx::query(INSERT_TENANT)
        .bind(tenant.to_string())
        .execute(admin)
        .await
        .expect("insert the tenant");
    tenant
}

/// What the tenant's tables and outbox hold: models, points, events.
async fn footprint(admin: &mut PgConnection, tenant: TenantId) -> (i64, i64, usize) {
    (
        count(admin, MODEL_ROWS, tenant).await,
        count(admin, POINT_ROWS, tenant).await,
        events(admin, tenant).await.len(),
    )
}

/// A model with a point of each kind: scaled, offset with a byte order, a string and a coil.
fn power_meter_points() -> Value {
    json!([
        {"key": "voltage", "function_code": 3, "address": 0, "data_type": "u16", "scale": 0.1,
         "description": "Phase A voltage in volts"},
        {"key": "energy", "function_code": 4, "address": 0, "data_type": "u32",
         "byte_order": "cdab", "scale": 0.01, "offset": -40},
        {"key": "serial", "function_code": 3, "address": 10, "data_type": "string", "length": 8,
         "byte_order": "badc", "description": "序號，ASCII"},
        {"key": "running", "function_code": 1, "address": 0, "data_type": "bool"},
    ])
}

/// Every data type and byte order once, without overlapping.
fn every_type_points() -> Value {
    json!([
        {"key": "b", "function_code": 2, "address": 7, "data_type": "bool"},
        {"key": "flag", "function_code": 3, "address": 0, "data_type": "bool"},
        {"key": "u16", "function_code": 3, "address": 1, "data_type": "u16", "byte_order": "badc"},
        {"key": "i16", "function_code": 3, "address": 2, "data_type": "i16", "offset": 1.5},
        {"key": "u32", "function_code": 3, "address": 3, "data_type": "u32", "byte_order": "dcba"},
        {"key": "i32", "function_code": 3, "address": 5, "data_type": "i32", "scale": -2},
        {"key": "u64", "function_code": 4, "address": 0, "data_type": "u64"},
        {"key": "i64", "function_code": 4, "address": 4, "data_type": "i64", "byte_order": "cdab"},
        {"key": "f32", "function_code": 4, "address": 8, "data_type": "f32", "scale": 1e-3,
         "offset": 273.15, "description": "Temperature\nin kelvin"},
        {"key": "f64", "function_code": 4, "address": 10, "data_type": "f64"},
        {"key": "text", "function_code": 4, "address": 14, "data_type": "string", "length": 125},
    ])
}

/// `points` as the API answers them: `byte_order` always there.
fn as_answered(points: &Value) -> Value {
    let mut points = points.clone();
    for point in points.as_array_mut().expect("an array") {
        if point.get("byte_order").is_none() {
            point["byte_order"] = json!("abcd");
        }
        if let Some(offset) = point.get("offset").and_then(Value::as_f64) {
            point["offset"] = json!(offset);
        }
        if let Some(scale) = point.get("scale").and_then(Value::as_f64) {
            point["scale"] = json!(scale);
        }
    }
    points
}

/// `points` as the events carry them: crates/protocol's format, without descriptions.
fn as_gateway_reads(points: &Value) -> Vec<protocol::config::DataPoint> {
    let mut points = points.clone();
    for point in points.as_array_mut().expect("an array") {
        point
            .as_object_mut()
            .expect("an object")
            .remove("description");
    }
    serde_json::from_value(points).expect("protocol data points")
}

fn assert_refused(reply: &Reply, status: u16, code: &str, key: Option<&str>) {
    assert_eq!(reply.status, status, "{}", reply.body);
    let body = reply.json();
    assert_eq!(body["code"], code, "{}", reply.body);
    if let Some(key) = key {
        assert_eq!(body["message_key"], key, "{}", reply.body);
    }
    assert_eq!(reply.header("cache-control"), Some("no-store"));
}

/// The (field, key) pairs of a 422.
fn field_errors(reply: &Reply) -> Vec<(String, String)> {
    assert_refused(reply, 422, "validation_failed", None);
    reply.json()["field_errors"]
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

async fn start(product: &ProductPostgres, issuer: &TestIssuer) -> Service {
    Service::start(product, &issuer.jwks()).await
}

#[tokio::test(flavor = "multi_thread")]
async fn models_are_written_with_their_events_and_round_trip() {
    let Some(product) = product_postgres(&["identity_app", "assets_app", "assets_platform"]).await
    else {
        return;
    };
    let issuer = TestIssuer::new();
    let assets = start(&product, &issuer).await;
    let mut admin = product.superuser().await;
    let tenant = known_tenant(&mut admin).await;
    let editor = issuer.tenant_token(ServiceName::Assets, tenant, TenantRole::Editor);
    let viewer = issuer.tenant_token(ServiceName::Assets, tenant, TenantRole::Viewer);

    // 1. Create: 201 with the model and its URL, one created event in the tenant.
    let created = assets
        .json(
            Method::POST,
            MODELS,
            Some(&editor),
            &json!({"name": "power-meter", "points": power_meter_points()}),
        )
        .await;
    assert_eq!(created.status, 201, "{}", created.body);
    assert_eq!(created.header("cache-control"), Some("no-store"));
    let model = created.json();
    let model_id = model["model_id"].as_str().expect("an ID").to_owned();
    let model_path = format!("{MODELS}/{model_id}");
    assert_eq!(created.header("location"), Some(model_path.as_str()));
    assert_eq!(model["name"], "power-meter");
    assert_eq!(model["revision"], 1);
    assert_eq!(model["points"], as_answered(&power_meter_points()));

    let stored = events(&mut admin, tenant).await;
    assert_eq!(stored.len(), 1, "{stored:?}");
    let event = &stored[0];
    assert_eq!(event.event_type, "assets.device_model_created");
    assert_eq!(event.aggregate_type, DEVICE_MODEL_AGGREGATE);
    assert_eq!(event.aggregate_id, model_id);
    assert_eq!(event.aggregate_seq, 1);
    assert_eq!(event.tenant_id, Some(tenant.to_string()));
    assert_eq!(event.envelope.producer, ServiceName::Assets);
    let payload: DeviceModelCreated = event.envelope.decode().expect("decode the event");
    assert_eq!(payload.model_id.to_string(), model_id);
    assert_eq!(payload.revision, 1);
    assert_eq!(payload.model.name.as_str(), "power-meter");
    assert_eq!(
        payload.model.points,
        as_gateway_reads(&power_meter_points())
    );
    payload.model.validate().expect("the gateway accepts it");
    assert!(
        !event.text.contains("Phase A") && !event.text.contains("description"),
        "descriptions stay out of the events: {}",
        event.text
    );

    // 2. Read: the model as created, for a viewer too; the list counts its points.
    let read = assets
        .call(Method::GET, &model_path, Some(&viewer), None)
        .await;
    assert_eq!(read.status, 200, "{}", read.body);
    assert_eq!(read.json(), model);
    let list = assets.call(Method::GET, MODELS, Some(&viewer), None).await;
    assert_eq!(list.status, 200, "{}", list.body);
    assert_eq!(
        list.json()["models"],
        json!([{
            "model_id": model_id, "name": "power-meter", "revision": 1, "point_count": 4,
            "created_at": model["created_at"], "updated_at": model["updated_at"],
        }])
    );

    // 3. A second model of the same name, renaming, and a stale revision are refused and write
    //    nothing (acceptance 4).
    let before = footprint(&mut admin, tenant).await;
    let duplicate = assets
        .json(
            Method::POST,
            MODELS,
            Some(&editor),
            &json!({"name": "power-meter", "points": []}),
        )
        .await;
    assert_refused(&duplicate, 409, "conflict", Some("device_model.name_taken"));
    let renamed = assets
        .json(
            Method::PUT,
            &model_path,
            Some(&editor),
            &json!({"name": "energy-meter", "points": power_meter_points()}),
        )
        .await;
    assert_eq!(
        field_errors(&renamed),
        [("/name".to_owned(), "device_model.name_immutable".to_owned())]
    );
    let stale = assets
        .json(
            Method::PUT,
            &model_path,
            Some(&editor),
            &json!({"revision": 5, "points": []}),
        )
        .await;
    assert_refused(
        &stale,
        409,
        "conflict",
        Some("device_model.revision_mismatch"),
    );
    assert_eq!(stale.json()["details"]["current"], 1);
    assert_eq!(footprint(&mut admin, tenant).await, before);

    // 4. Update: every data type and byte order; the next revision and an updated event.
    let updated = assets
        .json(
            Method::PUT,
            &model_path,
            Some(&editor),
            &json!({"name": "power-meter", "revision": 1, "points": every_type_points()}),
        )
        .await;
    assert_eq!(updated.status, 200, "{}", updated.body);
    let updated = updated.json();
    assert_eq!(updated["revision"], 2);
    assert_eq!(updated["name"], "power-meter");
    assert_eq!(updated["created_at"], model["created_at"]);
    assert_eq!(updated["points"], as_answered(&every_type_points()));
    let read = assets
        .call(Method::GET, &model_path, Some(&viewer), None)
        .await;
    assert_eq!(read.json(), updated);
    let stored = events(&mut admin, tenant).await;
    assert_eq!(stored.len(), 2);
    assert_eq!(stored[1].event_type, "assets.device_model_updated");
    assert_eq!(stored[1].aggregate_id, model_id);
    assert_eq!(stored[1].aggregate_seq, 2);
    let payload: DeviceModelUpdated = stored[1].envelope.decode().expect("decode the event");
    assert_eq!(payload.revision, 2);
    assert_eq!(payload.model.name.as_str(), "power-meter");
    assert_eq!(payload.model.points, as_gateway_reads(&every_type_points()));
    assert_eq!(count(&mut admin, POINT_ROWS, tenant).await, 11);

    // 5. Export, then import under another name: the same points, a new model and event.
    let export = assets
        .call(
            Method::GET,
            &format!("{model_path}/export"),
            Some(&viewer),
            None,
        )
        .await;
    assert_eq!(export.status, 200, "{}", export.body);
    assert_eq!(
        export.header("content-disposition"),
        Some("attachment; filename=\"power-meter.device-model.json\"")
    );
    let mut document = export.json();
    assert_eq!(
        document,
        json!({"format": "trustiot.device_model", "format_version": 1, "name": "power-meter",
               "points": updated["points"]})
    );
    let again = assets
        .json(Method::POST, IMPORT, Some(&editor), &document)
        .await;
    assert_refused(&again, 409, "conflict", Some("device_model.name_taken"));
    document["name"] = json!("power-meter-copy");
    let imported = assets
        .json(Method::POST, IMPORT, Some(&editor), &document)
        .await;
    assert_eq!(imported.status, 201, "{}", imported.body);
    let copy = imported.json();
    assert_eq!(copy["points"], updated["points"]);
    assert_eq!(copy["revision"], 1);
    assert_ne!(copy["model_id"], model["model_id"]);
    let copy_id = copy["model_id"].as_str().expect("an ID").to_owned();
    let stored = events(&mut admin, tenant).await;
    assert_eq!(stored.len(), 3);
    assert_eq!(stored[2].event_type, "assets.device_model_created");
    assert_eq!(stored[2].aggregate_id, copy_id);

    // 6. Delete: 204, the points go with the model, a deleted event; then 404.
    let copy_path = format!("{MODELS}/{copy_id}");
    let deleted = assets
        .call(Method::DELETE, &copy_path, Some(&editor), None)
        .await;
    assert_eq!(deleted.status, 204, "{}", deleted.body);
    assert_eq!(deleted.header("cache-control"), Some("no-store"));
    let stored = events(&mut admin, tenant).await;
    assert_eq!(stored.len(), 4);
    assert_eq!(stored[3].event_type, "assets.device_model_deleted");
    assert_eq!(stored[3].aggregate_seq, 2);
    let payload: DeviceModelDeleted = stored[3].envelope.decode().expect("decode the event");
    assert_eq!(payload.name.as_str(), "power-meter-copy");
    assert_eq!(payload.revision, 1);
    assert_eq!(count(&mut admin, MODEL_ROWS, tenant).await, 1);
    assert_eq!(count(&mut admin, POINT_ROWS, tenant).await, 11);
    for method in [Method::GET, Method::DELETE] {
        let gone = assets.call(method, &copy_path, Some(&editor), None).await;
        assert_refused(&gone, 404, "not_found", None);
    }
    let gone = assets
        .json(
            Method::PUT,
            &copy_path,
            Some(&editor),
            &json!({"points": []}),
        )
        .await;
    assert_refused(&gone, 404, "not_found", None);

    // 7. An ID that is not a UUID is not found either.
    for path in [
        format!("{MODELS}/power-meter"),
        format!("{MODELS}/power-meter/export"),
    ] {
        let reply = assets.call(Method::GET, &path, Some(&viewer), None).await;
        assert_refused(&reply, 404, "not_found", None);
    }
    assert_eq!(events(&mut admin, tenant).await.len(), 4);
}

/// One request body per rule: the points, the field error expected, and its key. The same points
/// go through create, import and update.
fn rule_cases() -> Vec<(Value, &'static str, &'static str)> {
    let point = |changes: Value| {
        let mut point = json!({"key": "p", "function_code": 3, "address": 0, "data_type": "u16"});
        for (field, value) in changes.as_object().expect("an object") {
            if value.is_null() {
                point.as_object_mut().expect("an object").remove(field);
            } else {
                point[field] = value.clone();
            }
        }
        point
    };
    vec![
        (
            json!([point(json!({"key": null}))]),
            "/points/0/key",
            "device_model.field_required",
        ),
        (
            json!([point(json!({"key": "a b"}))]),
            "/points/0/key",
            "device_model.point_key_invalid",
        ),
        (
            json!([point(json!({"function_code": 5}))]),
            "/points/0/function_code",
            "device_model.function_code_invalid",
        ),
        (
            json!([point(json!({"function_code": 6}))]),
            "/points/0/function_code",
            "device_model.function_code_invalid",
        ),
        (
            json!([point(json!({"address": 65_536}))]),
            "/points/0/address",
            "device_model.address_invalid",
        ),
        (
            json!([point(json!({"data_type": "float"}))]),
            "/points/0/data_type",
            "device_model.data_type_invalid",
        ),
        (
            json!([point(json!({"byte_order": "little"}))]),
            "/points/0/byte_order",
            "device_model.byte_order_invalid",
        ),
        (
            json!([point(json!({"data_type": "string", "length": 126}))]),
            "/points/0/length",
            "device_model.length_invalid",
        ),
        (
            json!([point(json!({"data_type": "string"}))]),
            "/points/0/length",
            "device_model.length_required",
        ),
        (
            json!([point(json!({"length": 4}))]),
            "/points/0/length",
            "device_model.length_unexpected",
        ),
        (
            json!([point(json!({"scale": 0}))]),
            "/points/0/scale",
            "device_model.scaling_invalid",
        ),
        (
            json!([point(json!({"offset": "1"}))]),
            "/points/0/offset",
            "device_model.scaling_invalid",
        ),
        (
            json!([point(
                json!({"data_type": "string", "length": 2, "scale": 2})
            )]),
            "/points/0/scale",
            "device_model.scaling_invalid",
        ),
        (
            json!([point(json!({"function_code": 1}))]),
            "/points/0/data_type",
            "device_model.function_type_mismatch",
        ),
        (
            json!([point(
                json!({"function_code": 1, "data_type": "bool", "byte_order": "dcba"})
            )]),
            "/points/0/byte_order",
            "device_model.byte_order_unexpected",
        ),
        (
            json!([point(json!({"address": 65_534, "data_type": "f64"}))]),
            "/points/0/address",
            "device_model.address_out_of_range",
        ),
        (
            json!([
                point(json!({"data_type": "u32"})),
                point(json!({"key": "q", "address": 1}))
            ]),
            "/points/1/address",
            "device_model.points_overlap",
        ),
        (
            json!([point(json!({})), point(json!({"address": 9}))]),
            "/points/1/key",
            "device_model.point_key_duplicate",
        ),
        (
            json!([point(json!({"description": "\u{202E}reversed"}))]),
            "/points/0/description",
            "device_model.description_invalid",
        ),
        (
            json!([point(json!({"unit": "V"}))]),
            "/points/0/unit",
            "device_model.unknown_field",
        ),
        (json!({"p": 1}), "/points", "device_model.points_invalid"),
    ]
}

#[tokio::test(flavor = "multi_thread")]
async fn every_write_entry_refuses_every_rule_and_writes_nothing() {
    let Some(product) = product_postgres(&["identity_app", "assets_app", "assets_platform"]).await
    else {
        return;
    };
    let issuer = TestIssuer::new();
    let assets = start(&product, &issuer).await;
    let mut admin = product.superuser().await;
    let tenant = known_tenant(&mut admin).await;
    let editor = issuer.tenant_token(ServiceName::Assets, tenant, TenantRole::Editor);
    let existing = assets
        .json(
            Method::POST,
            MODELS,
            Some(&editor),
            &json!({"name": "existing", "points": power_meter_points()}),
        )
        .await;
    assert_eq!(existing.status, 201, "{}", existing.body);
    let model_path = format!(
        "{MODELS}/{}",
        existing.json()["model_id"].as_str().expect("ID")
    );
    let before = footprint(&mut admin, tenant).await;

    // The data point rules, through each of the three entries.
    for (points, field, key) in rule_cases() {
        let expected = [(field.to_owned(), key.to_owned())];
        let create = assets
            .json(
                Method::POST,
                MODELS,
                Some(&editor),
                &json!({"name": "new-model", "points": points}),
            )
            .await;
        assert_eq!(field_errors(&create), expected, "create: {points}");
        let import = assets
            .json(
                Method::POST,
                IMPORT,
                Some(&editor),
                &json!({"format": "trustiot.device_model", "format_version": 1,
                        "name": "new-model", "points": points}),
            )
            .await;
        assert_eq!(field_errors(&import), expected, "import: {points}");
        let update = assets
            .json(
                Method::PUT,
                &model_path,
                Some(&editor),
                &json!({"points": points}),
            )
            .await;
        assert_eq!(field_errors(&update), expected, "update: {points}");
    }

    // The naming rule, on both entries that name a model.
    for name in [
        json!("Power-Meter"),
        json!("power_meter"),
        json!("power--meter"),
        json!("-power"),
        json!("9meter"),
        json!("a".repeat(41)),
        json!(""),
        json!(7),
    ] {
        let expected = [("/name".to_owned(), "device_model.name_invalid".to_owned())];
        let create = assets
            .json(
                Method::POST,
                MODELS,
                Some(&editor),
                &json!({"name": name, "points": []}),
            )
            .await;
        assert_eq!(field_errors(&create), expected, "create: {name}");
        assert_eq!(create.json()["field_errors"][0]["details"]["max"], 40);
        let import = assets
            .json(
                Method::POST,
                IMPORT,
                Some(&editor),
                &json!({"format": "trustiot.device_model", "format_version": 1,
                        "name": name, "points": []}),
            )
            .await;
        assert_eq!(field_errors(&import), expected, "import: {name}");
    }

    // The import's own fields.
    let foreign = assets
        .json(
            Method::POST,
            IMPORT,
            Some(&editor),
            &json!({"format": "csv", "format_version": 2, "name": "new-model", "points": []}),
        )
        .await;
    assert_eq!(
        field_errors(&foreign),
        [
            (
                "/format".to_owned(),
                "device_model.format_unsupported".to_owned()
            ),
            (
                "/format_version".to_owned(),
                "device_model.format_version_unsupported".to_owned()
            ),
        ]
    );
    // A tenant can never be chosen in the body.
    let other_tenant = assets
        .json(
            Method::POST,
            MODELS,
            Some(&editor),
            &json!({"name": "new-model", "points": [], "tenant_id": TenantId::generate()}),
        )
        .await;
    assert_eq!(
        field_errors(&other_tenant),
        [(
            "/tenant_id".to_owned(),
            "device_model.unknown_field".to_owned()
        )]
    );

    // Bodies that are not JSON objects, not JSON, or too large are bad requests.
    for (path, method) in [
        (MODELS, Method::POST),
        (IMPORT, Method::POST),
        (model_path.as_str(), Method::PUT),
    ] {
        for (content_type, body) in [
            ("text/plain", b"{\"name\": \"m\", \"points\": []}".to_vec()),
            ("application/x-www-form-urlencoded", b"name=m".to_vec()),
        ] {
            let reply = assets
                .call(
                    method.clone(),
                    path,
                    Some(&editor),
                    Some((content_type, body)),
                )
                .await;
            assert_refused(&reply, 400, "invalid_request", None);
        }
        let reply = assets.call(method.clone(), path, Some(&editor), None).await;
        assert_refused(&reply, 400, "invalid_request", None);
        for body in [&b"[]"[..], b"not json", b"{\"name\": "] {
            let reply = assets
                .call(
                    method.clone(),
                    path,
                    Some(&editor),
                    Some(("application/json", body.to_vec())),
                )
                .await;
            assert_refused(
                &reply,
                400,
                "invalid_request",
                Some("device_model.body_invalid"),
            );
        }
        let large = vec![b' '; 2 * 1024 * 1024 + 1];
        let reply = assets
            .call(
                method.clone(),
                path,
                Some(&editor),
                Some(("application/json", large)),
            )
            .await;
        assert_refused(
            &reply,
            400,
            "invalid_request",
            Some("device_model.body_too_large"),
        );
        assert_eq!(reply.json()["details"]["max_bytes"], 2 * 1024 * 1024);
    }

    assert_eq!(
        footprint(&mut admin, tenant).await,
        before,
        "a refused write changed the tables or wrote an event"
    );
    let read = assets
        .call(Method::GET, &model_path, Some(&editor), None)
        .await;
    assert_eq!(read.json()["points"], as_answered(&power_meter_points()));
    assert_eq!(read.json()["revision"], 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_model_a_device_uses_cannot_be_deleted() {
    let Some(product) = product_postgres(&["identity_app", "assets_app", "assets_platform"]).await
    else {
        return;
    };
    let issuer = TestIssuer::new();
    let assets = start(&product, &issuer).await;
    let mut admin = product.superuser().await;
    sqlx::query(CREATE_DEVICES)
        .execute(&mut admin)
        .await
        .expect("create the stand-in device table");
    let tenant = known_tenant(&mut admin).await;
    let editor = issuer.tenant_token(ServiceName::Assets, tenant, TenantRole::Editor);
    let created = assets
        .json(
            Method::POST,
            MODELS,
            Some(&editor),
            &json!({"name": "in-use", "points": power_meter_points()}),
        )
        .await;
    assert_eq!(created.status, 201, "{}", created.body);
    let model_id = created.json()["model_id"].as_str().expect("ID").to_owned();
    let model_path = format!("{MODELS}/{model_id}");
    sqlx::query(INSERT_DEVICE)
        .bind(tenant.to_string())
        .bind(&model_id)
        .execute(&mut admin)
        .await
        .expect("a device of the model");
    let before = footprint(&mut admin, tenant).await;

    let refused = assets
        .call(Method::DELETE, &model_path, Some(&editor), None)
        .await;
    assert_refused(&refused, 409, "conflict", Some("device_model.in_use"));
    assert_eq!(
        footprint(&mut admin, tenant).await,
        before,
        "the refused delete changed the model or wrote an event"
    );
    let read = assets
        .call(Method::GET, &model_path, Some(&editor), None)
        .await;
    assert_eq!(read.status, 200);

    // Once no device uses it, it can be deleted.
    sqlx::query(DELETE_DEVICES)
        .bind(tenant.to_string())
        .execute(&mut admin)
        .await
        .expect("remove the device");
    let deleted = assets
        .call(Method::DELETE, &model_path, Some(&editor), None)
        .await;
    assert_eq!(deleted.status, 204, "{}", deleted.body);
    let stored = events(&mut admin, tenant).await;
    assert_eq!(
        stored
            .iter()
            .map(|event| event.event_type.as_str())
            .collect::<Vec<_>>(),
        ["assets.device_model_created", "assets.device_model_deleted"]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn roles_decide_who_reads_and_who_writes() {
    let Some(product) = product_postgres(&["identity_app", "assets_app", "assets_platform"]).await
    else {
        return;
    };
    let issuer = TestIssuer::new();
    let assets = start(&product, &issuer).await;
    let mut admin = product.superuser().await;
    let tenant = known_tenant(&mut admin).await;
    let tenant_admin = issuer.tenant_token(ServiceName::Assets, tenant, TenantRole::Admin);
    let viewer = issuer.tenant_token(ServiceName::Assets, tenant, TenantRole::Viewer);
    let no_tenant = issuer.token(&TestIssuer::claims(ServiceName::Assets, None));
    let elsewhere = issuer.tenant_token(ServiceName::Identity, tenant, TenantRole::Admin);

    // An admin includes an editor's rights.
    let created = assets
        .json(
            Method::POST,
            MODELS,
            Some(&tenant_admin),
            &json!({"name": "by-admin", "points": []}),
        )
        .await;
    assert_eq!(created.status, 201, "{}", created.body);
    let model_path = format!(
        "{MODELS}/{}",
        created.json()["model_id"].as_str().expect("ID")
    );
    let export_path = format!("{model_path}/export");
    let before = footprint(&mut admin, tenant).await;

    let document = json!({"format": "trustiot.device_model", "format_version": 1,
                          "name": "by-viewer", "points": []});
    let writes = [
        (
            Method::POST,
            MODELS.to_owned(),
            json!({"name": "by-viewer", "points": []}),
        ),
        (Method::POST, IMPORT.to_owned(), document),
        (Method::PUT, model_path.clone(), json!({"points": []})),
    ];
    for (method, path, body) in &writes {
        for token in [&viewer, &no_tenant] {
            let reply = assets.json(method.clone(), path, Some(token), body).await;
            assert_refused(&reply, 403, "permission_denied", None);
        }
        let reply = assets.json(method.clone(), path, None, body).await;
        assert_refused(&reply, 401, "unauthenticated", None);
        let reply = assets
            .json(method.clone(), path, Some(&elsewhere), body)
            .await;
        assert_refused(&reply, 401, "unauthenticated", None);
    }
    for token in [Some(viewer.as_str()), Some(no_tenant.as_str()), None] {
        let reply = assets.call(Method::DELETE, &model_path, token, None).await;
        let (status, code) = if token.is_none() {
            (401, "unauthenticated")
        } else {
            (403, "permission_denied")
        };
        assert_refused(&reply, status, code, None);
    }
    assert_eq!(footprint(&mut admin, tenant).await, before);

    // A viewer reads; a user without a tenant does not.
    for path in [MODELS, model_path.as_str(), export_path.as_str()] {
        let reply = assets.call(Method::GET, path, Some(&viewer), None).await;
        assert_eq!(reply.status, 200, "{path}: {}", reply.body);
        let reply = assets.call(Method::GET, path, Some(&no_tenant), None).await;
        assert_refused(&reply, 403, "permission_denied", None);
        let reply = assets.call(Method::GET, path, None, None).await;
        assert_refused(&reply, 401, "unauthenticated", None);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_tenant_the_service_does_not_know_yet_cannot_create_models() {
    let Some(product) = product_postgres(&["identity_app", "assets_app", "assets_platform"]).await
    else {
        return;
    };
    let issuer = TestIssuer::new();
    let assets = start(&product, &issuer).await;
    let mut admin = product.superuser().await;
    // No row in assets.tenants: its identity.tenant_created has not been processed.
    let tenant = TenantId::generate();
    let editor = issuer.tenant_token(ServiceName::Assets, tenant, TenantRole::Editor);
    let reply = assets
        .json(
            Method::POST,
            MODELS,
            Some(&editor),
            &json!({"name": "early", "points": power_meter_points()}),
        )
        .await;
    assert_refused(
        &reply,
        503,
        "unavailable",
        Some("device_model.tenant_not_ready"),
    );
    assert_eq!(footprint(&mut admin, tenant).await, (0, 0, 0));
}
