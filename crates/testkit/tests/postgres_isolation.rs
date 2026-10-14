//! The suite against a real Postgres and a real HTTP service: it passes when tenant isolation
//! holds and fails when it does not, so a passing run in a service's CI means something.
//!
//! The example is service-kit's own template (`sql/database.sql`, `sql/tenant-table.sql`): the
//! table `example_items` with row-level security, and a small service in this file whose
//! handlers use `TenantDb` as a product service does. Each test breaks one thing (row-level
//! security off, a table nobody declared, an endpoint that reads or writes through the
//! migration role) and checks the suite reports it.
//!
//! Starts a throwaway Postgres with TLS (podman or docker); skipped without either unless `CI`
//! is set. Run with `--nocapture` to see every probe.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::{FromRef, Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use contract::ServiceName;
use serde::Deserialize;
use serde_json::{Value, json};
use service_kit::identity::{IdentityVerifier, UserIdentity};
use service_kit::tenant::{TenantDb, TenantDbError};
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgConnection, PgPool};
use testkit::identity::TestIssuer;
use testkit::isolation::api::{Endpoint, Method};
use testkit::isolation::postgres::PgStore;
use testkit::isolation::{Report, Suite};
use testkit::pg_table;
use testkit::postgres::{TlsPostgres, tls_postgres};

/// The roles of decision 29: the migration role owns the tables, the service role reads and
/// writes in tenant transactions, the platform role is the one the template's
/// `platform_access` policy names.
const ROLES: &str = "
CREATE ROLE example_owner LOGIN PASSWORD 'testkit-postgres';
CREATE ROLE example_app LOGIN PASSWORD 'testkit-postgres';
CREATE ROLE example_platform LOGIN PASSWORD 'testkit-postgres';
";

/// The seed of the template table: `$1` tenant, `$2` row id, `$3` canary as the name.
const SEED_ITEM: &str = "INSERT INTO example_items (tenant_id, id, name) VALUES ($1, $2, $3)";

const SERVICE: ServiceName = ServiceName::Assets;

/// One test's database, with the template table.
struct Fixture {
    postgres: Arc<TlsPostgres>,
    database: &'static str,
}

impl Fixture {
    /// Creates the database with `create` (a static `CREATE DATABASE ... OWNER example_owner`)
    /// and runs the kit's database and table templates as the owner.
    async fn new(create: &'static str, database: &'static str) -> Option<Self> {
        let postgres = tls_postgres(ROLES).await?;
        let mut admin = postgres.connect("postgres", "postgres").await;
        sqlx::raw_sql(create)
            .execute(&mut admin)
            .await
            .expect("create the database");
        let fixture = Self { postgres, database };
        fixture
            .as_owner(include_str!("../../service-kit/sql/database.sql"))
            .await;
        fixture
            .as_owner(include_str!("../../service-kit/sql/tenant-table.sql"))
            .await;
        Some(fixture)
    }

    async fn owner(&self) -> PgConnection {
        self.postgres.connect("example_owner", self.database).await
    }

    /// Runs static SQL as the owner (the migration role).
    async fn as_owner(&self, sql: &'static str) {
        sqlx::raw_sql(sql)
            .execute(&mut self.owner().await)
            .await
            .unwrap_or_else(|error| panic!("{sql}: {error}"));
    }

    /// The store as a service declares it: the owner seeds, the service role is attacked.
    fn store(&self) -> PgStore {
        PgStore::new(
            self.postgres.options("example_owner", self.database),
            self.postgres.options("example_app", self.database),
        )
        .table(pg_table!("example_items", seed = SEED_ITEM))
    }

    /// Starts the example service on a free local port; returns its base URL.
    async fn start_service(&self, issuer: &TestIssuer, flaw: Flaw) -> String {
        let db = TenantDb::connect_lazy(
            self.postgres.options("example_app", self.database),
            PgPoolOptions::new()
                .max_connections(4)
                .acquire_timeout(Duration::from_secs(2)),
        )
        .expect("verify-full options");
        let owner = match flaw {
            Flaw::None => None,
            Flaw::ReadsAsOwner | Flaw::WritesAsOwner => Some(
                PgPoolOptions::new()
                    .max_connections(2)
                    .connect_with(self.postgres.options("example_owner", self.database))
                    .await
                    .expect("owner pool"),
            ),
        };
        let state = AppState {
            verifier: issuer.verifier(SERVICE),
            db,
            owner,
            flaw,
        };
        let router = Router::new()
            .route("/v1/items", get(list_items).post(create_item))
            .route(
                "/v1/items/{id}",
                get(get_item).put(rename_item).delete(delete_item),
            )
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a local port");
        let address = listener.local_addr().expect("local address");
        tokio::spawn(async move { axum::serve(listener, router).await });
        format!("http://{address}")
    }
}

/// The example service's OpenAPI document, as `utoipa` would generate it (paths only).
fn openapi() -> Value {
    json!({
        "openapi": "3.1.0",
        "info": {"title": "example", "version": "0.1.0"},
        "paths": {
            "/v1/items": {"get": {}, "post": {}},
            "/v1/items/{id}": {"get": {}, "put": {}, "delete": {}}
        }
    })
}

/// The example's suite: the one table and its five endpoints, each a single declaration. The
/// `DELETE` comes before the `PUT`, so each write's positive control must use its own row.
fn suite<'a>(issuer: &'a TestIssuer, store: PgStore) -> Suite<'a, PgStore> {
    Suite::new(SERVICE, issuer, store)
        .openapi(openapi())
        .endpoint(Endpoint::list("/v1/items", "example_items"))
        .endpoint(Endpoint::get("/v1/items/{id}", "example_items"))
        .endpoint(Endpoint::item(
            Method::DELETE,
            "/v1/items/{id}",
            "example_items",
        ))
        .endpoint(
            Endpoint::item(Method::PUT, "/v1/items/{id}", "example_items")
                .body(json!({"name": "{canary}"})),
        )
        .endpoint(
            Endpoint::create(Method::POST, "/v1/items", "example_items")
                .body(json!({"name": "{canary}", "tenant_id": "{tenant}"})),
        )
}

/// A deliberate flaw of the example service, for the tests that prove the suite catches it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Flaw {
    None,
    /// `GET /v1/items/{id}` reads through the migration role, which row-level security exempts.
    ReadsAsOwner,
    /// `PUT /v1/items/{id}` writes through the migration role.
    WritesAsOwner,
}

#[derive(Clone)]
struct AppState {
    verifier: IdentityVerifier,
    db: TenantDb,
    owner: Option<PgPool>,
    flaw: Flaw,
}

impl FromRef<AppState> for IdentityVerifier {
    fn from_ref(state: &AppState) -> Self {
        state.verifier.clone()
    }
}

fn failure(error: &TenantDbError) -> Response {
    match error {
        TenantDbError::NoTenant => StatusCode::FORBIDDEN.into_response(),
        _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

fn server_error(_: sqlx::Error) -> Response {
    StatusCode::INTERNAL_SERVER_ERROR.into_response()
}

fn item(row: (String, String)) -> Value {
    json!({"id": row.0, "name": row.1})
}

async fn list_items(State(state): State<AppState>, user: UserIdentity) -> Response {
    let mut tx = match state.db.begin(&user).await {
        Ok(tx) => tx,
        Err(error) => return failure(&error),
    };
    match sqlx::query_as::<_, (String, String)>(
        "SELECT id::text, name FROM example_items ORDER BY name",
    )
    .fetch_all(&mut *tx)
    .await
    {
        Ok(rows) => {
            let items: Vec<Value> = rows.into_iter().map(item).collect();
            axum::Json(json!({ "items": items })).into_response()
        }
        Err(error) => server_error(error),
    }
}

async fn get_item(
    State(state): State<AppState>,
    user: UserIdentity,
    Path(id): Path<String>,
) -> Response {
    const ITEM: &str = "SELECT id::text, name FROM example_items WHERE id = $1::uuid";
    let row = match (&state.owner, state.flaw) {
        (Some(owner), Flaw::ReadsAsOwner) => sqlx::query_as::<_, (String, String)>(ITEM)
            .bind(&id)
            .fetch_optional(owner)
            .await
            .map_err(server_error),
        _ => match state.db.begin(&user).await {
            Ok(mut tx) => sqlx::query_as::<_, (String, String)>(ITEM)
                .bind(&id)
                .fetch_optional(&mut *tx)
                .await
                .map_err(server_error),
            Err(error) => Err(failure(&error)),
        },
    };
    match row {
        Ok(Some(row)) => axum::Json(item(row)).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(response) => response,
    }
}

async fn delete_item(
    State(state): State<AppState>,
    user: UserIdentity,
    Path(id): Path<String>,
) -> Response {
    let mut tx = match state.db.begin(&user).await {
        Ok(tx) => tx,
        Err(error) => return failure(&error),
    };
    let deleted = sqlx::query("DELETE FROM example_items WHERE id = $1::uuid")
        .bind(&id)
        .execute(&mut *tx)
        .await;
    match deleted {
        Ok(done) if done.rows_affected() == 0 => StatusCode::NOT_FOUND.into_response(),
        Ok(_) => match tx.commit().await {
            Ok(()) => StatusCode::NO_CONTENT.into_response(),
            Err(error) => server_error(error),
        },
        Err(error) => server_error(error),
    }
}

#[derive(Deserialize)]
struct Rename {
    name: String,
}

async fn rename_item(
    State(state): State<AppState>,
    user: UserIdentity,
    Path(id): Path<String>,
    axum::Json(body): axum::Json<Rename>,
) -> Response {
    const RENAME: &str = "UPDATE example_items SET name = $2 WHERE id = $1::uuid";
    let changed = match (&state.owner, state.flaw) {
        (Some(owner), Flaw::WritesAsOwner) => sqlx::query(RENAME)
            .bind(&id)
            .bind(&body.name)
            .execute(owner)
            .await
            .map(|done| done.rows_affected())
            .map_err(server_error),
        _ => match state.db.begin(&user).await {
            Ok(mut tx) => {
                let done = sqlx::query(RENAME)
                    .bind(&id)
                    .bind(&body.name)
                    .execute(&mut *tx)
                    .await;
                match done {
                    Ok(done) => tx
                        .commit()
                        .await
                        .map(|()| done.rows_affected())
                        .map_err(server_error),
                    Err(error) => Err(server_error(error)),
                }
            }
            Err(error) => Err(failure(&error)),
        },
    };
    match changed {
        Ok(0) => StatusCode::NOT_FOUND.into_response(),
        Ok(_) => axum::Json(json!({"id": id, "name": body.name})).into_response(),
        Err(response) => response,
    }
}

/// The body may name a tenant; the handler ignores it and writes in the caller's tenant.
#[derive(Deserialize)]
struct NewItem {
    name: String,
    #[serde(rename = "tenant_id")]
    _tenant_id: Option<String>,
}

async fn create_item(
    State(state): State<AppState>,
    user: UserIdentity,
    axum::Json(body): axum::Json<NewItem>,
) -> Response {
    let mut tx = match state.db.begin(&user).await {
        Ok(tx) => tx,
        Err(error) => return failure(&error),
    };
    let tenant = tx.tenant_id().to_string();
    let created = sqlx::query_scalar::<_, String>(
        "INSERT INTO example_items (tenant_id, id, name) \
         VALUES ($1::uuid, gen_random_uuid(), $2) RETURNING id::text",
    )
    .bind(tenant)
    .bind(&body.name)
    .fetch_one(&mut *tx)
    .await;
    match created {
        Ok(id) => match tx.commit().await {
            Ok(()) => (StatusCode::CREATED, axum::Json(json!({ "id": id }))).into_response(),
            Err(error) => server_error(error),
        },
        Err(error) => server_error(error),
    }
}

fn has_failure(report: &Report, prefix: &str) -> bool {
    report
        .failures()
        .iter()
        .any(|failure| failure.starts_with(prefix))
}

/// The acceptance example: with row-level security on, every probe passes; with it off on
/// `example_items`, the suite fails; turned on again, it passes.
#[tokio::test]
async fn the_example_fails_with_rls_off_and_passes_with_it_on() {
    let Some(fixture) = Fixture::new(
        "CREATE DATABASE testkit_rls OWNER example_owner",
        "testkit_rls",
    )
    .await
    else {
        return;
    };
    let issuer = TestIssuer::new();
    let base_url = fixture.start_service(&issuer, Flaw::None).await;

    println!("== row-level security on");
    let on = suite(&issuer, fixture.store()).run(Some(&base_url)).await;
    on.assert_isolated();

    fixture
        .as_owner("ALTER TABLE example_items DISABLE ROW LEVEL SECURITY")
        .await;
    println!("== row-level security off on example_items");
    let off = suite(&issuer, fixture.store()).run(Some(&base_url)).await;
    println!("{off}");
    assert!(!off.is_isolated(), "the suite must fail with RLS off");
    let failures = off.failures().join("\n");
    assert!(
        failures.contains("example_items")
            && failures.contains("row-level security is not enabled"),
        "the failure must name the unprotected table:\n{failures}"
    );
    assert!(
        has_failure(&off, "GET /v1/items | list as tenant A"),
        "the endpoints fail too:\n{failures}"
    );

    fixture
        .as_owner("ALTER TABLE example_items ENABLE ROW LEVEL SECURITY")
        .await;
    println!("== row-level security on again");
    let again = suite(&issuer, fixture.store()).run(Some(&base_url)).await;
    again.assert_isolated();
}

/// A table the service role can use but the suite does not declare fails the run, even though
/// the table itself is protected.
#[tokio::test]
async fn a_forgotten_table_fails() {
    let Some(fixture) = Fixture::new(
        "CREATE DATABASE testkit_forgotten OWNER example_owner",
        "testkit_forgotten",
    )
    .await
    else {
        return;
    };
    fixture
        .as_owner(
            "CREATE TABLE example_notes (tenant_id uuid NOT NULL, id uuid NOT NULL, body text NOT NULL, PRIMARY KEY (tenant_id, id));
             ALTER TABLE example_notes ENABLE ROW LEVEL SECURITY;
             CREATE POLICY tenant_isolation ON example_notes
                 USING (tenant_id = NULLIF(current_setting('trustiot.tenant_id', true), '')::uuid)
                 WITH CHECK (tenant_id = NULLIF(current_setting('trustiot.tenant_id', true), '')::uuid);
             GRANT SELECT ON example_notes TO example_app;",
        )
        .await;
    let issuer = TestIssuer::new();
    let report = Suite::new(SERVICE, &issuer, fixture.store())
        .without_api("database probes only")
        .run(None)
        .await;
    println!("{report}");
    assert_eq!(
        report.failure_count(),
        1,
        "only the coverage gap fails:\n{}",
        report.failures().join("\n")
    );
    assert!(has_failure(
        &report,
        "public.example_notes | coverage | the service role can use this table"
    ));
}

/// An endpoint that reads through a role row-level security exempts shows tenant B's row to
/// tenant A; the database probes cannot see it, the endpoint probes do.
#[tokio::test]
async fn an_endpoint_that_reads_across_tenants_fails() {
    let Some(fixture) = Fixture::new(
        "CREATE DATABASE testkit_leaky_read OWNER example_owner",
        "testkit_leaky_read",
    )
    .await
    else {
        return;
    };
    let issuer = TestIssuer::new();
    let base_url = fixture.start_service(&issuer, Flaw::ReadsAsOwner).await;
    let report = suite(&issuer, fixture.store()).run(Some(&base_url)).await;
    println!("{report}");
    let failures = report.failures().join("\n");
    assert!(
        has_failure(
            &report,
            "GET /v1/items/{id} | tenant B's row as tenant A | 200 shows tenant B's data"
        ),
        "{failures}"
    );
    // A user without a tenant reads tenant A's row through the same flaw.
    assert!(
        has_failure(
            &report,
            "GET /v1/items/{id} | without a tenant | 200 shows tenant data"
        ),
        "{failures}"
    );
    // Tenant B reads tenant A's row the same way.
    assert!(
        has_failure(
            &report,
            "GET /v1/items/{id} | tenant A's row as tenant B | 200 shows tenant A's data"
        ),
        "{failures}"
    );
    // The database itself is isolated: every database probe passes, and only these two probes
    // fail, in both rounds.
    assert_eq!(report.failure_count(), 4, "{failures}");
}

/// An endpoint that writes through a role row-level security exempts changes tenant B's row;
/// the suite sees the 200 and the changed row.
#[tokio::test]
async fn an_endpoint_that_writes_across_tenants_fails() {
    let Some(fixture) = Fixture::new(
        "CREATE DATABASE testkit_leaky_write OWNER example_owner",
        "testkit_leaky_write",
    )
    .await
    else {
        return;
    };
    let issuer = TestIssuer::new();
    let base_url = fixture.start_service(&issuer, Flaw::WritesAsOwner).await;
    let report = suite(&issuer, fixture.store()).run(Some(&base_url)).await;
    println!("{report}");
    let failures = report.failures().join("\n");
    assert!(
        has_failure(
            &report,
            "PUT /v1/items/{id} | tenant B's row as tenant A | expected 403 or 404"
        ),
        "{failures}"
    );
    assert!(
        has_failure(
            &report,
            "api | tenant B rows after tenant A's endpoint probes | changed"
        ),
        "{failures}"
    );
}

/// A table with one row per tenant, keyed by `tenant_id` (Laysi/trustiot-v2#65).
const SETTINGS_TABLE: &str = "
CREATE TABLE example_settings (tenant_id uuid PRIMARY KEY, theme text NOT NULL);
ALTER TABLE example_settings ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON example_settings
    USING (tenant_id = NULLIF(current_setting('trustiot.tenant_id', true), '')::uuid)
    WITH CHECK (tenant_id = NULLIF(current_setting('trustiot.tenant_id', true), '')::uuid);
GRANT SELECT, INSERT ON example_settings TO example_app;
";

/// `$2` is the tenant id for such a table; it is not stored, but Postgres needs its type.
const SEED_SETTINGS: &str =
    "INSERT INTO example_settings (tenant_id, theme) SELECT $1, $3 WHERE $2::uuid IS NOT NULL";

/// A table that holds one row per tenant cannot take the two rows per tenant the suite seeds
/// into other tables; `one_per_tenant` seeds one, whose id is the tenant id, and every probe
/// still runs. With row-level security off, the same table fails.
#[tokio::test]
async fn a_table_with_one_row_per_tenant_is_attacked_too() {
    let Some(fixture) = Fixture::new(
        "CREATE DATABASE testkit_one_per_tenant OWNER example_owner",
        "testkit_one_per_tenant",
    )
    .await
    else {
        return;
    };
    fixture.as_owner(SETTINGS_TABLE).await;
    let issuer = TestIssuer::new();
    let store = || {
        fixture.store().table(pg_table!(
            "example_settings",
            seed = SEED_SETTINGS,
            one_per_tenant
        ))
    };
    let report = Suite::new(SERVICE, &issuer, store())
        .without_api("the database probes only")
        .run(None)
        .await;
    println!("{report}");
    report.assert_isolated();

    fixture
        .as_owner("ALTER TABLE example_settings DISABLE ROW LEVEL SECURITY")
        .await;
    let off = Suite::new(SERVICE, &issuer, store())
        .without_api("the database probes only")
        .run(None)
        .await;
    let failures = off.failures().join("\n");
    assert!(
        failures.contains("example_settings"),
        "the suite must fail with row-level security off:\n{failures}"
    );
}
