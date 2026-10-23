//! The device API (Laysi/trustiot-v2#100) against the real binary on the product's schema,
//! applied by the migration Job (`testkit::product`):
//!
//! - invalid connection settings and poll intervals are refused on create and update, with a
//!   422 naming the field, and nothing is written, not even an event (acceptance 1); a device
//!   that leaves the unit ID and poll interval out gets the unified defaults;
//! - create, update and delete each write their `assets.device_*` event in the device's tenant,
//!   in order, with the device in the gateway's own format (acceptance 2);
//! - names that break the naming rule, duplicate names, changing the name, gateway or model, and
//!   a gateway not bound to the caller's tenant (another tenant's, unbound, not in the pool) are
//!   refused (acceptance 4); so are another tenant's model, a stale revision, and the rules of
//!   one converter (one transport, one device per unit ID), also when two requests race;
//! - a model a device uses cannot be deleted until the device is; roles decide who reads and
//!   who writes.
//!
//! The cross-tenant tests are `tests/cross_tenant.rs` (acceptance 3). Starts a throwaway
//! Postgres with TLS (podman or docker); skipped without either unless `CI` is set.

mod support;

use contract::events::assets::{DEVICE_AGGREGATE, DeviceCreated, DeviceDeleted, DeviceUpdated};
use contract::{EventEnvelope, ServiceName, TenantId, TenantRole, UserId};
use protocol::config::{DesiredConfig, DeviceModel, Transport};
use protocol::{ConfigVersion, Message, ModelName, PROTOCOL_REVISION, V1};
use reqwest::Method;
use serde_json::{Value, json};
use sqlx::PgConnection;
use support::{Reply, Service};
use testkit::identity::TestIssuer;
use testkit::product::{ProductPostgres, product_postgres};

const DEVICES: &str = "/api/assets/v1/devices";
const MODELS: &str = "/api/assets/v1/device-models";

const LOGINS: &[&str] = &["identity_app", "assets_app", "assets_platform"];

/// The tenant's row, as the consumer of `identity.tenant_created` writes it.
const INSERT_TENANT: &str = "INSERT INTO assets.tenants (tenant_id) VALUES ($1::uuid)";
/// A gateway in the pool, as the consumer of `certs.gateway_registered` writes it.
const INSERT_POOLED: &str =
    "INSERT INTO assets.gateway_pool (hardware_id, registered_at, workstation)
VALUES ($1, now(), 'ws-test')";
/// A current binding of a pooled gateway to a tenant, as the binding API writes it.
const INSERT_BINDING: &str =
    "INSERT INTO assets.gateway_bindings (tenant_id, binding_id, hardware_id)
VALUES ($1::uuid, gen_random_uuid(), $2)";
/// Ends a gateway's binding, as unbinding (Laysi/trustiot-v2#115) will.
const UNBIND: &str = "UPDATE assets.gateway_bindings SET unbound_at = now()
WHERE hardware_id = $1 AND unbound_at IS NULL";
const DEVICE_ROWS: &str = "SELECT count(*) FROM assets.devices WHERE tenant_id = $1::uuid";
/// The tenant's events in the order consumers receive them.
const EVENTS: &str = "SELECT event_type, aggregate_type, aggregate_id, aggregate_seq,
    tenant_id::text, envelope::text
FROM assets.outbox_event WHERE tenant_id = $1::uuid ORDER BY tx_order, id";

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

/// What the tenant's device table and outbox hold: device rows, events.
async fn footprint(admin: &mut PgConnection, tenant: TenantId) -> (i64, usize) {
    let devices: i64 = sqlx::query_scalar(DEVICE_ROWS)
        .bind(tenant.to_string())
        .fetch_one(&mut *admin)
        .await
        .expect("count the devices");
    (devices, events(admin, tenant).await.len())
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

/// A hardware ID no other test uses.
fn hardware_id(prefix: &str) -> String {
    format!(
        "{prefix}-{}",
        UserId::generate().to_string().replace('-', "")
    )
}

/// A gateway of the pool, not bound.
async fn pooled(admin: &mut PgConnection, prefix: &str) -> String {
    let hardware_id = hardware_id(prefix);
    sqlx::query(INSERT_POOLED)
        .bind(&hardware_id)
        .execute(admin)
        .await
        .expect("put the gateway in the pool");
    hardware_id
}

/// A gateway of the pool bound to `tenant`.
async fn bound(admin: &mut PgConnection, tenant: TenantId, prefix: &str) -> String {
    let hardware_id = pooled(admin, prefix).await;
    sqlx::query(INSERT_BINDING)
        .bind(tenant.to_string())
        .bind(&hardware_id)
        .execute(admin)
        .await
        .expect("bind the gateway");
    hardware_id
}

/// A device model of the tenant, created through the API; its ID.
async fn model(assets: &Service, token: &str, name: &str) -> String {
    let created = assets
        .json(
            Method::POST,
            MODELS,
            Some(token),
            &json!({"name": name, "points": [
                {"key": "voltage", "function_code": 3, "address": 0, "data_type": "u16",
                 "scale": 0.1}
            ]}),
        )
        .await;
    assert_eq!(created.status, 201, "{}", created.body);
    created.json()["model_id"]
        .as_str()
        .expect("a model ID")
        .to_owned()
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

fn one_error(field: &str, key: &str) -> Vec<(String, String)> {
    vec![(field.to_owned(), key.to_owned())]
}

/// `base` with `changes` applied; a `null` change removes the field.
fn with(base: &Value, changes: &Value) -> Value {
    let mut body = base.clone();
    for (field, value) in changes.as_object().expect("an object") {
        if value.is_null() {
            body.as_object_mut().expect("an object").remove(field);
        } else {
            body[field] = value.clone();
        }
    }
    body
}

/// The gateway's configuration accepts `device` with its model.
fn assert_the_gateway_accepts(device: &protocol::config::Device) {
    DesiredConfig {
        v: V1,
        rev: PROTOCOL_REVISION,
        version: ConfigVersion::new(1).expect("a version"),
        models: vec![DeviceModel {
            name: device.model.clone(),
            points: Vec::new(),
        }],
        devices: vec![device.clone()],
    }
    .validate()
    .expect("the gateway accepts the device");
}

async fn start(product: &ProductPostgres, issuer: &TestIssuer) -> Service {
    Service::start(product, &issuer.jwks()).await
}

#[tokio::test(flavor = "multi_thread")]
async fn devices_are_written_with_their_events() {
    let Some(product) = product_postgres(LOGINS).await else {
        return;
    };
    let issuer = TestIssuer::new();
    let assets = start(&product, &issuer).await;
    let mut admin = product.superuser().await;
    let tenant = known_tenant(&mut admin).await;
    let editor = issuer.tenant_token(ServiceName::Assets, tenant, TenantRole::Editor);
    let viewer = issuer.tenant_token(ServiceName::Assets, tenant, TenantRole::Viewer);
    let gateway = bound(&mut admin, tenant, "tg-dev").await;
    let model_id = model(&assets, &editor, "power-meter").await;
    let model_path = format!("{MODELS}/{model_id}");

    // 1. Create without a unit ID or poll interval: the unified defaults, 201 with the device
    //    and its URL, and one created event in the tenant.
    let created = assets
        .json(
            Method::POST,
            DEVICES,
            Some(&editor),
            &json!({"name": "line-1-meter", "hardware_id": gateway, "model_id": model_id,
                    "transport": "rtu_over_tcp", "host": "192.0.2.10", "port": 8899}),
        )
        .await;
    assert_eq!(created.status, 201, "{}", created.body);
    assert_eq!(created.header("cache-control"), Some("no-store"));
    let device = created.json();
    let device_id = device["device_id"].as_str().expect("an ID").to_owned();
    let device_path = format!("{DEVICES}/{device_id}");
    assert_eq!(created.header("location"), Some(device_path.as_str()));
    assert_eq!(
        device,
        json!({"device_id": device_id, "name": "line-1-meter", "hardware_id": gateway,
               "model_id": model_id, "transport": "rtu_over_tcp", "host": "192.0.2.10",
               "port": 8899, "unit_id": 1, "poll_interval_ms": 60000, "revision": 1,
               "created_at": device["created_at"], "updated_at": device["updated_at"]})
    );

    let stored = events(&mut admin, tenant).await;
    let event = stored.last().expect("an event");
    assert_eq!(event.event_type, "assets.device_created");
    assert_eq!(event.aggregate_type, DEVICE_AGGREGATE);
    assert_eq!(event.aggregate_id, device_id);
    assert_eq!(event.aggregate_seq, 1);
    assert_eq!(event.tenant_id, Some(tenant.to_string()));
    assert_eq!(event.envelope.producer, ServiceName::Assets);
    let payload: DeviceCreated = event.envelope.decode().expect("decode the event");
    assert_eq!(payload.device_id.to_string(), device_id);
    assert_eq!(payload.revision, 1);
    assert_eq!(payload.name.as_str(), "line-1-meter");
    assert_eq!(payload.hardware_id.as_str(), gateway);
    assert_eq!(payload.model_id.to_string(), model_id);
    assert_eq!(payload.device.id.as_str(), device_id);
    assert_eq!(payload.device.model.as_str(), "power-meter");
    assert_eq!(payload.device.transport, Transport::RtuOverTcp);
    assert_eq!(payload.device.host.as_str(), "192.0.2.10");
    assert_eq!(payload.device.port, 8899);
    assert_eq!(payload.device.unit_id, 1);
    assert_eq!(payload.device.poll_interval_ms, 60_000);
    assert_the_gateway_accepts(&payload.device);
    assert_eq!(event.envelope.tenant_id, Some(tenant));
    assert!(
        !event
            .envelope
            .payload
            .to_string()
            .contains(&tenant.to_string()),
        "the payload names no tenant: {}",
        event.text
    );

    // 2. Read: the device as created, for a viewer too, and in the list.
    let read = assets
        .call(Method::GET, &device_path, Some(&viewer), None)
        .await;
    assert_eq!(read.status, 200, "{}", read.body);
    assert_eq!(read.json(), device);
    let list = assets.call(Method::GET, DEVICES, Some(&viewer), None).await;
    assert_eq!(list.status, 200, "{}", list.body);
    assert_eq!(list.json(), json!({"devices": [device.clone()]}));

    // 3. The model the device uses cannot be deleted.
    let before = footprint(&mut admin, tenant).await;
    let refused = assets
        .call(Method::DELETE, &model_path, Some(&editor), None)
        .await;
    assert_refused(&refused, 409, "conflict", Some("device_model.in_use"));
    assert_eq!(footprint(&mut admin, tenant).await, before);

    // 4. Update: new settings, the unchanging fields sent as they are, the next revision and an
    //    updated event.
    let updated = assets
        .json(
            Method::PUT,
            &device_path,
            Some(&editor),
            &json!({"revision": 1, "name": "line-1-meter", "hardware_id": gateway,
                    "model_id": model_id.to_uppercase(), "transport": "modbus_tcp",
                    "host": "meter-1.plant.example", "port": 502, "unit_id": 0,
                    "poll_interval_ms": 10000}),
        )
        .await;
    assert_eq!(updated.status, 200, "{}", updated.body);
    let after = updated.json();
    assert_eq!(after["revision"], 2);
    assert_eq!(after["transport"], "modbus_tcp");
    assert_eq!(after["host"], "meter-1.plant.example");
    assert_eq!(after["port"], 502);
    assert_eq!(after["unit_id"], 0);
    assert_eq!(after["poll_interval_ms"], 10000);
    assert_eq!(after["created_at"], device["created_at"]);
    assert_eq!(after["name"], "line-1-meter");
    let stored = events(&mut admin, tenant).await;
    let event = stored.last().expect("an event");
    assert_eq!(event.event_type, "assets.device_updated");
    assert_eq!(event.aggregate_id, device_id);
    assert_eq!(event.aggregate_seq, 2);
    let payload: DeviceUpdated = event.envelope.decode().expect("decode the event");
    assert_eq!(payload.revision, 2);
    assert_eq!(payload.device.transport, Transport::ModbusTcp);
    assert_eq!(payload.device.host.as_str(), "meter-1.plant.example");
    assert_eq!(payload.device.unit_id, 0);
    assert_eq!(payload.device.poll_interval_ms, 10_000);
    assert_eq!(payload.device.model.as_str(), "power-meter");
    assert_the_gateway_accepts(&payload.device);
    let read = assets
        .call(Method::GET, &device_path, Some(&viewer), None)
        .await;
    assert_eq!(read.json(), after);

    // 5. An update that leaves the unit ID and interval out returns them to the defaults.
    let defaults = assets
        .json(
            Method::PUT,
            &device_path,
            Some(&editor),
            &json!({"revision": 2, "transport": "modbus_tcp", "host": "198.51.100.7",
                    "port": 502}),
        )
        .await;
    assert_eq!(defaults.status, 200, "{}", defaults.body);
    assert_eq!(defaults.json()["unit_id"], 1);
    assert_eq!(defaults.json()["poll_interval_ms"], 60000);
    assert_eq!(defaults.json()["revision"], 3);

    // 6. Delete: 204, a deleted event naming the device, its gateway and its model; the device
    //    is gone and its model can be deleted now.
    let deleted = assets
        .call(Method::DELETE, &device_path, Some(&editor), None)
        .await;
    assert_eq!(deleted.status, 204, "{}", deleted.body);
    assert_eq!(deleted.header("cache-control"), Some("no-store"));
    let stored = events(&mut admin, tenant).await;
    let event = stored.last().expect("an event");
    assert_eq!(event.event_type, "assets.device_deleted");
    assert_eq!(event.aggregate_id, device_id);
    assert_eq!(event.aggregate_seq, 4);
    let payload: DeviceDeleted = event.envelope.decode().expect("decode the event");
    assert_eq!(payload.device_id.to_string(), device_id);
    assert_eq!(
        payload.name,
        ModelName::new("line-1-meter").expect("a name")
    );
    assert_eq!(payload.hardware_id.as_str(), gateway);
    assert_eq!(payload.model_id.to_string(), model_id);
    assert_eq!(payload.revision, 3);
    assert_eq!(
        event.envelope.payload,
        json!({"device_id": device_id, "name": "line-1-meter", "hardware_id": gateway,
               "model_id": model_id, "revision": 3})
    );
    let gone = assets
        .call(Method::GET, &device_path, Some(&viewer), None)
        .await;
    assert_refused(&gone, 404, "not_found", None);
    let again = assets
        .call(Method::DELETE, &device_path, Some(&editor), None)
        .await;
    assert_refused(&again, 404, "not_found", None);
    let model_deleted = assets
        .call(Method::DELETE, &model_path, Some(&editor), None)
        .await;
    assert_eq!(model_deleted.status, 204, "{}", model_deleted.body);

    let types: Vec<String> = events(&mut admin, tenant)
        .await
        .into_iter()
        .map(|event| event.event_type)
        .collect();
    assert_eq!(
        types,
        [
            "assets.device_model_created",
            "assets.device_created",
            "assets.device_updated",
            "assets.device_updated",
            "assets.device_deleted",
            "assets.device_model_deleted",
        ]
    );

    // Each write logged one INFO line without the host.
    let logs = assets.stdout.text();
    for message in ["created a device", "updated a device", "deleted a device"] {
        assert!(
            !assets.stdout.messages(message).is_empty(),
            "no `{message}` line"
        );
    }
    assert!(!logs.contains("192.0.2.10") && !logs.contains("meter-1.plant.example"));
}

#[tokio::test(flavor = "multi_thread")]
async fn invalid_settings_are_refused_and_nothing_is_written() {
    let Some(product) = product_postgres(LOGINS).await else {
        return;
    };
    let issuer = TestIssuer::new();
    let assets = start(&product, &issuer).await;
    let mut admin = product.superuser().await;
    let tenant = known_tenant(&mut admin).await;
    let editor = issuer.tenant_token(ServiceName::Assets, tenant, TenantRole::Editor);
    let gateway = bound(&mut admin, tenant, "tg-inv").await;
    let model_id = model(&assets, &editor, "meter").await;
    let create = json!({"name": "meter-1", "hardware_id": gateway, "model_id": model_id,
                        "transport": "modbus_tcp", "host": "192.0.2.20", "port": 502});
    let created = assets
        .json(Method::POST, DEVICES, Some(&editor), &create)
        .await;
    assert_eq!(created.status, 201, "{}", created.body);
    let device_path = format!(
        "{DEVICES}/{}",
        created.json()["device_id"].as_str().expect("an ID")
    );
    let update = json!({"revision": 1, "transport": "modbus_tcp", "host": "192.0.2.20",
                        "port": 502});
    let before = footprint(&mut admin, tenant).await;

    let cases = [
        (
            json!({"transport": null}),
            "/transport",
            "device.field_required",
        ),
        (
            json!({"transport": "mock"}),
            "/transport",
            "device.transport_invalid",
        ),
        (
            json!({"transport": "modbus-rtu-tcp"}),
            "/transport",
            "device.transport_invalid",
        ),
        (json!({"host": null}), "/host", "device.field_required"),
        (
            json!({"host": "192.168.1.300"}),
            "/host",
            "device.host_invalid",
        ),
        (json!({"host": "127.0.0.1"}), "/host", "device.host_invalid"),
        (json!({"host": "::1"}), "/host", "device.host_invalid"),
        (json!({"host": "localhost"}), "/host", "device.host_invalid"),
        (json!({"host": "0.0.0.0"}), "/host", "device.host_invalid"),
        (
            json!({"host": "192.0.2.1:502"}),
            "/host",
            "device.host_invalid",
        ),
        (json!({"port": null}), "/port", "device.field_required"),
        (json!({"port": 0}), "/port", "device.port_invalid"),
        (json!({"port": 65_536}), "/port", "device.port_invalid"),
        (json!({"port": "502"}), "/port", "device.port_invalid"),
        (
            json!({"unit_id": 256}),
            "/unit_id",
            "device.unit_id_invalid",
        ),
        (
            json!({"transport": "rtu_over_tcp", "unit_id": 0}),
            "/unit_id",
            "device.unit_id_invalid",
        ),
        (
            json!({"transport": "rtu_over_tcp", "unit_id": 248}),
            "/unit_id",
            "device.unit_id_invalid",
        ),
        (
            json!({"poll_interval_ms": 999}),
            "/poll_interval_ms",
            "device.poll_interval_invalid",
        ),
        (
            json!({"poll_interval_ms": 100}),
            "/poll_interval_ms",
            "device.poll_interval_invalid",
        ),
        (
            json!({"poll_interval_ms": 86_400_001}),
            "/poll_interval_ms",
            "device.poll_interval_invalid",
        ),
        (
            json!({"poll_interval_ms": 1500.5}),
            "/poll_interval_ms",
            "device.poll_interval_invalid",
        ),
        (json!({"mode": "server"}), "/mode", "device.unknown_field"),
        (
            json!({"tenant_id": tenant.to_string()}),
            "/tenant_id",
            "device.unknown_field",
        ),
    ];
    for (changes, field, key) in cases {
        let reply = assets
            .json(
                Method::POST,
                DEVICES,
                Some(&editor),
                &with(&create, &changes),
            )
            .await;
        assert_eq!(
            field_errors(&reply),
            one_error(field, key),
            "create {changes}"
        );
        let reply = assets
            .json(
                Method::PUT,
                &device_path,
                Some(&editor),
                &with(&update, &changes),
            )
            .await;
        assert_eq!(
            field_errors(&reply),
            one_error(field, key),
            "update {changes}"
        );
    }
    let error = assets
        .json(
            Method::POST,
            DEVICES,
            Some(&editor),
            &with(&create, &json!({"poll_interval_ms": 999})),
        )
        .await
        .json();
    assert_eq!(
        error["field_errors"][0]["details"],
        json!({"min": 1000, "max": 86_400_000})
    );
    // Bodies that are not JSON objects, or not JSON at all, are bad requests.
    for (content_type, body) in [
        ("application/json", b"[]".to_vec()),
        ("application/json", b"{\"name\":".to_vec()),
        ("text/plain", serde_json::to_vec(&create).expect("JSON")),
        ("application/json", vec![b' '; 16 * 1024 + 1]),
    ] {
        for (method, path) in [(Method::POST, DEVICES), (Method::PUT, device_path.as_str())] {
            let reply = assets
                .call(
                    method.clone(),
                    path,
                    Some(&editor),
                    Some((content_type, body.clone())),
                )
                .await;
            assert_refused(&reply, 400, "invalid_request", None);
        }
    }
    assert_eq!(
        footprint(&mut admin, tenant).await,
        before,
        "a refused write changed a device or wrote an event"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn names_gateways_and_models_are_checked() {
    let Some(product) = product_postgres(LOGINS).await else {
        return;
    };
    let issuer = TestIssuer::new();
    let assets = start(&product, &issuer).await;
    let mut admin = product.superuser().await;
    let tenant = known_tenant(&mut admin).await;
    let other = known_tenant(&mut admin).await;
    let editor = issuer.tenant_token(ServiceName::Assets, tenant, TenantRole::Editor);
    let other_editor = issuer.tenant_token(ServiceName::Assets, other, TenantRole::Editor);
    let gateway = bound(&mut admin, tenant, "tg-own").await;
    let second_gateway = bound(&mut admin, tenant, "tg-own2").await;
    let others_gateway = bound(&mut admin, other, "tg-other").await;
    let unbound_gateway = pooled(&mut admin, "tg-free").await;
    let ended_gateway = bound(&mut admin, tenant, "tg-ended").await;
    sqlx::query(UNBIND)
        .bind(&ended_gateway)
        .execute(&mut admin)
        .await
        .expect("end the binding");
    let model_id = model(&assets, &editor, "meter").await;
    let second_model = model(&assets, &editor, "other-meter").await;
    let others_model = model(&assets, &other_editor, "meter").await;
    let create = json!({"name": "meter-1", "hardware_id": gateway, "model_id": model_id,
                        "transport": "modbus_tcp", "host": "192.0.2.30", "port": 502});
    let created = assets
        .json(Method::POST, DEVICES, Some(&editor), &create)
        .await;
    assert_eq!(created.status, 201, "{}", created.body);
    let device_path = format!(
        "{DEVICES}/{}",
        created.json()["device_id"].as_str().expect("an ID")
    );
    let before = (
        footprint(&mut admin, tenant).await,
        footprint(&mut admin, other).await,
    );

    // The naming rule of models.
    for name in [
        json!("Meter-1"),
        json!("meter_1"),
        json!("1-meter"),
        json!("meter--1"),
        json!("電表"),
        json!("m".repeat(41)),
        json!(7),
    ] {
        let reply = assets
            .json(
                Method::POST,
                DEVICES,
                Some(&editor),
                &with(&create, &json!({"name": name, "host": "192.0.2.31"})),
            )
            .await;
        assert_eq!(
            field_errors(&reply),
            one_error("/name", "device.name_invalid"),
            "{name}"
        );
    }
    // A name the tenant has: 409, even on another gateway and model.
    let duplicate = assets
        .json(
            Method::POST,
            DEVICES,
            Some(&editor),
            &with(
                &create,
                &json!({"hardware_id": second_gateway, "model_id": second_model}),
            ),
        )
        .await;
    assert_refused(&duplicate, 409, "conflict", Some("device.name_taken"));
    // Gateways not bound to the tenant: another tenant's, one in the pool bound to no tenant,
    // one whose binding ended, one not in the pool; all answer the same.
    for hardware_id in [
        others_gateway.as_str(),
        unbound_gateway.as_str(),
        ended_gateway.as_str(),
        "tg-never-registered",
    ] {
        let reply = assets
            .json(
                Method::POST,
                DEVICES,
                Some(&editor),
                &with(
                    &create,
                    &json!({"name": "meter-2", "hardware_id": hardware_id}),
                ),
            )
            .await;
        assert_eq!(
            field_errors(&reply),
            one_error("/hardware_id", "device.gateway_not_bound"),
            "{hardware_id}"
        );
        assert_eq!(reply.json().get("details"), None, "{}", reply.body);
    }
    // A model the tenant does not have: another tenant's, or none.
    for model in [others_model.clone(), TenantId::generate().to_string()] {
        let reply = assets
            .json(
                Method::POST,
                DEVICES,
                Some(&editor),
                &with(&create, &json!({"name": "meter-2", "model_id": model})),
            )
            .await;
        assert_eq!(
            field_errors(&reply),
            one_error("/model_id", "device.model_not_found")
        );
    }
    // The name, gateway and model never change.
    let update = json!({"revision": 1, "transport": "modbus_tcp", "host": "192.0.2.30",
                        "port": 502});
    for (changes, expected) in [
        (
            json!({"name": "meter-9"}),
            one_error("/name", "device.name_immutable"),
        ),
        (
            json!({"hardware_id": second_gateway}),
            one_error("/hardware_id", "device.gateway_immutable"),
        ),
        (
            json!({"model_id": second_model}),
            one_error("/model_id", "device.model_immutable"),
        ),
        (
            json!({"name": 5, "hardware_id": others_gateway, "model_id": "meter"}),
            vec![
                ("/name".to_owned(), "device.name_immutable".to_owned()),
                (
                    "/hardware_id".to_owned(),
                    "device.gateway_immutable".to_owned(),
                ),
                ("/model_id".to_owned(), "device.model_immutable".to_owned()),
            ],
        ),
    ] {
        let reply = assets
            .json(
                Method::PUT,
                &device_path,
                Some(&editor),
                &with(&update, &changes),
            )
            .await;
        assert_eq!(field_errors(&reply), expected, "{changes}");
    }
    // A stale revision.
    let stale = assets
        .json(
            Method::PUT,
            &device_path,
            Some(&editor),
            &with(&update, &json!({"revision": 4})),
        )
        .await;
    assert_refused(&stale, 409, "conflict", Some("device.revision_mismatch"));
    assert_eq!(stale.json()["details"]["current"], 1);
    // Another tenant cannot place a device on this tenant's gateway or with its model.
    let reply = assets
        .json(
            Method::POST,
            DEVICES,
            Some(&other_editor),
            &with(
                &create,
                &json!({"name": "intruder", "model_id": others_model}),
            ),
        )
        .await;
    assert_eq!(
        field_errors(&reply),
        one_error("/hardware_id", "device.gateway_not_bound")
    );
    let reply = assets
        .json(
            Method::POST,
            DEVICES,
            Some(&other_editor),
            &with(
                &create,
                &json!({"name": "intruder", "hardware_id": others_gateway}),
            ),
        )
        .await;
    assert_eq!(
        field_errors(&reply),
        one_error("/model_id", "device.model_not_found")
    );
    assert_eq!(
        (
            footprint(&mut admin, tenant).await,
            footprint(&mut admin, other).await,
        ),
        before,
        "a refused write changed a device or wrote an event"
    );
    // The same name in another tenant is fine.
    let reply = assets
        .json(
            Method::POST,
            DEVICES,
            Some(&other_editor),
            &with(
                &create,
                &json!({"hardware_id": others_gateway, "model_id": others_model}),
            ),
        )
        .await;
    assert_eq!(reply.status, 201, "{}", reply.body);
}

#[tokio::test(flavor = "multi_thread")]
async fn devices_behind_one_converter_share_its_transport_and_not_a_unit_id() {
    let Some(product) = product_postgres(LOGINS).await else {
        return;
    };
    let issuer = TestIssuer::new();
    let assets = start(&product, &issuer).await;
    let mut admin = product.superuser().await;
    let tenant = known_tenant(&mut admin).await;
    let editor = issuer.tenant_token(ServiceName::Assets, tenant, TenantRole::Editor);
    let gateway = bound(&mut admin, tenant, "tg-conv").await;
    let second_gateway = bound(&mut admin, tenant, "tg-conv2").await;
    let model_id = model(&assets, &editor, "meter").await;
    let rtu = |name: &str, unit_id: u8| {
        json!({"name": name, "hardware_id": gateway, "model_id": model_id,
               "transport": "rtu_over_tcp", "host": "192.0.2.40", "port": 4196,
               "unit_id": unit_id})
    };
    let create = async |body: Value| {
        assets
            .json(Method::POST, DEVICES, Some(&editor), &body)
            .await
    };

    // Two devices behind one converter, different unit IDs.
    let first = create(rtu("meter-1", 1)).await;
    assert_eq!(first.status, 201, "{}", first.body);
    let first_id = first.json()["device_id"].as_str().expect("ID").to_owned();
    let second = create(rtu("meter-2", 2)).await;
    assert_eq!(second.status, 201, "{}", second.body);
    let second_path = format!(
        "{DEVICES}/{}",
        second.json()["device_id"].as_str().expect("ID")
    );
    let before = footprint(&mut admin, tenant).await;

    // The same unit ID on the converter, also spelled as an IPv4-mapped address: 409 naming
    // the device that has it.
    for host in ["192.0.2.40", "::ffff:192.0.2.40"] {
        let mut body = rtu("meter-3", 1);
        body["host"] = json!(host);
        let reply = create(body).await;
        assert_refused(&reply, 409, "conflict", Some("device.unit_taken"));
        assert_eq!(
            reply.json()["details"],
            json!({"device_id": first_id}),
            "{host}"
        );
    }
    // Modbus TCP at the converter's host and port: 409 naming a device there and its transport.
    let mut tcp = rtu("meter-3", 5);
    tcp["transport"] = json!("modbus_tcp");
    let reply = create(tcp.clone()).await;
    assert_refused(&reply, 409, "conflict", Some("device.transport_mismatch"));
    assert_eq!(
        reply.json()["details"],
        json!({"device_id": first_id, "transport": "rtu_over_tcp"})
    );
    // An update cannot move a device onto a taken unit ID or switch its converter's transport
    // either.
    let base_update = json!({"revision": 1, "transport": "rtu_over_tcp", "host": "192.0.2.40",
                             "port": 4196, "unit_id": 2});
    let reply = assets
        .json(
            Method::PUT,
            &second_path,
            Some(&editor),
            &with(&base_update, &json!({"unit_id": 1})),
        )
        .await;
    assert_refused(&reply, 409, "conflict", Some("device.unit_taken"));
    let reply = assets
        .json(
            Method::PUT,
            &second_path,
            Some(&editor),
            &with(&base_update, &json!({"transport": "modbus_tcp"})),
        )
        .await;
    assert_refused(&reply, 409, "conflict", Some("device.transport_mismatch"));
    assert_eq!(footprint(&mut admin, tenant).await, before);
    // Updating a device to its own settings is not a conflict with itself.
    let reply = assets
        .json(Method::PUT, &second_path, Some(&editor), &base_update)
        .await;
    assert_eq!(reply.status, 200, "{}", reply.body);

    // Another port, or the same endpoint on another gateway, is another converter.
    let mut other_port = tcp.clone();
    other_port["port"] = json!(502);
    let reply = create(other_port).await;
    assert_eq!(reply.status, 201, "{}", reply.body);
    let mut other_gateway = rtu("meter-4", 1);
    other_gateway["hardware_id"] = json!(second_gateway);
    let reply = create(other_gateway).await;
    assert_eq!(reply.status, 201, "{}", reply.body);
    // Modbus TCP devices at one host and port with their own unit IDs (a Modbus TCP gateway in
    // front of several devices) are fine; the same unit ID is not.
    let mut tcp_two = tcp.clone();
    tcp_two["name"] = json!("meter-5");
    tcp_two["port"] = json!(502);
    tcp_two["unit_id"] = json!(6);
    let reply = create(tcp_two.clone()).await;
    assert_eq!(reply.status, 201, "{}", reply.body);
    tcp_two["name"] = json!("meter-6");
    let reply = create(tcp_two).await;
    assert_refused(&reply, 409, "conflict", Some("device.unit_taken"));
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_writes_on_one_converter_give_one_device() {
    let Some(product) = product_postgres(LOGINS).await else {
        return;
    };
    let issuer = TestIssuer::new();
    let assets = start(&product, &issuer).await;
    let mut admin = product.superuser().await;
    let tenant = known_tenant(&mut admin).await;
    let editor = issuer.tenant_token(ServiceName::Assets, tenant, TenantRole::Editor);
    let gateway = bound(&mut admin, tenant, "tg-race").await;
    let model_id = model(&assets, &editor, "meter").await;
    let body = |name: &str, transport: &str, unit_id: u8, port: u16| {
        json!({"name": name, "hardware_id": gateway, "model_id": model_id,
               "transport": transport, "host": "192.0.2.50", "port": port, "unit_id": unit_id})
    };

    // Rounds of two requests at once: the same unit ID, then the two transports.
    for (round, (left, right, key)) in [
        (
            body("same-unit-a", "rtu_over_tcp", 7, 8899),
            body("same-unit-b", "rtu_over_tcp", 7, 8899),
            "device.unit_taken",
        ),
        (
            body("mixed-a", "rtu_over_tcp", 1, 8886),
            body("mixed-b", "modbus_tcp", 2, 8886),
            "device.transport_mismatch",
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let (a, b) = tokio::join!(
            assets.json(Method::POST, DEVICES, Some(&editor), &left),
            assets.json(Method::POST, DEVICES, Some(&editor), &right),
        );
        let mut statuses = [a.status, b.status];
        statuses.sort_unstable();
        assert_eq!(
            statuses,
            [201, 409],
            "round {round}: {} / {}",
            a.body,
            b.body
        );
        let refused = if a.status == 409 { &a } else { &b };
        assert_refused(refused, 409, "conflict", Some(key));
    }
    let (devices, events) = footprint(&mut admin, tenant).await;
    assert_eq!(devices, 2);
    // The model's created event and one created event per device.
    assert_eq!(events, 3);
}

#[tokio::test(flavor = "multi_thread")]
async fn roles_decide_who_reads_and_who_writes() {
    let Some(product) = product_postgres(LOGINS).await else {
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
    let gateway = bound(&mut admin, tenant, "tg-roles").await;
    let model_id = model(&assets, &tenant_admin, "meter").await;

    // An admin includes an editor's rights.
    let create = json!({"name": "by-admin", "hardware_id": gateway, "model_id": model_id,
                        "transport": "modbus_tcp", "host": "192.0.2.60", "port": 502});
    let created = assets
        .json(Method::POST, DEVICES, Some(&tenant_admin), &create)
        .await;
    assert_eq!(created.status, 201, "{}", created.body);
    let device_path = format!(
        "{DEVICES}/{}",
        created.json()["device_id"].as_str().expect("ID")
    );
    let before = footprint(&mut admin, tenant).await;

    let writes = [
        (
            Method::POST,
            DEVICES.to_owned(),
            with(&create, &json!({"name": "by-viewer", "unit_id": 2})),
        ),
        (
            Method::PUT,
            device_path.clone(),
            json!({"revision": 1, "transport": "modbus_tcp", "host": "192.0.2.61", "port": 502}),
        ),
    ];
    for (method, path, body) in &writes {
        for token in [&viewer, &no_tenant] {
            let reply = assets.json(method.clone(), path, Some(token), body).await;
            assert_refused(&reply, 403, "permission_denied", None);
        }
        for token in [None, Some(elsewhere.as_str())] {
            let reply = assets.json(method.clone(), path, token, body).await;
            assert_refused(&reply, 401, "unauthenticated", None);
        }
    }
    for token in [Some(viewer.as_str()), Some(no_tenant.as_str()), None] {
        let reply = assets.call(Method::DELETE, &device_path, token, None).await;
        let (status, code) = if token.is_none() {
            (401, "unauthenticated")
        } else {
            (403, "permission_denied")
        };
        assert_refused(&reply, status, code, None);
    }
    assert_eq!(footprint(&mut admin, tenant).await, before);

    // A viewer reads; a user without a tenant does not; a path that is not a device ID is 404.
    for path in [DEVICES, device_path.as_str()] {
        let reply = assets.call(Method::GET, path, Some(&viewer), None).await;
        assert_eq!(reply.status, 200, "{path}: {}", reply.body);
        let reply = assets.call(Method::GET, path, Some(&no_tenant), None).await;
        assert_refused(&reply, 403, "permission_denied", None);
        let reply = assets.call(Method::GET, path, None, None).await;
        assert_refused(&reply, 401, "unauthenticated", None);
    }
    for method in [Method::GET, Method::DELETE] {
        let reply = assets
            .call(
                method,
                &format!("{DEVICES}/by-admin"),
                Some(&tenant_admin),
                None,
            )
            .await;
        assert_refused(&reply, 404, "not_found", None);
    }
}
