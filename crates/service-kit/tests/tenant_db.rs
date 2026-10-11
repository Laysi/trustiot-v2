//! Tenant isolation against a real Postgres (decision 29): row-level security with the policy
//! template in `sql/tenant-table.sql`, the kit's tenant transactions, the session check, and
//! server verification (checklist M3), library calls and the example service end to end.
//!
//! The tests start a throwaway Postgres with TLS (podman or docker; skipped without either
//! unless `CI` is set) and create the roles of decision 29 in it: `example_owner` runs the
//! migration and owns the tables, `example_app` is the service role, `example_platform` the
//! platform role. Each test works in its own database. `TEST_DATABASE_URL` is not used: the
//! tests create roles as the superuser and need a server whose CA they know.
#![cfg(feature = "contract")]

mod support;

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use contract::{ServiceName, TenantId};
use ed25519_dalek::{Signer, SigningKey};
use serde_json::{Value, json};
use service_kit::identity::{IdentityKeys, IdentityVerifier, ServiceIdentity, UserIdentity};
use service_kit::postgres::{self, InsecureConnection, SessionRole, UnsafeSession};
use service_kit::tenant::{PlatformActor, PlatformDb, TenantDb, TenantDbError};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions, PgSslMode};
use sqlx::{Connection, PgConnection, PgPool};
use support::postgres::{SERVER_HOST, TestCa, TlsPostgres, tls_postgres};
use support::{Service, request, spawn_example, wait_exit};

const TENANT_A: &str = "01928f6e-7a3c-7d4e-8f00-00000000000a";
const TENANT_B: &str = "01928f6e-7a3c-7d4e-8f00-00000000000b";
const USER: &str = "01928f6e-7a3c-7d4e-8f00-0123456789ab";

/// The roles of decision 29, created once per Postgres. `example_reader` stands for the
/// platform role of a service that consumes `example_app`'s outbox events. `example_bypass` and
/// `example_owner_member` exist only to prove the session check refuses them.
const ROLES: &str = "
CREATE ROLE example_owner LOGIN PASSWORD 'service-kit-test';
CREATE ROLE example_app LOGIN PASSWORD 'service-kit-test';
CREATE ROLE example_platform LOGIN PASSWORD 'service-kit-test';
CREATE ROLE example_reader LOGIN PASSWORD 'service-kit-test';
CREATE ROLE example_bypass LOGIN BYPASSRLS PASSWORD 'service-kit-test';
CREATE ROLE example_owner_member LOGIN PASSWORD 'service-kit-test' IN ROLE example_owner;
CREATE ROLE example_platform_group NOLOGIN;
GRANT example_platform_group TO example_platform;
";

/// The seed rows, inserted by the owner (exempt from row-level security).
const SEED: &str = "
INSERT INTO example_items (tenant_id, id, name) VALUES
    ('01928f6e-7a3c-7d4e-8f00-00000000000a', '00000000-0000-7000-8000-0000000000a1', 'a-1'),
    ('01928f6e-7a3c-7d4e-8f00-00000000000a', '00000000-0000-7000-8000-0000000000a2', 'a-2'),
    ('01928f6e-7a3c-7d4e-8f00-00000000000b', '00000000-0000-7000-8000-0000000000b1', 'b-1');
";

const COUNT_ITEMS: &str = "SELECT count(*) FROM example_items";
const ITEM_NAMES: &str = "SELECT name FROM example_items ORDER BY name";
const INSERT_ITEM: &str =
    "INSERT INTO example_items (tenant_id, id, name) VALUES ($1::uuid, $2::uuid, $3)";
const CURRENT_TENANT: &str = "SELECT current_setting('trustiot.tenant_id', true)";
const BACKEND_PID: &str = "SELECT pg_backend_pid()";

/// SQLSTATE `insufficient_privilege`: row-level security violations and missing rights.
const INSUFFICIENT_PRIVILEGE: &str = "42501";

/// One test's database, with the template table and the seed rows.
struct Fixture {
    postgres: Arc<TlsPostgres>,
    database: &'static str,
}

impl Fixture {
    /// Creates `database` with `create` (a static `CREATE DATABASE ... OWNER example_owner`),
    /// runs the database and policy templates as the owner, and seeds it.
    async fn new(create: &'static str, database: &'static str) -> Option<Self> {
        let postgres = tls_postgres(ROLES).await?;
        let mut admin = PgConnection::connect_with(&postgres.superuser("postgres"))
            .await
            .expect("connect as the superuser");
        sqlx::raw_sql(create)
            .execute(&mut admin)
            .await
            .expect("create the database");
        let fixture = Self { postgres, database };
        let mut owner = fixture.connect("example_owner").await;
        sqlx::raw_sql(include_str!("../sql/database.sql"))
            .execute(&mut owner)
            .await
            .expect("run the database template");
        sqlx::raw_sql(include_str!("../sql/tenant-table.sql"))
            .execute(&mut owner)
            .await
            .expect("run the policy template");
        sqlx::raw_sql(SEED)
            .execute(&mut owner)
            .await
            .expect("seed the rows");
        Some(fixture)
    }

    fn options(&self, role: &str) -> PgConnectOptions {
        self.postgres.options(role, self.database)
    }

    /// A plain connection (no session check), for setup and inspection.
    async fn connect(&self, role: &str) -> PgConnection {
        PgConnection::connect_with(&self.options(role))
            .await
            .unwrap_or_else(|error| panic!("connect as {role}: {error}"))
    }

    fn tenant_db(&self, max_connections: u32) -> TenantDb {
        TenantDb::connect_lazy(
            self.options("example_app"),
            PgPoolOptions::new().max_connections(max_connections),
        )
        .expect("verify-full options")
    }

    /// Item names per tenant as the owner sees them.
    async fn owner_view(&self) -> Vec<(String, String)> {
        let mut owner = self.connect("example_owner").await;
        sqlx::query_as("SELECT tenant_id::text, name FROM example_items ORDER BY tenant_id, name")
            .fetch_all(&mut owner)
            .await
            .expect("owner reads every row")
    }
}

/// A throwaway identity-service key and a verifier for tokens addressed to `assets`.
struct Identity {
    key: SigningKey,
    verifier: IdentityVerifier,
}

impl Identity {
    fn new() -> Self {
        let key = SigningKey::from_bytes(&[53; 32]);
        let verifier = IdentityVerifier::new(
            ServiceName::Assets,
            IdentityKeys::from_jwks(&Self::jwks_of(&key)).expect("key set"),
        );
        Self { key, verifier }
    }

    fn jwks_of(key: &SigningKey) -> String {
        let x = URL_SAFE_NO_PAD.encode(key.verifying_key().as_bytes());
        json!({"keys": [{"kty": "OKP", "crv": "Ed25519", "kid": "test", "x": x}]}).to_string()
    }

    fn jwks(&self) -> String {
        Self::jwks_of(&self.key)
    }

    /// A token for `tenant` (or none), as the identity service would sign it.
    fn token(&self, tenant: Option<&str>, platform_admin: bool) -> String {
        let iat = i64::try_from(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_secs(),
        )
        .expect("seconds");
        let claims = json!({
            "iss": "identity", "sub": USER, "aud": "assets", "iat": iat, "exp": iat + 60,
            "tenant": tenant.map(|id| json!({"id": id, "role": "editor"})),
            "platform_admin": platform_admin,
        });
        let header = json!({"alg": "EdDSA", "typ": "internal-identity+jwt", "kid": "test"});
        let signed = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(header.to_string()),
            URL_SAFE_NO_PAD.encode(claims.to_string())
        );
        let signature = self.key.sign(signed.as_bytes());
        format!("{signed}.{}", URL_SAFE_NO_PAD.encode(signature.to_bytes()))
    }

    /// A verified user, as a handler receives it.
    fn user(&self, tenant: Option<&str>, platform_admin: bool) -> UserIdentity {
        self.verifier
            .verify(&self.token(tenant, platform_admin))
            .expect("valid token")
    }
}

fn tenant(id: &str) -> TenantId {
    id.parse().expect("tenant id")
}

fn sqlstate(error: &sqlx::Error) -> Option<String> {
    error
        .as_database_error()
        .and_then(|error| error.code())
        .map(|code| code.into_owned())
}

/// Without a tenant set, the service role sees no rows of a tenant table and can write none.
#[tokio::test]
async fn without_a_tenant_no_rows_are_visible_or_writable() {
    let Some(fixture) = Fixture::new(
        "CREATE DATABASE without_tenant OWNER example_owner",
        "without_tenant",
    )
    .await
    else {
        return;
    };
    let db = fixture.tenant_db(2);
    let pool = db.pool();

    let outside: i64 = sqlx::query_scalar(COUNT_ITEMS)
        .fetch_one(pool)
        .await
        .expect("count");
    assert_eq!(outside, 0, "outside a transaction");

    let mut tx = pool.begin().await.expect("begin");
    let inside: i64 = sqlx::query_scalar(COUNT_ITEMS)
        .fetch_one(&mut *tx)
        .await
        .expect("count");
    assert_eq!(inside, 0, "in a transaction without a tenant");
    let by_tenant: i64 =
        sqlx::query_scalar("SELECT count(*) FROM example_items WHERE tenant_id = $1::uuid")
            .bind(TENANT_A)
            .fetch_one(&mut *tx)
            .await
            .expect("count");
    assert_eq!(by_tenant, 0, "even when asking for a tenant's rows");
    let updated = sqlx::query("UPDATE example_items SET name = 'changed'")
        .execute(&mut *tx)
        .await
        .expect("update");
    let deleted = sqlx::query("DELETE FROM example_items")
        .execute(&mut *tx)
        .await
        .expect("delete");
    assert_eq!((updated.rows_affected(), deleted.rows_affected()), (0, 0));
    tx.commit().await.expect("commit");

    let insert = sqlx::query(INSERT_ITEM)
        .bind(TENANT_A)
        .bind("00000000-0000-7000-8000-0000000000f1")
        .bind("no-tenant")
        .execute(pool)
        .await
        .expect_err("an insert without a tenant is refused");
    assert_eq!(
        sqlstate(&insert).as_deref(),
        Some(INSUFFICIENT_PRIVILEGE),
        "{insert}"
    );

    assert_eq!(fixture.owner_view().await.len(), 3, "no row changed");
    println!("without a tenant: 0 rows visible, update 0, delete 0, insert refused: {insert}");
}

/// Tenant A's transaction sees and changes only tenant A's rows, even when its SQL names tenant
/// B; it cannot insert rows into B or move its rows to B.
#[tokio::test]
async fn a_tenant_reads_and_writes_only_its_own_rows() {
    let Some(fixture) =
        Fixture::new("CREATE DATABASE isolation OWNER example_owner", "isolation").await
    else {
        return;
    };
    let db = fixture.tenant_db(2);
    let identity = Identity::new();
    let user_a = identity.user(Some(TENANT_A), false);

    let mut tx = db.begin(&user_a).await.expect("begin for tenant A");
    assert_eq!(tx.tenant_id(), tenant(TENANT_A));
    let names: Vec<String> = sqlx::query_scalar(ITEM_NAMES)
        .fetch_all(&mut *tx)
        .await
        .expect("names");
    assert_eq!(names, ["a-1", "a-2"], "no tenant condition, only A's rows");
    let of_b: Vec<String> =
        sqlx::query_scalar("SELECT name FROM example_items WHERE tenant_id = $1::uuid")
            .bind(TENANT_B)
            .fetch_all(&mut *tx)
            .await
            .expect("select B");
    assert!(of_b.is_empty(), "B's rows by tenant condition: {of_b:?}");
    let updated =
        sqlx::query("UPDATE example_items SET name = 'stolen' WHERE tenant_id = $1::uuid")
            .bind(TENANT_B)
            .execute(&mut *tx)
            .await
            .expect("update B");
    let deleted = sqlx::query("DELETE FROM example_items WHERE tenant_id = $1::uuid")
        .bind(TENANT_B)
        .execute(&mut *tx)
        .await
        .expect("delete B");
    assert_eq!((updated.rows_affected(), deleted.rows_affected()), (0, 0));
    sqlx::query(INSERT_ITEM)
        .bind(TENANT_A)
        .bind("00000000-0000-7000-8000-0000000000a3")
        .bind("a-3")
        .execute(&mut *tx)
        .await
        .expect("insert into A");
    tx.commit().await.expect("commit");

    // A statement that fails aborts the transaction, so each attempt gets its own.
    let mut tx = db.begin(&user_a).await.expect("begin");
    let insert_b = sqlx::query(INSERT_ITEM)
        .bind(TENANT_B)
        .bind("00000000-0000-7000-8000-0000000000b9")
        .bind("planted")
        .execute(&mut *tx)
        .await
        .expect_err("inserting into B is refused");
    assert_eq!(sqlstate(&insert_b).as_deref(), Some(INSUFFICIENT_PRIVILEGE));
    drop(tx);
    let mut tx = db.begin(&user_a).await.expect("begin");
    let move_to_b = sqlx::query("UPDATE example_items SET tenant_id = $1::uuid WHERE name = 'a-1'")
        .bind(TENANT_B)
        .execute(&mut *tx)
        .await
        .expect_err("moving a row to B is refused");
    assert_eq!(
        sqlstate(&move_to_b).as_deref(),
        Some(INSUFFICIENT_PRIVILEGE)
    );
    drop(tx);

    // Background work for tenant B sees B only, not the row A just added.
    let (_, background) = TenantDb::connect_lazy_with_background(
        fixture.options("example_app"),
        PgPoolOptions::new().max_connections(1),
        ServiceIdentity::new(ServiceName::Assets),
    )
    .expect("verify-full options");
    assert_eq!(background.actor().service(), ServiceName::Assets);
    let mut tx = background
        .begin(tenant(TENANT_B), "tenant-isolation-test")
        .await
        .expect("begin for tenant B");
    let names: Vec<String> = sqlx::query_scalar(ITEM_NAMES)
        .fetch_all(&mut *tx)
        .await
        .expect("names");
    assert_eq!(names, ["b-1"]);
    // The sqlx transaction handed to library code (an outbox handler, `outbox::publish`) is
    // the same transaction, still in tenant B.
    let current: Option<String> = sqlx::query_scalar(CURRENT_TENANT)
        .fetch_one(&mut **tx.transaction())
        .await
        .expect("tenant setting");
    assert_eq!(current.as_deref(), Some(TENANT_B));
    tx.rollback().await.expect("rollback");

    // Background work without a tenant (bookkeeping, platform-wide events) sees and writes no
    // tenant's rows.
    let mut tx = background
        .begin_without_tenant("no-tenant-test")
        .await
        .expect("begin without a tenant");
    let count: i64 = sqlx::query_scalar(COUNT_ITEMS)
        .fetch_one(&mut *tx)
        .await
        .expect("count");
    assert_eq!(count, 0, "no tenant, no rows");
    let insert_without_tenant = sqlx::query(INSERT_ITEM)
        .bind(TENANT_A)
        .bind("00000000-0000-7000-8000-0000000000a9")
        .bind("a-9")
        .execute(&mut *tx)
        .await
        .expect_err("no tenant, no writes");
    assert_eq!(
        sqlstate(&insert_without_tenant).as_deref(),
        Some(INSUFFICIENT_PRIVILEGE)
    );
    drop(tx);

    let no_tenant = db
        .begin(&identity.user(None, false))
        .await
        .expect_err("a user without a tenant gets no transaction");
    assert!(matches!(no_tenant, TenantDbError::NoTenant), "{no_tenant}");

    assert_eq!(
        fixture.owner_view().await,
        [
            (TENANT_A.to_owned(), "a-1".to_owned()),
            (TENANT_A.to_owned(), "a-2".to_owned()),
            (TENANT_A.to_owned(), "a-3".to_owned()),
            (TENANT_B.to_owned(), "b-1".to_owned()),
        ],
        "B's row is untouched; A's insert committed"
    );
    println!("tenant A: insert into B refused ({insert_b}); move to B refused ({move_to_b})");
}

/// The tenant is set by the kit as the transaction starts and ends with it: a pooled
/// connection's next transaction starts without it, after commit, rollback, drop, or an error.
#[tokio::test]
async fn the_tenant_setting_does_not_leak_across_pooled_connections() {
    let Some(fixture) =
        Fixture::new("CREATE DATABASE pooling OWNER example_owner", "pooling").await
    else {
        return;
    };
    // One connection, so every transaction below reuses the same session.
    let db = fixture.tenant_db(1);
    let identity = Identity::new();
    let user_a = identity.user(Some(TENANT_A), false);
    let user_b = identity.user(Some(TENANT_B), false);

    /// The backend, the tenant setting, and the visible rows of a transaction without a tenant.
    async fn untenanted(pool: &PgPool) -> (i32, Option<String>, i64) {
        let mut tx = pool.begin().await.expect("begin");
        let pid: i32 = sqlx::query_scalar(BACKEND_PID)
            .fetch_one(&mut *tx)
            .await
            .expect("pid");
        let setting: Option<String> = sqlx::query_scalar(CURRENT_TENANT)
            .fetch_one(&mut *tx)
            .await
            .expect("setting");
        let rows: i64 = sqlx::query_scalar(COUNT_ITEMS)
            .fetch_one(&mut *tx)
            .await
            .expect("count");
        tx.commit().await.expect("commit");
        (pid, setting.filter(|value| !value.is_empty()), rows)
    }

    // The first statement a tenant transaction runs already sees its tenant.
    let mut tx = db.begin(&user_a).await.expect("begin");
    let setting: Option<String> = sqlx::query_scalar(CURRENT_TENANT)
        .fetch_one(&mut *tx)
        .await
        .expect("setting");
    assert_eq!(setting.as_deref(), Some(TENANT_A));
    let pid: i32 = sqlx::query_scalar(BACKEND_PID)
        .fetch_one(&mut *tx)
        .await
        .expect("pid");
    tx.commit().await.expect("commit");
    assert_eq!(untenanted(db.pool()).await, (pid, None, 0), "after commit");

    let mut tx = db.begin(&user_a).await.expect("begin");
    sqlx::query(INSERT_ITEM)
        .bind(TENANT_A)
        .bind("00000000-0000-7000-8000-0000000000a4")
        .bind("rolled-back")
        .execute(&mut *tx)
        .await
        .expect("insert");
    tx.rollback().await.expect("rollback");
    assert_eq!(
        untenanted(db.pool()).await,
        (pid, None, 0),
        "after rollback"
    );

    let tx = db.begin(&user_a).await.expect("begin");
    drop(tx);
    assert_eq!(untenanted(db.pool()).await, (pid, None, 0), "after drop");

    let mut tx = db.begin(&user_a).await.expect("begin");
    sqlx::query("SELECT 1 / 0")
        .execute(&mut *tx)
        .await
        .expect_err("division by zero");
    drop(tx);
    assert_eq!(
        untenanted(db.pool()).await,
        (pid, None, 0),
        "after an error"
    );

    // The next tenant on the same session sees only its own rows.
    let mut tx = db.begin(&user_b).await.expect("begin");
    let same_pid: i32 = sqlx::query_scalar(BACKEND_PID)
        .fetch_one(&mut *tx)
        .await
        .expect("pid");
    let names: Vec<String> = sqlx::query_scalar(ITEM_NAMES)
        .fetch_all(&mut *tx)
        .await
        .expect("names");
    tx.commit().await.expect("commit");
    assert_eq!((same_pid, names), (pid, vec!["b-1".to_owned()]));
    assert_eq!(
        fixture.owner_view().await.len(),
        3,
        "the rollback left nothing"
    );
    println!("one pooled session (pid {pid}): no tenant after commit, rollback, drop, or error");
}

/// The service role is not the table owner, has no BYPASSRLS, and cannot turn row-level
/// security off or around.
#[tokio::test]
async fn the_service_role_cannot_bypass_row_level_security() {
    let Some(fixture) = Fixture::new("CREATE DATABASE roles OWNER example_owner", "roles").await
    else {
        return;
    };
    let mut app = fixture.connect("example_app").await;
    let (superuser, bypass): (bool, bool) =
        sqlx::query_as("SELECT rolsuper, rolbypassrls FROM pg_roles WHERE rolname = current_user")
            .fetch_one(&mut app)
            .await
            .expect("role attributes");
    assert!(
        !superuser && !bypass,
        "superuser {superuser}, bypassrls {bypass}"
    );
    let owner: String = sqlx::query_scalar(
        "SELECT tableowner::text FROM pg_tables WHERE tablename = 'example_items'",
    )
    .fetch_one(&mut app)
    .await
    .expect("owner");
    assert_eq!(owner, "example_owner");
    assert_eq!(
        postgres::check_session(&mut app, SessionRole::Service)
            .await
            .expect("check"),
        Ok(()),
        "the service role passes the session check"
    );

    for (attempt, sql) in [
        (
            "disable row-level security",
            "ALTER TABLE example_items DISABLE ROW LEVEL SECURITY",
        ),
        (
            "drop the policy",
            "DROP POLICY tenant_isolation ON example_items",
        ),
        (
            "add a permissive policy",
            "CREATE POLICY everything ON example_items USING (true)",
        ),
        ("truncate", "TRUNCATE example_items"),
        (
            "copy rows into a temporary table",
            "CREATE TEMPORARY TABLE kept_items AS SELECT * FROM example_items",
        ),
    ] {
        let error = sqlx::query(sql).execute(&mut app).await.expect_err(attempt);
        assert_eq!(
            sqlstate(&error).as_deref(),
            Some(INSUFFICIENT_PRIVILEGE),
            "{attempt}: {error}"
        );
        println!("{attempt}: refused ({error})");
    }
    // With row_security off, a query that the policy would filter fails instead of bypassing it.
    sqlx::query("SET row_security = off")
        .execute(&mut app)
        .await
        .expect("set row_security");
    let error = sqlx::query(COUNT_ITEMS)
        .execute(&mut app)
        .await
        .expect_err("row_security off does not bypass the policy");
    assert_eq!(
        sqlstate(&error).as_deref(),
        Some(INSUFFICIENT_PRIVILEGE),
        "{error}"
    );
    println!("row_security = off: refused ({error})");
    assert_eq!(fixture.owner_view().await.len(), 3);
}

/// The kit refuses sessions that row-level security would not protect, before any query runs.
#[tokio::test]
async fn sessions_row_level_security_would_not_protect_are_refused() {
    let Some(fixture) = Fixture::new("CREATE DATABASE guard OWNER example_owner", "guard").await
    else {
        return;
    };
    async fn check_as(options: &PgConnectOptions, role: SessionRole) -> Result<(), UnsafeSession> {
        let mut connection = PgConnection::connect_with(options).await.expect("connect");
        postgres::check_session(&mut connection, role)
            .await
            .expect("the check runs")
    }
    async fn check(options: &PgConnectOptions) -> Result<(), UnsafeSession> {
        check_as(options, SessionRole::Service).await
    }
    let item_table = "public.example_items".to_owned();
    assert_eq!(check(&fixture.options("example_app")).await, Ok(()));
    assert_eq!(
        check_as(&fixture.options("example_platform"), SessionRole::Platform).await,
        Ok(())
    );
    // The policy that opens the table to the platform role is refused for a service session.
    assert_eq!(
        check(&fixture.options("example_platform")).await,
        Err(UnsafeSession::PermissivePolicy(
            "public.example_items policy platform_access".to_owned()
        ))
    );
    assert_eq!(
        check(&fixture.postgres.superuser("guard")).await,
        Err(UnsafeSession::BypassesRls)
    );
    assert_eq!(
        check(&fixture.options("example_bypass")).await,
        Err(UnsafeSession::BypassesRls)
    );
    assert_eq!(
        check(&fixture.options("example_owner")).await,
        Err(UnsafeSession::OwnsRelation(item_table.clone()))
    );
    assert_eq!(
        check(&fixture.options("example_owner_member")).await,
        Err(UnsafeSession::OwnsRelation(item_table.clone()))
    );
    assert_eq!(
        check(
            &fixture
                .options("example_app")
                .options([("trustiot.tenant_id", TENANT_A)])
        )
        .await,
        Err(UnsafeSession::TenantPreset)
    );

    // Each migration mistake is refused until the migration fixes it.
    let mut owner = fixture.connect("example_owner").await;
    // The platform role's session passes only policies granted to it by name: one it gets
    // through a group it belongs to is refused.
    sqlx::raw_sql(
        "CREATE POLICY group_access ON example_items TO example_platform_group USING (true);",
    )
    .execute(&mut owner)
    .await
    .expect("a policy for a group of the platform role");
    let refused = check_as(&fixture.options("example_platform"), SessionRole::Platform).await;
    assert_eq!(
        refused,
        Err(UnsafeSession::PermissivePolicy(
            "public.example_items policy group_access".to_owned()
        ))
    );
    sqlx::raw_sql("DROP POLICY group_access ON example_items;")
        .execute(&mut owner)
        .await
        .expect("drop the group policy");
    assert_eq!(
        check_as(&fixture.options("example_platform"), SessionRole::Platform).await,
        Ok(())
    );
    println!("a policy the platform role inherits from a group: refused with {refused:?}");
    let mistakes: [(&str, &str, UnsafeSession, &str); 9] = [
        (
            "a tenant table without row-level security",
            "CREATE TABLE loose_items (tenant_id uuid NOT NULL, name text);
             GRANT SELECT ON loose_items TO example_app;",
            UnsafeSession::UnprotectedTable("public.loose_items".to_owned()),
            "ALTER TABLE loose_items ENABLE ROW LEVEL SECURITY;",
        ),
        (
            "a view with its owner's rights",
            "CREATE VIEW item_names AS SELECT tenant_id, name FROM example_items;
             GRANT SELECT ON item_names TO example_app;",
            UnsafeSession::UnprotectedView("public.item_names".to_owned()),
            "ALTER VIEW item_names SET (security_invoker = true);",
        ),
        (
            "a materialized view",
            "CREATE MATERIALIZED VIEW item_counts AS
                 SELECT tenant_id, count(*) AS items FROM example_items GROUP BY tenant_id;
             GRANT SELECT ON item_counts TO example_app;",
            UnsafeSession::Unprotectable("public.item_counts".to_owned()),
            "REVOKE SELECT ON item_counts FROM example_app;",
        ),
        (
            "a permissive policy for every role",
            "CREATE POLICY everything ON example_items FOR SELECT USING (true);",
            UnsafeSession::PermissivePolicy("public.example_items policy everything".to_owned()),
            "DROP POLICY everything ON example_items;",
        ),
        (
            "a widened tenant policy for the service role",
            "CREATE POLICY widened ON example_items TO example_app
                 USING (tenant_id = NULLIF(current_setting('trustiot.tenant_id', true), '')::uuid
                        OR name = 'shared');",
            UnsafeSession::PermissivePolicy("public.example_items policy widened".to_owned()),
            "DROP POLICY widened ON example_items;",
        ),
        (
            "a view of tenant data without the tenant column",
            "CREATE VIEW all_names AS SELECT name FROM example_items;
             CREATE VIEW all_names_again AS SELECT name FROM all_names;
             GRANT SELECT ON all_names_again TO example_app;",
            UnsafeSession::UnprotectedView("public.all_names_again".to_owned()),
            "ALTER VIEW all_names_again SET (security_invoker = true);",
        ),
        (
            "a SECURITY DEFINER function over tenant data",
            "CREATE FUNCTION all_item_names() RETURNS SETOF text LANGUAGE sql SECURITY DEFINER
                 AS 'SELECT name FROM example_items';",
            UnsafeSession::DefinerFunction("all_item_names()".to_owned()),
            "REVOKE EXECUTE ON FUNCTION all_item_names() FROM PUBLIC;",
        ),
        (
            "TRUNCATE on a tenant table",
            "GRANT TRUNCATE ON example_items TO example_app;",
            UnsafeSession::TruncateAllowed(item_table.clone()),
            "REVOKE TRUNCATE ON example_items FROM example_app;",
        ),
        (
            "temporary tables",
            "DO $$ BEGIN
                 EXECUTE format('GRANT TEMPORARY ON DATABASE %I TO example_app', current_database());
             END $$;",
            UnsafeSession::TemporaryTables,
            "DO $$ BEGIN
                 EXECUTE format('REVOKE TEMPORARY ON DATABASE %I FROM example_app', current_database());
             END $$;",
        ),
    ];
    for (mistake, make, refused, fix) in mistakes {
        sqlx::raw_sql(make)
            .execute(&mut owner)
            .await
            .unwrap_or_else(|error| panic!("{mistake}: {error}"));
        assert_eq!(
            check(&fixture.options("example_app")).await,
            Err(refused.clone()),
            "{mistake}"
        );
        sqlx::raw_sql(fix)
            .execute(&mut owner)
            .await
            .unwrap_or_else(|error| panic!("{mistake}: {error}"));
        assert_eq!(
            check(&fixture.options("example_app")).await,
            Ok(()),
            "{mistake} fixed"
        );
        println!("{mistake}: refused with \"{refused}\", accepted once fixed");
    }

    // A SECURITY DEFINER function of the platform role reads across tenants through its
    // platform_access policy, so the service role must not be able to call it either.
    let mut superuser = PgConnection::connect_with(&fixture.postgres.superuser("guard"))
        .await
        .expect("connect as the superuser");
    sqlx::raw_sql(
        "CREATE FUNCTION platform_names() RETURNS SETOF text LANGUAGE sql SECURITY DEFINER
             AS 'SELECT name FROM example_items';
         ALTER FUNCTION platform_names() OWNER TO example_platform;",
    )
    .execute(&mut superuser)
    .await
    .expect("a definer function of the platform role");
    let refused = check(&fixture.options("example_app")).await;
    assert_eq!(
        refused,
        Err(UnsafeSession::DefinerFunction(
            "platform_names()".to_owned()
        ))
    );
    sqlx::raw_sql("REVOKE EXECUTE ON FUNCTION platform_names() FROM PUBLIC;")
        .execute(&mut superuser)
        .await
        .expect("revoke");
    assert_eq!(check(&fixture.options("example_app")).await, Ok(()));
    println!(
        "a platform role's SECURITY DEFINER function: refused with {refused:?}, accepted once revoked"
    );

    // Through the kit's pool, a refused session never reaches the caller: sqlx retries until
    // the acquire timeout, and the kit reports the refusal.
    let as_owner = TenantDb::connect_lazy(
        fixture.options("example_owner"),
        PgPoolOptions::new().acquire_timeout(Duration::from_secs(2)),
    )
    .expect("verify-full options");
    let error = as_owner
        .begin(&Identity::new().user(Some(TENANT_A), false))
        .await
        .expect_err("the owner's session is refused");
    assert!(
        matches!(
            &error,
            TenantDbError::UnsafeSession(UnsafeSession::OwnsRelation(_))
        ),
        "{error}"
    );
    assert!(as_owner.ping().await.is_err(), "readiness fails too");
}

/// Connections verify the server's certificate chain and host name; options that would not
/// are refused before connecting.
#[tokio::test]
async fn connections_verify_the_server_certificate_and_host_name() {
    let Some(fixture) = Fixture::new("CREATE DATABASE tls OWNER example_owner", "tls").await else {
        return;
    };
    let mut connection = fixture.connect("example_app").await;
    let encrypted: bool =
        sqlx::query_scalar("SELECT ssl FROM pg_stat_ssl WHERE pid = pg_backend_pid()")
            .fetch_one(&mut connection)
            .await
            .expect("ssl status");
    assert!(encrypted, "the session uses TLS");

    let other_ca = TestCa::new("unrelated CA");
    let wrong_ca = fixture
        .options("example_app")
        .ssl_root_cert_from_pem(other_ca.pem.into_bytes());
    let error = PgConnection::connect_with(&wrong_ca)
        .await
        .expect_err("a certificate from another CA is refused");
    println!("unrelated CA: {error}");
    assert!(error.to_string().contains("UnknownIssuer"), "{error}");

    // The certificate names 127.0.0.1 only; "localhost" reaches the same server.
    assert_eq!(SERVER_HOST, "127.0.0.1");
    let wrong_name = fixture.options("example_app").host("localhost");
    let error = PgConnection::connect_with(&wrong_name)
        .await
        .expect_err("a host name the certificate does not name is refused");
    println!("host name mismatch: {error}");
    assert!(error.to_string().contains("not valid for name"), "{error}");

    for mode in [
        PgSslMode::Disable,
        PgSslMode::Allow,
        PgSslMode::Prefer,
        PgSslMode::Require,
        PgSslMode::VerifyCa,
    ] {
        let options = fixture.options("example_app").ssl_mode(mode);
        assert_eq!(
            TenantDb::connect_lazy(options.clone(), PgPoolOptions::new()).err(),
            Some(InsecureConnection::SslMode),
            "{mode:?}"
        );
        assert_eq!(
            PlatformDb::connect_lazy(options, PgPoolOptions::new()).err(),
            Some(InsecureConnection::SslMode),
            "{mode:?}"
        );
    }
}

/// The platform role sees every tenant of the tables whose template includes
/// `platform_access`, and nothing of the others.
#[tokio::test]
async fn the_platform_role_reads_across_tenants_only_where_allowed() {
    let Some(fixture) =
        Fixture::new("CREATE DATABASE platform OWNER example_owner", "platform").await
    else {
        return;
    };
    let mut owner = fixture.connect("example_owner").await;
    sqlx::raw_sql(
        "CREATE TABLE private_items (tenant_id uuid NOT NULL, name text NOT NULL);
         ALTER TABLE private_items ENABLE ROW LEVEL SECURITY;
         CREATE POLICY tenant_isolation ON private_items
             USING (tenant_id = NULLIF(current_setting('trustiot.tenant_id', true), '')::uuid)
             WITH CHECK (tenant_id = NULLIF(current_setting('trustiot.tenant_id', true), '')::uuid);
         GRANT SELECT ON private_items TO example_app, example_platform;
         INSERT INTO private_items VALUES ('01928f6e-7a3c-7d4e-8f00-00000000000a', 'secret');",
    )
    .execute(&mut owner)
    .await
    .expect("a table without platform access");

    let identity = Identity::new();
    assert_eq!(
        PlatformActor::admin(&identity.user(Some(TENANT_A), false)),
        None,
        "a user who is not a platform administrator is no platform actor"
    );
    let admin = PlatformActor::admin(&identity.user(None, true)).expect("platform admin");

    let platform = PlatformDb::connect_lazy(
        fixture.options("example_platform"),
        PgPoolOptions::new().max_connections(1),
    )
    .expect("verify-full options");
    platform
        .ping()
        .await
        .expect("the platform role passes the session check");
    for actor in [
        admin,
        PlatformActor::service(ServiceIdentity::new(ServiceName::Assets)),
    ] {
        let mut tx = platform
            .begin(actor, "cross-tenant-test")
            .await
            .expect("begin");
        assert_eq!(tx.actor(), actor);
        let names: Vec<String> = sqlx::query_scalar(ITEM_NAMES)
            .fetch_all(&mut *tx)
            .await
            .expect("names");
        assert_eq!(names, ["a-1", "a-2", "b-1"], "every tenant");
        let private: i64 = sqlx::query_scalar("SELECT count(*) FROM private_items")
            .fetch_one(&mut *tx)
            .await
            .expect("count");
        assert_eq!(private, 0, "no platform_access policy, no rows");
        let error = sqlx::query("DELETE FROM example_items")
            .execute(&mut *tx)
            .await
            .expect_err("the template grants the platform role no DELETE");
        assert_eq!(sqlstate(&error).as_deref(), Some(INSUFFICIENT_PRIVILEGE));
        tx.rollback().await.expect("rollback");
    }

    // A reader's transactions see the same rows but are read-only: the template grants the
    // platform role UPDATE, and the database still refuses it.
    let reader = platform.reader(
        PlatformActor::service(ServiceIdentity::new(ServiceName::Assets)),
        "reader-test",
    );
    assert_eq!(reader.purpose(), "reader-test");
    let mut tx = reader.begin().await.expect("begin a read");
    assert_eq!(tx.actor(), reader.actor());
    let names: Vec<String> = sqlx::query_scalar(ITEM_NAMES)
        .fetch_all(&mut *tx)
        .await
        .expect("names");
    assert_eq!(names, ["a-1", "a-2", "b-1"], "every tenant");
    let error = sqlx::query("UPDATE example_items SET name = name")
        .execute(&mut *tx)
        .await
        .expect_err("a reader cannot write");
    assert_eq!(
        sqlstate(&error).as_deref(),
        Some("25006"),
        "read_only_sql_transaction"
    );
    tx.rollback().await.expect("rollback");

    // The service role still sees only its tenant there.
    let db = fixture.tenant_db(1);
    let mut tx = db
        .begin(&identity.user(Some(TENANT_B), false))
        .await
        .expect("begin");
    let private: i64 = sqlx::query_scalar("SELECT count(*) FROM private_items")
        .fetch_one(&mut *tx)
        .await
        .expect("count");
    assert_eq!(private, 0);
    tx.rollback().await.expect("rollback");
}

/// The outbox template (`sql/outbox-event.sql`): the service role writes events of its tenant
/// only, platform-wide events come only from the platform role, and a consumer's platform role
/// reads every tenant's events.
#[tokio::test]
async fn outbox_events_follow_the_tenant_policies() {
    let Some(fixture) = Fixture::new("CREATE DATABASE outbox OWNER example_owner", "outbox").await
    else {
        return;
    };
    // The columns of crates/outbox's outbox_event that row-level security depends on.
    let mut owner = fixture.connect("example_owner").await;
    sqlx::raw_sql(
        "CREATE TABLE outbox_event (
             id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
             tenant_id uuid,
             envelope jsonb NOT NULL
         );",
    )
    .execute(&mut owner)
    .await
    .expect("outbox table");
    sqlx::raw_sql(include_str!("../sql/outbox-event.sql"))
        .execute(&mut owner)
        .await
        .expect("run the outbox template");
    const PUBLISH: &str = "INSERT INTO outbox_event (tenant_id, envelope) VALUES ($1::uuid, '{}')";

    let identity = Identity::new();
    let db = fixture.tenant_db(1);
    db.ping()
        .await
        .expect("the producer's service role passes the session check");
    let mut tx = db
        .begin(&identity.user(Some(TENANT_A), false))
        .await
        .expect("begin");
    sqlx::query(PUBLISH)
        .bind(TENANT_A)
        .execute(&mut *tx)
        .await
        .expect("an event of the transaction's tenant");
    tx.commit().await.expect("commit");
    for (case, tenant) in [("another tenant", Some(TENANT_B)), ("platform-wide", None)] {
        let mut tx = db
            .begin(&identity.user(Some(TENANT_A), false))
            .await
            .expect("begin");
        let error = sqlx::query(PUBLISH)
            .bind(tenant)
            .execute(&mut *tx)
            .await
            .expect_err(case);
        assert_eq!(
            sqlstate(&error).as_deref(),
            Some(INSUFFICIENT_PRIVILEGE),
            "{case}: {error}"
        );
        println!("service role, {case} event: refused ({error})");
    }

    let platform = PlatformDb::connect_lazy(
        fixture.options("example_platform"),
        PgPoolOptions::new().max_connections(1),
    )
    .expect("verify-full options");
    let actor = PlatformActor::service(ServiceIdentity::new(ServiceName::Identity));
    let mut tx = platform.begin(actor, "outbox-test").await.expect("begin");
    sqlx::query(PUBLISH)
        .bind(None::<&str>)
        .execute(&mut *tx)
        .await
        .expect("a platform-wide event");
    tx.commit().await.expect("commit");
    let mut tx = platform.begin(actor, "outbox-test").await.expect("begin");
    let error = sqlx::query(PUBLISH)
        .bind(TENANT_B)
        .execute(&mut *tx)
        .await
        .expect_err("the platform role writes no tenant's events");
    assert_eq!(sqlstate(&error).as_deref(), Some(INSUFFICIENT_PRIVILEGE));
    drop(tx);

    let reader = PlatformDb::connect_lazy(
        fixture.options("example_reader"),
        PgPoolOptions::new().max_connections(1),
    )
    .expect("verify-full options");
    let mut tx = reader
        .begin(
            PlatformActor::service(ServiceIdentity::new(ServiceName::Assets)),
            "outbox-delivery",
        )
        .await
        .expect("begin");
    let tenants: Vec<Option<String>> =
        sqlx::query_scalar("SELECT tenant_id::text FROM outbox_event ORDER BY id")
            .fetch_all(&mut *tx)
            .await
            .expect("read events");
    tx.rollback().await.expect("rollback");
    assert_eq!(tenants, [Some(TENANT_A.to_owned()), None]);

    // The consumer must not read as a service role: a policy that opens the table to it makes
    // its tenant sessions unsafe.
    let mut reader_session = fixture.connect("example_reader").await;
    assert_eq!(
        postgres::check_session(&mut reader_session, SessionRole::Service)
            .await
            .expect("check"),
        Err(UnsafeSession::PermissivePolicy(
            "public.outbox_event policy consumer_read".to_owned()
        ))
    );
    println!(
        "outbox: tenant events by the service role, platform-wide by the platform role, all \
         read by the consumer's platform role"
    );
}

/// The example service refuses to start when its database settings do not verify the server,
/// and the error names the variable without repeating its value.
#[test]
fn a_service_without_server_verification_refuses_to_start() {
    for sslmode in ["disable", "allow", "prefer", "require", "verify-ca", ""] {
        let (mut child, stdout, _stderr) = spawn_example(&[
            ("TRUSTIOT__DATABASE__USERNAME", "example_app"),
            ("TRUSTIOT__DATABASE__PASSWORD", "service-kit-test"),
            ("TRUSTIOT_POSTGRES_HOST", "postgres.example.internal"),
            ("TRUSTIOT_POSTGRES_PORT", "5432"),
            ("TRUSTIOT_POSTGRES_DATABASE", "trustiot"),
            ("TRUSTIOT_POSTGRES_SSLMODE", sslmode),
        ]);
        let status =
            wait_exit(&mut child, Duration::from_secs(10)).expect("the service exits by itself");
        assert_eq!(status.code(), Some(1), "sslmode {sslmode:?}");
        let error = stdout
            .wait_for_message(
                "invalid configuration; not starting",
                Duration::from_secs(2),
            )
            .unwrap_or_else(|| panic!("sslmode {sslmode:?}: the error is logged"));
        let text = error["fields"]["error"].as_str().expect("error text");
        assert!(text.contains("platform.postgres_sslmode"), "{text}");
        assert!(
            text.contains("TRUSTIOT_POSTGRES_SSLMODE") || sslmode.is_empty(),
            "{text}"
        );
        if !sslmode.is_empty() {
            assert!(text.contains("only `verify-full` is accepted"), "{text}");
            assert!(!text.contains(sslmode), "the value is repeated: {text}");
        }
        assert!(
            !stdout.text().contains("service-kit-test"),
            "the password is logged"
        );
        println!("sslmode {sslmode:?}: {status}; error: {text}");
    }
}

/// End to end: the example service connects with verify-full, and each request's tenant comes
/// from its verified token; the handler's query has no tenant condition.
#[tokio::test]
async fn the_example_service_serves_each_user_only_their_tenant() {
    let Some(fixture) =
        Fixture::new("CREATE DATABASE service OWNER example_owner", "service").await
    else {
        return;
    };
    let ca_file =
        std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(support::unique_name("ca.crt"));
    std::fs::write(&ca_file, &fixture.postgres.ca_pem).expect("write the CA file");
    let other_ca_file = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(support::unique_name("other-ca.crt"));
    std::fs::write(&other_ca_file, TestCa::new("unrelated CA").pem).expect("write the CA file");
    let identity = Identity::new();
    let jwks = identity.jwks();
    let port = fixture.postgres.address.port().to_string();
    let env = |ca_file: &std::path::Path| {
        vec![
            ("TRUSTIOT_INTERNAL_IDENTITY_KEYS".to_owned(), jwks.clone()),
            (
                "TRUSTIOT__DATABASE__USERNAME".to_owned(),
                "example_app".to_owned(),
            ),
            (
                "TRUSTIOT__DATABASE__PASSWORD".to_owned(),
                "service-kit-test".to_owned(),
            ),
            ("TRUSTIOT_POSTGRES_HOST".to_owned(), SERVER_HOST.to_owned()),
            ("TRUSTIOT_POSTGRES_PORT".to_owned(), port.clone()),
            (
                "TRUSTIOT_POSTGRES_DATABASE".to_owned(),
                "service".to_owned(),
            ),
            (
                "TRUSTIOT_POSTGRES_SSLMODE".to_owned(),
                "verify-full".to_owned(),
            ),
            (
                "TRUSTIOT_POSTGRES_CA_FILE".to_owned(),
                ca_file.display().to_string(),
            ),
        ]
    };
    let start = |env: Vec<(String, String)>| {
        tokio::task::spawn_blocking(move || {
            let env: Vec<(&str, &str)> = env
                .iter()
                .map(|(name, value)| (name.as_str(), value.as_str()))
                .collect();
            Service::start(&env)
        })
    };

    // The wrong CA: the service starts, but never becomes ready.
    let untrusted = start(env(&other_ca_file)).await.expect("start");
    let address = untrusted.address;
    let readiness = tokio::task::spawn_blocking(move || {
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut last = 0;
        while Instant::now() < deadline {
            last = support::get(address, "/readyz").expect("readyz").status;
            std::thread::sleep(Duration::from_millis(200));
        }
        last
    })
    .await
    .expect("poll");
    assert_eq!(
        readiness, 503,
        "not ready with an untrusted server certificate"
    );
    drop(untrusted);

    let service = start(env(&ca_file)).await.expect("start");
    let address = service.address;
    let tokens = [
        identity.token(Some(TENANT_A), false),
        identity.token(Some(TENANT_B), false),
        identity.token(None, false),
    ];
    let responses = tokio::task::spawn_blocking(move || {
        let deadline = Instant::now() + Duration::from_secs(20);
        while support::get(address, "/readyz").expect("readyz").status != 200 {
            assert!(
                Instant::now() < deadline,
                "the service did not become ready"
            );
            std::thread::sleep(Duration::from_millis(100));
        }
        tokens
            .iter()
            .map(|token| {
                let response = request(
                    address,
                    "GET",
                    "/v1/items",
                    &[("x-internal-identity", token)],
                    None,
                )
                .expect("GET /v1/items");
                (response.status, response.json())
            })
            .collect::<Vec<(u16, Value)>>()
    })
    .await
    .expect("requests");
    assert_eq!(
        responses,
        [
            (200, json!({"tenant": TENANT_A, "items": ["a-1", "a-2"]})),
            (200, json!({"tenant": TENANT_B, "items": ["b-1"]})),
            (
                403,
                json!({"code": "permission_denied", "message_key": "error.permission_denied"})
            ),
        ]
    );
    for (status, body) in &responses {
        println!("GET /v1/items: {status} {body}");
    }
    drop(service);
    let _ = std::fs::remove_file(ca_file);
    let _ = std::fs::remove_file(other_ca_file);
}
