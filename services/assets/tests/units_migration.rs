//! 0006_device_model_point_units.sql (Laysi/trustiot-v2#982) on a database the previous release
//! filled: the migration Job applies the product with the assets service's files up to
//! 0005_device_update_columns.sql, models and points are written as that release's store wrote
//! them (in two tenants, with and without descriptions, scaling and lengths); then the Job runs
//! again with every file, as the next release's does, and
//!
//! - every model row is unchanged, and every point row is unchanged but for a new `unit` that is
//!   NULL;
//! - the service answers the old models exactly as before: no point has a `unit`;
//! - the previous release's insert, which does not name `unit`, still works (an image rolled
//!   back runs on the new schema), and the database refuses a unit over 24 characters.
//!
//! Starts a throwaway Postgres with TLS (podman or docker) of its own; skipped without either
//! unless `CI` is set.

mod support;

use std::sync::{Arc, OnceLock};

use contract::{DeviceModelId, ServiceName, TenantId, TenantRole};
use migrate::config::{Platform, PostgresSettings, RunConfig, WaitSettings};
use migrate::registry::{PRODUCT, Registry, Service as RegistryService};
use reqwest::Method;
use serde_json::{Value, json};
use service_kit::migrations::Migration;
use service_kit::postgres::SslMode;
use sqlx::{Connection as _, PgConnection};
use support::Service;
use testkit::identity::TestIssuer;
use testkit::postgres::{SERVER_HOST, TEST_PASSWORD, TlsPostgres, tls_postgres};
use testkit::product::{DATABASE, MIGRATOR, PRODUCT_INIT, ProductPostgres};

/// The assets service's files the previous release applied: up to 0005.
const PREVIOUS_FILES: usize = 5;

/// The file under test, the one the next release's Job adds.
const UNDER_TEST: &str = "0006_device_model_point_units.sql";

const CREATE_DATABASE: &str = "CREATE DATABASE trustiot OWNER trustiot_owner";

/// The Job leaves the service's roles NOLOGIN without a bound Secret; the chart's Secrets would
/// give them a password, as here.
const ALLOW_LOGIN: &str = "ALTER ROLE assets_app LOGIN PASSWORD 'testkit-postgres';
ALTER ROLE assets_platform LOGIN PASSWORD 'testkit-postgres'";

/// A tenant the service knows. `$1` tenant.
const INSERT_TENANT: &str = "INSERT INTO assets.tenants (tenant_id) VALUES ($1::uuid)";

/// A model as the previous release's store inserted it. `$1` tenant, `$2` model, `$3` name.
const INSERT_MODEL: &str = "INSERT INTO assets.device_models (tenant_id, model_id, name)
VALUES ($1::uuid, $2::uuid, $3)";

/// The previous release's insert of a model's points (its `INSERT_POINTS`, which does not name
/// `unit`). `$1` tenant, `$2` model, then one array per column.
const INSERT_POINTS: &str = "INSERT INTO assets.device_model_points
    (tenant_id, model_id, position, key, function_code, address, data_type, byte_order, length,
     scale, value_offset, description)
SELECT $1::uuid, $2::uuid, point.position, point.key, point.function_code, point.address,
    point.data_type, point.byte_order, point.length, point.scale, point.value_offset,
    point.description
FROM UNNEST($3::integer[], $4::text[], $5::smallint[], $6::integer[], $7::text[], $8::text[],
    $9::integer[], $10::double precision[], $11::double precision[], $12::text[])
    AS point (position, key, function_code, address, data_type, byte_order, length, scale,
              value_offset, description)";

/// Every row of both tables, as the superuser (no row-level security) reads them.
const MODELS: &str =
    "SELECT coalesce(jsonb_agg(to_jsonb(m) ORDER BY tenant_id, model_id), '[]')::text
    FROM assets.device_models m";
const POINTS: &str = "SELECT coalesce(jsonb_agg(to_jsonb(p)
    ORDER BY tenant_id, model_id, position), '[]')::text FROM assets.device_model_points p";

/// The files the Job applied in the assets service's schema, by name.
const APPLIED: &str = "SELECT coalesce(jsonb_agg(name ORDER BY name), '[]')::text
    FROM assets.trustiot_migrate_applied WHERE component = 'assets'";

/// One point with a unit of `$3`, in a transaction the test rolls back. `$1` tenant, `$2` model.
const INSERT_UNIT: &str = "INSERT INTO assets.device_model_points
    (tenant_id, model_id, position, key, function_code, address, data_type, unit)
VALUES ($1::uuid, $2::uuid, 99, 'with-unit', 4, 900, 'u16', $3)";

/// The registry of the previous release: the product with every assets file before
/// [`UNDER_TEST`], which is the last one.
fn previous_release() -> Registry {
    static MIGRATIONS: OnceLock<Vec<Migration>> = OnceLock::new();
    static SERVICES: OnceLock<Vec<RegistryService>> = OnceLock::new();
    let services = SERVICES.get_or_init(|| {
        PRODUCT
            .services
            .iter()
            .map(|service| {
                let mut service = *service;
                if service.name == "assets" {
                    let all = service.migrations;
                    let under_test = all
                        .iter()
                        .position(|migration| migration.name == UNDER_TEST)
                        .expect("the file under test");
                    assert_eq!(under_test, PREVIOUS_FILES, "{UNDER_TEST} follows 0005");
                    assert_eq!(
                        all.len(),
                        PREVIOUS_FILES + 1,
                        "{UNDER_TEST} is the last file"
                    );
                    service.migrations = MIGRATIONS.get_or_init(|| all[..PREVIOUS_FILES].to_vec());
                }
                service
            })
            .collect()
    });
    Registry {
        services: services.as_slice(),
        ..PRODUCT
    }
}

fn run_config(postgres: &TlsPostgres, ca_file: &std::path::Path) -> RunConfig {
    RunConfig {
        platform: Platform {
            postgres_host: SERVER_HOST.to_owned(),
            postgres_port: postgres.address.port(),
            postgres_database: DATABASE.to_owned(),
            postgres_sslmode: SslMode::VerifyFull,
            postgres_ca_file: ca_file.to_owned(),
            clickhouse_url: None,
            clickhouse_ca_file: None,
        },
        postgres: PostgresSettings {
            username: MIGRATOR.to_owned(),
            password: TEST_PASSWORD.to_owned(),
        },
        clickhouse: None,
        passwords: None,
        wait: WaitSettings::default(),
    }
}

/// One run of the migration Job with `registry`, then LOGIN for the service's roles.
async fn migrate(product: &ProductPostgres, registry: &Registry, what: &str) {
    migrate::run(&run_config(&product.postgres, &product.ca_file), registry)
        .await
        .unwrap_or_else(|error| panic!("the migration Job's run ({what}): {error}"));
    let mut admin = product.superuser().await;
    sqlx::raw_sql(ALLOW_LOGIN)
        .execute(&mut admin)
        .await
        .expect("let the service's roles log in");
    admin.close().await.expect("close");
}

/// The JSON text `query` returns, read back.
async fn json_of(admin: &mut PgConnection, query: &'static str) -> Value {
    let text: String = sqlx::query_scalar(query)
        .fetch_one(&mut *admin)
        .await
        .unwrap_or_else(|error| panic!("{query}: {error}"));
    serde_json::from_str(&text).expect("JSON text")
}

/// The points of one model, one array per column of [`INSERT_POINTS`].
struct OldPoints {
    keys: Vec<&'static str>,
    function_codes: Vec<i16>,
    addresses: Vec<i32>,
    data_types: Vec<&'static str>,
    byte_orders: Vec<&'static str>,
    lengths: Vec<Option<i32>>,
    scales: Vec<Option<f64>>,
    offsets: Vec<Option<f64>>,
    descriptions: Vec<Option<&'static str>>,
}

/// A model and its points, written as the previous release's store wrote them.
async fn old_model(
    admin: &mut PgConnection,
    tenant: TenantId,
    name: &str,
    points: &OldPoints,
) -> String {
    let model = DeviceModelId::generate().to_string();
    let mut tx = admin.begin().await.expect("begin");
    sqlx::query(INSERT_MODEL)
        .bind(tenant.to_string())
        .bind(&model)
        .bind(name)
        .execute(&mut *tx)
        .await
        .expect("insert the model");
    let positions: Vec<i32> = (0..i32::try_from(points.keys.len()).expect("few")).collect();
    sqlx::query(INSERT_POINTS)
        .bind(tenant.to_string())
        .bind(&model)
        .bind(positions)
        .bind(&points.keys)
        .bind(&points.function_codes)
        .bind(&points.addresses)
        .bind(&points.data_types)
        .bind(&points.byte_orders)
        .bind(&points.lengths)
        .bind(&points.scales)
        .bind(&points.offsets)
        .bind(&points.descriptions)
        .execute(&mut *tx)
        .await
        .expect("insert the points");
    tx.commit().await.expect("commit");
    model
}

/// The SQLSTATE and constraint of inserting a point with `unit`, rolled back; `None` when the
/// database accepts it.
async fn insert_unit(
    admin: &mut PgConnection,
    tenant: TenantId,
    model: &str,
    unit: &str,
) -> Option<(String, String)> {
    let mut tx = admin.begin().await.expect("begin");
    let inserted = sqlx::query(INSERT_UNIT)
        .bind(tenant.to_string())
        .bind(model)
        .bind(unit)
        .execute(&mut *tx)
        .await;
    tx.rollback().await.expect("roll back");
    inserted.err().map(|error| {
        let database = error
            .as_database_error()
            .unwrap_or_else(|| panic!("not a database error: {error}"));
        (
            database.code().unwrap_or_default().into_owned(),
            database.constraint().unwrap_or_default().to_owned(),
        )
    })
}

#[tokio::test]
async fn existing_points_get_an_empty_unit_and_nothing_else_changes() {
    let Some(postgres) = tls_postgres(PRODUCT_INIT).await else {
        return;
    };
    let mut admin = PgConnection::connect_with(&postgres.superuser("postgres"))
        .await
        .expect("connect as the superuser");
    sqlx::raw_sql(CREATE_DATABASE)
        .execute(&mut admin)
        .await
        .expect("create the product database");
    admin.close().await.expect("close");
    let dir = std::env::temp_dir().join(format!(
        "assets-units-migration-{}-{}",
        std::process::id(),
        postgres.address.port()
    ));
    std::fs::create_dir_all(&dir).expect("a directory for the CA file");
    let ca_file = dir.join("postgres-ca.crt");
    std::fs::write(&ca_file, &postgres.ca_pem).expect("write the CA file");
    let product = Arc::new(ProductPostgres { postgres, ca_file });

    // The previous release's schema.
    migrate(&product, &previous_release(), "up to 0005").await;
    let mut admin = product.superuser().await;
    assert_eq!(
        json_of(&mut admin, APPLIED).await,
        json!([
            "0001_assets.sql",
            "0002_device_models.sql",
            "0003_gateway_pool.sql",
            "0004_devices.sql",
            "0005_device_update_columns.sql"
        ])
    );

    // What that release wrote: a transmitter and a meter in one tenant, a model without points
    // and one with in another.
    let tenant = TenantId::generate();
    let other = TenantId::generate();
    for id in [tenant, other] {
        sqlx::query(INSERT_TENANT)
            .bind(id.to_string())
            .execute(&mut admin)
            .await
            .expect("insert the tenant");
    }
    let transmitter = old_model(
        &mut admin,
        tenant,
        "ebyte-eid041-temp-humidity",
        &OldPoints {
            keys: vec!["temperature", "humidity"],
            function_codes: vec![3, 3],
            addresses: vec![0, 1],
            data_types: vec!["i16", "u16"],
            byte_orders: vec!["abcd", "abcd"],
            lengths: vec![None, None],
            scales: vec![Some(0.1), Some(0.1)],
            offsets: vec![None, None],
            descriptions: vec![Some("Air temperature (°C)"), None],
        },
    )
    .await;
    old_model(
        &mut admin,
        tenant,
        "power-meter",
        &OldPoints {
            keys: vec!["energy", "serial", "running"],
            function_codes: vec![4, 3, 1],
            addresses: vec![0, 10, 0],
            data_types: vec!["u32", "string", "bool"],
            byte_orders: vec!["cdab", "badc", "abcd"],
            lengths: vec![None, Some(8), None],
            scales: vec![Some(0.01), None, None],
            offsets: vec![Some(-40.0), None, None],
            descriptions: vec![None, Some("序號，ASCII"), None],
        },
    )
    .await;
    let empty = OldPoints {
        keys: vec![],
        function_codes: vec![],
        addresses: vec![],
        data_types: vec![],
        byte_orders: vec![],
        lengths: vec![],
        scales: vec![],
        offsets: vec![],
        descriptions: vec![],
    };
    old_model(&mut admin, other, "no-points", &empty).await;
    old_model(
        &mut admin,
        other,
        "flow-meter",
        &OldPoints {
            keys: vec!["flow"],
            function_codes: vec![4],
            addresses: vec![2],
            data_types: vec!["f32"],
            byte_orders: vec!["dcba"],
            lengths: vec![None],
            scales: vec![None],
            offsets: vec![Some(1.5)],
            descriptions: vec![Some("Flow\nin m³/h")],
        },
    )
    .await;
    let models_before = json_of(&mut admin, MODELS).await;
    let points_before = json_of(&mut admin, POINTS).await;
    assert_eq!(points_before.as_array().map(Vec::len), Some(6));
    assert!(points_before[0].get("unit").is_none(), "{points_before}");
    admin.close().await.expect("close");

    // The next release's Job.
    migrate(&product, &PRODUCT, "every file").await;
    let mut admin = product.superuser().await;
    let applied = json_of(&mut admin, APPLIED).await;
    assert_eq!(
        applied
            .as_array()
            .and_then(|names| names.get(PREVIOUS_FILES)),
        Some(&Value::from(UNDER_TEST))
    );

    // Models unchanged; every point unchanged but for an empty unit.
    assert_eq!(json_of(&mut admin, MODELS).await, models_before);
    let mut expected = points_before.clone();
    for point in expected.as_array_mut().expect("rows") {
        point["unit"] = Value::Null;
    }
    assert_eq!(json_of(&mut admin, POINTS).await, expected);

    // The previous release's insert still works on the new schema, without a unit.
    let rolled_back = old_model(
        &mut admin,
        tenant,
        "rolled-back",
        &OldPoints {
            keys: vec!["level"],
            function_codes: vec![3],
            addresses: vec![0],
            data_types: vec!["u16"],
            byte_orders: vec!["abcd"],
            lengths: vec![None],
            scales: vec![None],
            offsets: vec![None],
            descriptions: vec![None],
        },
    )
    .await;
    // The database repeats the API's length rule: 1 to 24 characters.
    assert_eq!(
        insert_unit(&mut admin, tenant, &rolled_back, &"℃".repeat(24)).await,
        None
    );
    let too_long = "u".repeat(25);
    for refused in ["", too_long.as_str()] {
        assert_eq!(
            insert_unit(&mut admin, tenant, &rolled_back, refused).await,
            Some((
                "23514".to_owned(),
                "device_model_points_unit_length".to_owned()
            )),
            "{refused:?}"
        );
    }
    admin.close().await.expect("close");

    // The service answers the old model as the previous release did: no unit on any point.
    let issuer = TestIssuer::new();
    let assets = Service::start(&product, &issuer.jwks()).await;
    let viewer = issuer.tenant_token(ServiceName::Assets, tenant, TenantRole::Viewer);
    let read = assets
        .call(
            Method::GET,
            &format!("/api/assets/v1/device-models/{transmitter}"),
            Some(&viewer),
            None,
        )
        .await;
    assert_eq!(read.status, 200, "{}", read.body);
    assert_eq!(
        read.json()["points"],
        json!([
            {"key": "temperature", "function_code": 3, "address": 0, "data_type": "i16",
             "byte_order": "abcd", "scale": 0.1, "description": "Air temperature (°C)"},
            {"key": "humidity", "function_code": 3, "address": 1, "data_type": "u16",
             "byte_order": "abcd", "scale": 0.1},
        ])
    );
}
