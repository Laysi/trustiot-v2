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
use support::{OUTPUT_DRAIN_TIMEOUT, Output, Service, request, spawn_example, wait_exit};

const TENANT_A: &str = "01928f6e-7a3c-7d4e-8f00-00000000000a";
const TENANT_B: &str = "01928f6e-7a3c-7d4e-8f00-00000000000b";
const USER: &str = "01928f6e-7a3c-7d4e-8f00-0123456789ab";

/// The roles of decision 29, created once per Postgres. `example_reader` stands for the
/// platform role of a service that consumes `example_app`'s outbox events. `example_bypass` and
/// `example_owner_member` exist only to prove the session check refuses them. The platform role
/// inherits the rights of `example_platform_group`, and can take those of
/// `example_platform_switch` only with `SET ROLE`; `example_platform_switch` belongs to
/// `example_switch_parent` without inheriting its rights. The platform role inherits the rights
/// of `example_platform_inherited` but cannot `SET ROLE` to it, and can neither inherit nor take
/// those of `example_platform_unusable`. The service role has the same kinds of memberships
/// (Laysi/trustiot-v2#381): it can become `example_app_switch` and `example_alias` only with
/// `SET ROLE`; through `example_app_switch` also `example_app_switch_parent` (a chain of
/// `SET` memberships), and `example_app_switch` inherits `example_app_switch_group`, which the
/// service role can neither inherit nor become; it inherits `example_app_group` and through it
/// can become `example_app_group_switch` only with `SET ROLE`; it inherits
/// `example_app_inherited` without being able to become it, and through it can neither inherit
/// nor become `example_app_inherited_switch`; and it can neither inherit nor become
/// `example_app_unusable`. `example_administrator` only holds the ADMIN option on
/// `example_app_switch`. None of them has rights unless a test grants them.
const ROLES: &str = "
CREATE ROLE example_owner LOGIN PASSWORD 'service-kit-test';
CREATE ROLE example_app LOGIN PASSWORD 'service-kit-test';
CREATE ROLE example_platform LOGIN PASSWORD 'service-kit-test';
CREATE ROLE example_reader LOGIN PASSWORD 'service-kit-test';
CREATE ROLE example_bypass LOGIN BYPASSRLS PASSWORD 'service-kit-test';
CREATE ROLE example_owner_member LOGIN PASSWORD 'service-kit-test' IN ROLE example_owner;
CREATE ROLE example_platform_group NOLOGIN;
GRANT example_platform_group TO example_platform;
CREATE ROLE example_platform_switch NOLOGIN;
GRANT example_platform_switch TO example_platform WITH INHERIT FALSE;
CREATE ROLE example_switch_parent NOLOGIN;
GRANT example_switch_parent TO example_platform_switch WITH INHERIT FALSE;
CREATE ROLE example_platform_inherited NOLOGIN;
GRANT example_platform_inherited TO example_platform WITH INHERIT TRUE, SET FALSE;
CREATE ROLE example_platform_unusable NOLOGIN;
GRANT example_platform_unusable TO example_platform WITH INHERIT FALSE, SET FALSE;
CREATE ROLE example_app_switch NOLOGIN;
GRANT example_app_switch TO example_app WITH INHERIT FALSE, SET TRUE;
CREATE ROLE example_alias NOLOGIN;
GRANT example_alias TO example_app WITH INHERIT FALSE, SET TRUE;
CREATE ROLE example_app_switch_parent NOLOGIN;
GRANT example_app_switch_parent TO example_app_switch WITH INHERIT FALSE, SET TRUE;
CREATE ROLE example_app_switch_group NOLOGIN;
GRANT example_app_switch_group TO example_app_switch WITH INHERIT TRUE, SET FALSE;
CREATE ROLE example_app_group NOLOGIN;
GRANT example_app_group TO example_app;
CREATE ROLE example_app_group_switch NOLOGIN;
GRANT example_app_group_switch TO example_app_group WITH INHERIT FALSE, SET TRUE;
CREATE ROLE example_app_inherited NOLOGIN;
GRANT example_app_inherited TO example_app WITH INHERIT TRUE, SET FALSE;
CREATE ROLE example_app_inherited_switch NOLOGIN;
GRANT example_app_inherited_switch TO example_app_inherited WITH INHERIT FALSE, SET TRUE;
CREATE ROLE example_app_unusable NOLOGIN;
GRANT example_app_unusable TO example_app WITH INHERIT FALSE, SET FALSE;
CREATE ROLE example_administrator LOGIN PASSWORD 'service-kit-test';
GRANT example_app_switch TO example_administrator WITH ADMIN TRUE, INHERIT FALSE, SET FALSE;
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

/// The pool acquire timeouts, in seconds, that a test tries one after the other while it waits
/// for the kit to report a refused session. Opening a session (TLS handshake, then the session
/// check) takes a few hundred milliseconds on an idle machine and several seconds on a busy
/// one, and a pool that times out before its first session was checked has no refusal to
/// report yet. The first budget is what an idle machine needs; each retry doubles it, so the
/// wait is as long as the machine needs, up to a minute in all.
const ACQUIRE_BUDGETS_SECONDS: [u64; 5] = [2, 4, 8, 16, 32];

/// How long the example service may need to become ready. Its readiness check has to open a
/// database session within 800 ms (`CHECK_TIMEOUT` in `health.rs`), so a busy machine can take
/// many attempts; an idle one needs one. The wait ends at once when the service is ready or
/// stops answering.
const READY_BUDGET: Duration = Duration::from_secs(120);

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

    // A session-level tenant (`set_config(..., false)`) outlives the transaction. The pool
    // closes such a session when it comes back instead of handing it to the next user
    // (Laysi/trustiot-v2#271, verification F-1): inside a committed tenant transaction, and
    // straight on the pool.
    let mut tx = db.begin(&user_a).await.expect("begin");
    sqlx::query(SET_SESSION_TENANT)
        .bind(TENANT_B)
        .execute(&mut *tx)
        .await
        .expect("session-level tenant");
    tx.commit().await.expect("commit");
    let (after_tx, setting, rows) = untenanted(db.pool()).await;
    assert_ne!(after_tx, pid, "the leaking session was closed");
    assert_eq!((setting, rows), (None, 0), "the next session has no tenant");

    sqlx::query(SET_SESSION_TENANT)
        .bind(TENANT_B)
        .execute(db.pool())
        .await
        .expect("session-level tenant");
    let (after_pool, setting, rows) = untenanted(db.pool()).await;
    assert_ne!(after_pool, after_tx, "the leaking session was closed");
    assert_eq!((setting, rows), (None, 0), "the next session has no tenant");
    println!(
        "a session left with a session-level tenant is closed on release (pids {pid}, \
         {after_tx}, {after_pool})"
    );
}

/// Sets the session's tenant beyond the transaction, as nothing may: the pool must not hand
/// such a session to anyone.
const SET_SESSION_TENANT: &str = "SELECT set_config('trustiot.tenant_id', $1, false)";

/// Sets a tenant for the rest of the transaction outside service-kit's tenant transactions, as
/// nothing may: a platform role that does so must still gain nothing (Laysi/trustiot-v2#360).
const SET_TRANSACTION_TENANT: &str = "SELECT set_config('trustiot.tenant_id', $1, true)";

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
    let app = "example_app".to_owned();
    assert_eq!(check(&fixture.options("example_app")).await, Ok(()));
    assert_eq!(
        check_as(&fixture.options("example_platform"), SessionRole::Platform).await,
        Ok(())
    );
    // The policy that opens the table to the platform role is refused for a service session.
    assert_eq!(
        check(&fixture.options("example_platform")).await,
        Err(UnsafeSession::PermissivePolicy {
            policy: "public.example_items policy platform_access".to_owned(),
            role: "example_platform".to_owned(),
        })
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
        Err(UnsafeSession::PermissivePolicy {
            policy: "public.example_items policy group_access".to_owned(),
            role: "example_platform".to_owned(),
        })
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
            UnsafeSession::UnprotectedTable {
                table: "public.loose_items".to_owned(),
                role: app.clone(),
            },
            "ALTER TABLE loose_items ENABLE ROW LEVEL SECURITY;",
        ),
        (
            "a view with its owner's rights",
            "CREATE VIEW item_names AS SELECT tenant_id, name FROM example_items;
             GRANT SELECT ON item_names TO example_app;",
            UnsafeSession::UnprotectedView {
                view: "public.item_names".to_owned(),
                role: app.clone(),
            },
            "ALTER VIEW item_names SET (security_invoker = true);",
        ),
        (
            "a materialized view",
            "CREATE MATERIALIZED VIEW item_counts AS
                 SELECT tenant_id, count(*) AS items FROM example_items GROUP BY tenant_id;
             GRANT SELECT ON item_counts TO example_app;",
            UnsafeSession::Unprotectable {
                relation: "public.item_counts".to_owned(),
                role: app.clone(),
            },
            "REVOKE SELECT ON item_counts FROM example_app;",
        ),
        (
            "a permissive policy for every role",
            "CREATE POLICY everything ON example_items FOR SELECT USING (true);",
            UnsafeSession::PermissivePolicy {
                policy: "public.example_items policy everything".to_owned(),
                role: app.clone(),
            },
            "DROP POLICY everything ON example_items;",
        ),
        (
            "a widened tenant policy for the service role",
            "CREATE POLICY widened ON example_items TO example_app
                 USING (tenant_id = NULLIF(current_setting('trustiot.tenant_id', true), '')::uuid
                        OR name = 'shared');",
            UnsafeSession::PermissivePolicy {
                policy: "public.example_items policy widened".to_owned(),
                role: app.clone(),
            },
            "DROP POLICY widened ON example_items;",
        ),
        (
            "a view of tenant data without the tenant column",
            "CREATE VIEW all_names AS SELECT name FROM example_items;
             CREATE VIEW all_names_again AS SELECT name FROM all_names;
             GRANT SELECT ON all_names_again TO example_app;",
            UnsafeSession::UnprotectedView {
                view: "public.all_names_again".to_owned(),
                role: app.clone(),
            },
            "ALTER VIEW all_names_again SET (security_invoker = true);",
        ),
        (
            "a SECURITY DEFINER function over tenant data",
            "CREATE FUNCTION all_item_names() RETURNS SETOF text LANGUAGE sql SECURITY DEFINER
                 AS 'SELECT name FROM example_items';",
            UnsafeSession::DefinerFunction {
                function: "all_item_names()".to_owned(),
                role: app.clone(),
            },
            "REVOKE EXECUTE ON FUNCTION all_item_names() FROM PUBLIC;",
        ),
        (
            "TRUNCATE on a tenant table",
            "GRANT TRUNCATE ON example_items TO example_app;",
            UnsafeSession::TruncateAllowed {
                table: item_table.clone(),
                role: app.clone(),
            },
            "REVOKE TRUNCATE ON example_items FROM example_app;",
        ),
        (
            "temporary tables",
            "DO $$ BEGIN
                 EXECUTE format('GRANT TEMPORARY ON DATABASE %I TO example_app', current_database());
             END $$;",
            UnsafeSession::TemporaryTables { role: app.clone() },
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
        Err(UnsafeSession::DefinerFunction {
            function: "platform_names()".to_owned(),
            role: app.clone(),
        })
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
    let (as_owner, error) = refusal_through_pool(fixture.options("example_owner")).await;
    assert!(
        matches!(
            &error,
            TenantDbError::UnsafeSession(UnsafeSession::OwnsRelation(_))
        ),
        "{error}"
    );
    assert!(as_owner.ping().await.is_err(), "readiness fails too");
}

/// Begins a transaction through the kit's pool for `options` until the pool reports the
/// refusal of the session, and returns that pool and the error. A pool that times out before
/// its first session was checked has nothing to report (`TenantDbError::Database` with
/// `PoolTimedOut`): that says the machine is slow, not that the kit is wrong, so it is tried
/// again with a longer acquire timeout (`ACQUIRE_BUDGETS_SECONDS`). Any other outcome ends the
/// wait: a refusal is the result, and a transaction that began or another error is a bug.
#[allow(
    clippy::print_stdout,
    reason = "evidence for the test report, shown with `--nocapture`"
)]
async fn refusal_through_pool(options: PgConnectOptions) -> (TenantDb, TenantDbError) {
    let user = Identity::new().user(Some(TENANT_A), false);
    for seconds in ACQUIRE_BUDGETS_SECONDS {
        let pool = PgPoolOptions::new().acquire_timeout(Duration::from_secs(seconds));
        let db = TenantDb::connect_lazy(options.clone(), pool).expect("verify-full options");
        match db.begin(&user).await {
            Err(TenantDbError::Database(sqlx::Error::PoolTimedOut)) => {
                println!("no session was checked within {seconds} s; trying a longer timeout");
            }
            Err(error) => return (db, error),
            Ok(_) => panic!("a transaction began on a session that must be refused"),
        }
    }
    panic!(
        "the pool reported no refusal within {ACQUIRE_BUDGETS_SECONDS:?} s: no session was \
         checked in that time"
    );
}

/// Polls `/readyz` until it answers 200, for at most `READY_BUDGET`. A service that stops
/// answering, or answers anything but 200 or 503, fails the test at once; so does one that is
/// still not ready at the end, with its last answer and its output, which names the failing
/// check.
#[allow(
    clippy::print_stdout,
    reason = "evidence for the test report, shown with `--nocapture`"
)]
fn wait_until_ready(address: std::net::SocketAddr, stdout: &Output, stderr: &Output) {
    let started = Instant::now();
    loop {
        let answer = support::get(address, "/readyz");
        match &answer {
            Ok(response) if response.status == 200 => {
                println!("ready after {} ms", started.elapsed().as_millis());
                return;
            }
            Ok(response) if response.status == 503 => {}
            _ => panic!(
                "GET /readyz did not answer 200 or 503: {answer:?}\nstdout:\n{}\nstderr:\n{}",
                stdout.text(),
                stderr.text()
            ),
        }
        assert!(
            started.elapsed() < READY_BUDGET,
            "the service was not ready after {READY_BUDGET:?}; last answer: {answer:?}\nstdout:\n{}\nstderr:\n{}",
            stdout.text(),
            stderr.text()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
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
         GRANT SELECT ON private_items TO example_app;
         INSERT INTO private_items VALUES ('01928f6e-7a3c-7d4e-8f00-00000000000a', 'secret');",
    )
    .execute(&mut owner)
    .await
    .expect("a table without platform access, as the template says to leave it");

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
        let error = sqlx::query("DELETE FROM example_items")
            .execute(&mut *tx)
            .await
            .expect_err("the template grants the platform role no DELETE");
        assert_eq!(sqlstate(&error).as_deref(), Some(INSUFFICIENT_PRIVILEGE));
        tx.rollback().await.expect("rollback");
        // A table without platform_access gives the platform role no grant, so nothing.
        let mut tx = platform
            .begin(actor, "cross-tenant-test")
            .await
            .expect("begin");
        let error = sqlx::query("SELECT count(*) FROM private_items")
            .execute(&mut *tx)
            .await
            .expect_err("no platform_access policy, no grant");
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
    // Nor can its first statement turn the transaction back to read-write: `begin` has already
    // run a query, after which Postgres refuses (Laysi/trustiot-v2#271, verification F-4).
    let mut tx = reader.begin().await.expect("begin a read");
    let error = sqlx::query("SET TRANSACTION READ WRITE")
        .execute(&mut *tx)
        .await
        .expect_err("READ ONLY stays");
    assert_eq!(
        sqlstate(&error).as_deref(),
        Some("25001"),
        "active_sql_transaction: read-write mode must be set before any query"
    );
    drop(tx);

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

/// The tenant template's `platform_access` (Laysi/trustiot-v2#360): `tenant_isolation` applies
/// to the platform role too, but a platform session that sets a tenant itself gains nothing on a
/// table with `platform_access` (it already sees every tenant, and its grants still limit the
/// commands) or on a table it has no grant on. Only a grant without `platform_access` would hand
/// it the rows of the tenant it set, which is why the template says to grant nothing then, and
/// why the session check refuses the platform role's sessions when it has such a grant
/// (Laysi/trustiot-v2#366).
#[tokio::test]
async fn the_platform_role_gains_nothing_by_setting_a_tenant() {
    let Some(fixture) = Fixture::new(
        "CREATE DATABASE platform_tenant OWNER example_owner",
        "platform_tenant",
    )
    .await
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
         GRANT SELECT ON private_items TO example_app;
         INSERT INTO private_items VALUES ('01928f6e-7a3c-7d4e-8f00-00000000000a', 'secret');",
    )
    .execute(&mut owner)
    .await
    .expect("a table without platform access, as the template says to leave it");

    let platform = PlatformDb::connect_lazy(
        fixture.options("example_platform"),
        PgPoolOptions::new().max_connections(1),
    )
    .expect("verify-full options");
    let actor = PlatformActor::service(ServiceIdentity::new(ServiceName::Assets));
    let mut tx = platform
        .begin(actor, "tenant-setting-test")
        .await
        .expect("begin");
    let without_tenant: Vec<String> = sqlx::query_scalar(ITEM_NAMES)
        .fetch_all(&mut *tx)
        .await
        .expect("names");
    tx.rollback().await.expect("rollback");

    // Each statement in its own transaction, so a refusal does not abort the next one.
    let in_tenant = async |sql: &'static str| {
        let mut tx = platform
            .begin(actor, "tenant-setting-test")
            .await
            .expect("begin");
        sqlx::query(SET_TRANSACTION_TENANT)
            .bind(TENANT_A)
            .execute(&mut *tx)
            .await
            .expect("set a tenant in the platform transaction");
        let names = sqlx::query_scalar::<_, String>(sql)
            .fetch_all(&mut *tx)
            .await;
        tx.rollback().await.expect("rollback");
        names
    };
    let with_tenant = in_tenant(ITEM_NAMES).await.expect("names");
    assert_eq!(
        without_tenant,
        ["a-1", "a-2", "b-1"],
        "platform_access: every tenant"
    );
    assert_eq!(
        with_tenant, without_tenant,
        "platform_access: the same rows with a tenant set"
    );
    for (case, sql) in [
        (
            "INSERT, which the template does not grant",
            "INSERT INTO example_items (tenant_id, id, name) VALUES \
             ('01928f6e-7a3c-7d4e-8f00-00000000000a', gen_random_uuid(), 'x') RETURNING name",
        ),
        (
            "DELETE, which the template does not grant",
            "DELETE FROM example_items RETURNING name",
        ),
        (
            "a table without platform_access or a grant",
            "SELECT name FROM private_items",
        ),
    ] {
        let error = in_tenant(sql).await.expect_err(case);
        assert_eq!(
            sqlstate(&error).as_deref(),
            Some(INSUFFICIENT_PRIVILEGE),
            "{case}: {error}"
        );
        println!("platform role with tenant A set, {case}: refused ({error})");
    }

    // The case the template rules out: a grant without platform_access. The session check
    // refuses the platform role's new sessions from then on (Laysi/trustiot-v2#366). A plain
    // session, which skips the check, shows why: it sees no row until it sets a tenant itself.
    sqlx::raw_sql("GRANT SELECT ON private_items TO example_platform;")
        .execute(&mut owner)
        .await
        .expect("grant");
    assert_eq!(
        platform_check(&fixture).await,
        Err(tenant_only(
            "public.private_items",
            "example_platform",
            "SELECT"
        ))
    );
    let mut session = fixture.connect("example_platform").await;
    let mut names_in = async |tenant: Option<&str>| {
        let mut tx = session.begin().await.expect("begin");
        if let Some(tenant) = tenant {
            sqlx::query(SET_TRANSACTION_TENANT)
                .bind(tenant)
                .execute(&mut *tx)
                .await
                .expect("set a tenant");
        }
        let names: Vec<String> = sqlx::query_scalar("SELECT name FROM private_items")
            .fetch_all(&mut *tx)
            .await
            .expect("names");
        tx.rollback().await.expect("rollback");
        names
    };
    let without_tenant = names_in(None).await;
    let with_tenant = names_in(Some(TENANT_A)).await;
    assert_eq!(
        (without_tenant, with_tenant),
        (Vec::<String>::new(), vec!["secret".to_owned()]),
        "a grant without platform_access opens the tenant a platform session sets"
    );
    println!(
        "platform_access: a self-set tenant adds no row or command; a grant without \
         platform_access would open the self-set tenant's rows, so the template grants nothing \
         and the session check refuses the platform role's sessions"
    );
}

/// The session check of a plain session of the platform role, with the platform role's rules.
async fn platform_check(fixture: &Fixture) -> Result<(), UnsafeSession> {
    let mut session = fixture.connect("example_platform").await;
    postgres::check_session(&mut session, SessionRole::Platform)
        .await
        .expect("the check runs")
}

/// The refusal of a platform session whose `role` may run `command` on `table` where only the
/// tenant condition opens it.
fn tenant_only(table: &str, role: &str, command: &str) -> UnsafeSession {
    UnsafeSession::PlatformTenantOnly {
        table: table.to_owned(),
        role: role.to_owned(),
        command: command.to_owned(),
    }
}

/// The rows `sql` reaches in a plain session of the platform role (no session check): in a
/// transaction that first takes `role` with `SET ROLE` (as `set_config('role', ...)`) when given
/// and sets `tenant` when given, as platform code that went wrong could. Rolled back. The error
/// is the first statement's that failed.
async fn try_reach(
    fixture: &Fixture,
    role: Option<&str>,
    tenant: Option<&str>,
    sql: &'static str,
) -> Result<u64, sqlx::Error> {
    try_reach_as(fixture, "example_platform", role, tenant, sql).await
}

/// [`try_reach`] in a plain session of `login`.
async fn try_reach_as(
    fixture: &Fixture,
    login: &str,
    role: Option<&str>,
    tenant: Option<&str>,
    sql: &'static str,
) -> Result<u64, sqlx::Error> {
    async fn run(
        tx: &mut PgConnection,
        role: Option<&str>,
        tenant: Option<&str>,
        sql: &'static str,
    ) -> Result<u64, sqlx::Error> {
        if let Some(role) = role {
            sqlx::query("SELECT set_config('role', $1, true)")
                .bind(role)
                .execute(&mut *tx)
                .await?;
        }
        if let Some(tenant) = tenant {
            sqlx::query(SET_TRANSACTION_TENANT)
                .bind(tenant)
                .execute(&mut *tx)
                .await?;
        }
        Ok(sqlx::query(sql).execute(&mut *tx).await?.rows_affected())
    }
    let mut session = fixture.connect(login).await;
    let mut tx = session.begin().await.expect("begin");
    let outcome = run(&mut tx, role, tenant, sql).await;
    tx.rollback().await.expect("rollback");
    outcome
}

/// [`try_reach`], where a statement that row-level security refuses (a new row that fails
/// `WITH CHECK`) or that reads the tenant setting while it is not set reaches no row; any other
/// error fails the test.
async fn rows_reached(
    fixture: &Fixture,
    role: Option<&str>,
    tenant: Option<&str>,
    sql: &'static str,
) -> u64 {
    match try_reach(fixture, role, tenant, sql).await {
        Ok(rows) => rows,
        Err(error) => {
            let text = error.to_string();
            assert!(
                text.contains("violates row-level security policy")
                    || text
                        .contains(r#"unrecognized configuration parameter "trustiot.tenant_id""#),
                "{sql}: {error}"
            );
            0
        }
    }
}

/// A migration mistake for [`platform_sessions_only_the_tenant_condition_opens_are_refused`].
struct PlatformMistake {
    mistake: &'static str,
    make: &'static str,
    refused: UnsafeSession,
    /// What a platform session reaches: `SET ROLE` to this role first (when given), then run
    /// the statement.
    probe: (Option<&'static str>, &'static str),
    /// Whether the probe reaches rows with tenant A set (it never does without a tenant).
    leaks: bool,
    fix: &'static str,
}

/// Laysi/trustiot-v2#366: the platform role's session is refused when the platform role, or a
/// role it can become with `SET ROLE`, may run a command on a table with row-level security but
/// the permissive policies that apply to it for that command may open only the session's tenant
/// (the tenant condition, however written, or a policy that uses a function, operator, or
/// relation the check cannot look into). Such a grant
/// gives `PlatformDb`, which never sets a tenant, nothing, and gives a platform session that sets
/// a tenant itself that tenant's rows; each case shows both on a plain session, then that the
/// fix is accepted. The templates stay accepted (here, in
/// `sessions_row_level_security_would_not_protect_are_refused`, and in
/// `outbox_events_follow_the_tenant_policies`).
#[tokio::test]
async fn platform_sessions_only_the_tenant_condition_opens_are_refused() {
    let Some(fixture) = Fixture::new(
        "CREATE DATABASE platform_guard OWNER example_owner",
        "platform_guard",
    )
    .await
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
         GRANT SELECT, INSERT, UPDATE, DELETE ON private_items TO example_app;
         INSERT INTO private_items VALUES
             ('01928f6e-7a3c-7d4e-8f00-00000000000a', 'secret-a'),
             ('01928f6e-7a3c-7d4e-8f00-00000000000b', 'secret-b');
         CREATE SCHEMA other;
         CREATE TABLE other.private_items (tenant_id uuid NOT NULL, name text NOT NULL);
         ALTER TABLE other.private_items ENABLE ROW LEVEL SECURITY;
         CREATE POLICY tenant_isolation ON other.private_items
             USING (tenant_id = NULLIF(current_setting('trustiot.tenant_id', true), '')::uuid)
             WITH CHECK (tenant_id = NULLIF(current_setting('trustiot.tenant_id', true), '')::uuid);
         INSERT INTO other.private_items VALUES ('01928f6e-7a3c-7d4e-8f00-00000000000a', 'other-a');
         CREATE TABLE locked_items (tenant_id uuid NOT NULL, name text NOT NULL);
         ALTER TABLE locked_items ENABLE ROW LEVEL SECURITY;
         INSERT INTO locked_items VALUES ('01928f6e-7a3c-7d4e-8f00-00000000000a', 'locked-a');
         CREATE FUNCTION current_tenant() RETURNS uuid LANGUAGE sql STABLE
             AS $$ SELECT NULLIF(current_setting('trustiot.tenant_id', true), '')::uuid $$;
         CREATE FUNCTION is_current_tenant(uuid) RETURNS boolean LANGUAGE sql STABLE
             AS $$ SELECT $1 = NULLIF(current_setting('trustiot.tenant_id', true), '')::uuid $$;
         CREATE OPERATOR ~~~ (RIGHTARG = uuid, FUNCTION = is_current_tenant);
         CREATE VIEW current_tenant_view WITH (security_invoker = true) AS
             SELECT NULLIF(current_setting('trustiot.tenant_id', true), '')::uuid AS tenant;
         GRANT SELECT ON current_tenant_view TO example_platform, example_platform_switch;",
    )
    .execute(&mut owner)
    .await
    .expect("tables without platform access");
    assert_eq!(
        platform_check(&fixture).await,
        Ok(()),
        "the tenant template"
    );
    let mut app = fixture.connect("example_app").await;
    assert_eq!(
        postgres::check_session(&mut app, SessionRole::Service)
            .await
            .expect("check"),
        Ok(()),
        "the rule is the platform role's only: the service role's tables have only the tenant \
         condition"
    );

    let private = "public.private_items";
    let mistakes = [
        PlatformMistake {
            mistake: "SELECT without platform_access",
            make: "GRANT SELECT ON private_items TO example_platform;",
            refused: tenant_only(private, "example_platform", "SELECT"),
            probe: (None, "SELECT name FROM private_items"),
            leaks: true,
            fix: "REVOKE SELECT ON private_items FROM example_platform;",
        },
        PlatformMistake {
            mistake: "INSERT without platform_access",
            make: "GRANT INSERT ON private_items TO example_platform;",
            refused: tenant_only(private, "example_platform", "INSERT"),
            probe: (
                None,
                "INSERT INTO private_items VALUES ('01928f6e-7a3c-7d4e-8f00-00000000000a', 'planted')",
            ),
            leaks: true,
            fix: "REVOKE INSERT ON private_items FROM example_platform;",
        },
        PlatformMistake {
            mistake: "UPDATE without platform_access",
            make: "GRANT UPDATE ON private_items TO example_platform;",
            refused: tenant_only(private, "example_platform", "UPDATE"),
            probe: (None, "UPDATE private_items SET name = 'changed'"),
            leaks: true,
            fix: "REVOKE UPDATE ON private_items FROM example_platform;",
        },
        PlatformMistake {
            mistake: "DELETE without platform_access",
            make: "GRANT DELETE ON private_items TO example_platform;",
            refused: tenant_only(private, "example_platform", "DELETE"),
            probe: (None, "DELETE FROM private_items"),
            leaks: true,
            fix: "REVOKE DELETE ON private_items FROM example_platform;",
        },
        PlatformMistake {
            mistake: "a column privilege",
            make: "GRANT SELECT (name) ON private_items TO example_platform;",
            refused: tenant_only(private, "example_platform", "SELECT"),
            probe: (None, "SELECT name FROM private_items"),
            leaks: true,
            fix: "REVOKE SELECT (name) ON private_items FROM example_platform;",
        },
        PlatformMistake {
            mistake: "a privilege for PUBLIC",
            make: "GRANT SELECT ON private_items TO PUBLIC;",
            refused: tenant_only(private, "example_platform", "SELECT"),
            probe: (None, "SELECT name FROM private_items"),
            leaks: true,
            fix: "REVOKE SELECT ON private_items FROM PUBLIC;",
        },
        PlatformMistake {
            mistake: "a privilege inherited from a group",
            make: "GRANT SELECT ON private_items TO example_platform_group;",
            refused: tenant_only(private, "example_platform", "SELECT"),
            probe: (None, "SELECT name FROM private_items"),
            leaks: true,
            fix: "REVOKE SELECT ON private_items FROM example_platform_group;",
        },
        PlatformMistake {
            // platform_access names the platform role, so it does not apply after SET ROLE.
            mistake: "a group's privilege on a table with platform_access",
            make: "GRANT SELECT ON example_items TO example_platform_group;",
            refused: tenant_only("public.example_items", "example_platform_group", "SELECT"),
            probe: (Some("example_platform_group"), "SELECT name FROM example_items"),
            leaks: true,
            fix: "REVOKE SELECT ON example_items FROM example_platform_group;",
        },
        PlatformMistake {
            mistake: "a privilege the platform role reaches only with SET ROLE",
            make: "GRANT SELECT ON private_items TO example_platform_switch;",
            refused: tenant_only(private, "example_platform_switch", "SELECT"),
            probe: (Some("example_platform_switch"), "SELECT name FROM private_items"),
            leaks: true,
            fix: "REVOKE SELECT ON private_items FROM example_platform_switch;",
        },
        PlatformMistake {
            mistake: "a policy for the platform role by name that is the tenant condition",
            make: "CREATE POLICY platform_tenant ON private_items TO example_platform
                       USING (tenant_id = NULLIF(current_setting('trustiot.tenant_id', true), '')::uuid)
                       WITH CHECK (tenant_id = NULLIF(current_setting('trustiot.tenant_id', true), '')::uuid);
                   GRANT SELECT ON private_items TO example_platform;",
            refused: tenant_only(private, "example_platform", "SELECT"),
            probe: (None, "SELECT name FROM private_items"),
            leaks: true,
            fix: "DROP POLICY platform_tenant ON private_items;
                  REVOKE SELECT ON private_items FROM example_platform;",
        },
        PlatformMistake {
            mistake: "a platform policy for SELECT and a DELETE grant",
            make: "CREATE POLICY platform_read ON private_items FOR SELECT TO example_platform
                       USING (true);
                   GRANT SELECT, DELETE ON private_items TO example_platform;",
            refused: tenant_only(private, "example_platform", "DELETE"),
            probe: (None, "DELETE FROM private_items"),
            leaks: true,
            fix: "DROP POLICY platform_read ON private_items;
                  REVOKE SELECT, DELETE ON private_items FROM example_platform;",
        },
        PlatformMistake {
            // Every row is visible to UPDATE, but only a row of the session's tenant may result:
            // a session that sets tenant A can move tenant B's rows into tenant A.
            mistake: "a platform UPDATE policy whose WITH CHECK is the tenant condition",
            make: "CREATE POLICY platform_update ON private_items FOR UPDATE TO example_platform
                       USING (true)
                       WITH CHECK (tenant_id = NULLIF(current_setting('trustiot.tenant_id', true), '')::uuid);
                   GRANT UPDATE ON private_items TO example_platform;",
            refused: tenant_only(private, "example_platform", "UPDATE"),
            probe: (
                None,
                "UPDATE private_items
                 SET tenant_id = '01928f6e-7a3c-7d4e-8f00-00000000000a', name = 'moved'",
            ),
            leaks: true,
            fix: "DROP POLICY platform_update ON private_items;
                  REVOKE UPDATE ON private_items FROM example_platform;",
        },
        PlatformMistake {
            mistake: "a table in another schema the platform role may use",
            make: "GRANT USAGE ON SCHEMA other TO example_platform;
                   GRANT SELECT ON other.private_items TO example_platform;",
            refused: tenant_only("other.private_items", "example_platform", "SELECT"),
            probe: (None, "SELECT name FROM other.private_items"),
            leaks: true,
            // Without USAGE on the schema the grant reaches nothing.
            fix: "REVOKE USAGE ON SCHEMA other FROM example_platform;",
        },
        PlatformMistake {
            // Laysi/trustiot-v2#366 verification F2: a policy applies to a role that inherits the
            // privileges of the policy's role. The role the platform role can SET ROLE to belongs
            // to example_switch_parent without inheriting, so the parent's policy opens nothing.
            mistake: "a policy for a role the SET ROLE target belongs to without inheriting",
            make: "GRANT SELECT ON private_items TO example_platform_switch;
                   CREATE POLICY parent_access ON private_items TO example_switch_parent
                       USING (true);",
            refused: tenant_only(private, "example_platform_switch", "SELECT"),
            probe: (Some("example_platform_switch"), "SELECT name FROM private_items"),
            leaks: true,
            fix: "DROP POLICY parent_access ON private_items;
                  REVOKE SELECT ON private_items FROM example_platform_switch;",
        },
        PlatformMistake {
            // Laysi/trustiot-v2#366 verification F1: only the session's tenant, written another way.
            mistake: "a platform policy reading the tenant setting, written the other way round",
            make: "CREATE POLICY platform_reversed ON private_items TO example_platform
                       USING (current_setting('trustiot.tenant_id', true)::uuid = tenant_id);
                   GRANT SELECT ON private_items TO example_platform;",
            refused: tenant_only(private, "example_platform", "SELECT"),
            probe: (None, "SELECT name FROM private_items"),
            leaks: true,
            fix: "DROP POLICY platform_reversed ON private_items;
                  REVOKE SELECT ON private_items FROM example_platform;",
        },
        PlatformMistake {
            // Laysi/trustiot-v2#366 verification F1: only the session's tenant, written another way.
            mistake: "a platform policy comparing the tenant setting as text",
            make: "CREATE POLICY platform_text ON private_items TO example_platform
                       USING (tenant_id::text = current_setting('trustiot.tenant_id'));
                   GRANT SELECT ON private_items TO example_platform;",
            refused: tenant_only(private, "example_platform", "SELECT"),
            probe: (None, "SELECT name FROM private_items"),
            leaks: true,
            fix: "DROP POLICY platform_text ON private_items;
                  REVOKE SELECT ON private_items FROM example_platform;",
        },
        PlatformMistake {
            // Laysi/trustiot-v2#366 verification F1: only the session's tenant, written another way.
            mistake: "a platform policy that is the tenant condition AND true",
            make: "CREATE POLICY platform_and_true ON private_items TO example_platform
                       USING (tenant_id = NULLIF(current_setting('trustiot.tenant_id', true), '')::uuid AND true);
                   GRANT SELECT ON private_items TO example_platform;",
            refused: tenant_only(private, "example_platform", "SELECT"),
            probe: (None, "SELECT name FROM private_items"),
            leaks: true,
            fix: "DROP POLICY platform_and_true ON private_items;
                  REVOKE SELECT ON private_items FROM example_platform;",
        },
        PlatformMistake {
            // Laysi/trustiot-v2#366 verification F1: only the session's tenant, written another way.
            mistake: "a platform policy that is the tenant condition OR false",
            make: "CREATE POLICY platform_or_false ON private_items TO example_platform
                       USING (tenant_id = NULLIF(current_setting('trustiot.tenant_id', true), '')::uuid OR false);
                   GRANT SELECT ON private_items TO example_platform;",
            refused: tenant_only(private, "example_platform", "SELECT"),
            probe: (None, "SELECT name FROM private_items"),
            leaks: true,
            fix: "DROP POLICY platform_or_false ON private_items;
                  REVOKE SELECT ON private_items FROM example_platform;",
        },
        PlatformMistake {
            // Laysi/trustiot-v2#366 verification F1: only the session's tenant, written another way.
            mistake: "a platform policy with IS NOT DISTINCT FROM the tenant setting",
            make: "CREATE POLICY platform_not_distinct ON private_items TO example_platform
                       USING (tenant_id IS NOT DISTINCT FROM NULLIF(current_setting('trustiot.tenant_id', true), '')::uuid);
                   GRANT SELECT ON private_items TO example_platform;",
            refused: tenant_only(private, "example_platform", "SELECT"),
            probe: (None, "SELECT name FROM private_items"),
            leaks: true,
            fix: "DROP POLICY platform_not_distinct ON private_items;
                  REVOKE SELECT ON private_items FROM example_platform;",
        },
        PlatformMistake {
            // Laysi/trustiot-v2#366 verification F1: only the session's tenant, written another way.
            mistake: "a platform policy USING (false)",
            make: "CREATE POLICY platform_false ON private_items TO example_platform
                       USING (false);
                   GRANT SELECT ON private_items TO example_platform;",
            refused: tenant_only(private, "example_platform", "SELECT"),
            probe: (None, "SELECT name FROM private_items"),
            leaks: true,
            fix: "DROP POLICY platform_false ON private_items;
                  REVOKE SELECT ON private_items FROM example_platform;",
        },
        PlatformMistake {
            // The tenant condition behind a function: the check cannot see the function's body,
            // so a policy that calls a function that is not built in opens nothing beyond the
            // tenant as far as the check can tell.
            mistake: "a policy for the SET ROLE target that reads the tenant through a function",
            make: "CREATE POLICY switch_function ON private_items TO example_platform_switch
                       USING (tenant_id = current_tenant());
                   GRANT SELECT ON private_items TO example_platform_switch;",
            refused: tenant_only(private, "example_platform_switch", "SELECT"),
            probe: (Some("example_platform_switch"), "SELECT name FROM private_items"),
            leaks: true,
            fix: "DROP POLICY switch_function ON private_items;
                  REVOKE SELECT ON private_items FROM example_platform_switch;",
        },
        PlatformMistake {
            mistake: "a platform policy that reads the tenant through a function",
            make: "CREATE POLICY platform_function ON private_items TO example_platform
                       USING (tenant_id = current_tenant());
                   GRANT SELECT ON private_items TO example_platform;",
            refused: tenant_only(private, "example_platform", "SELECT"),
            probe: (None, "SELECT name FROM private_items"),
            leaks: true,
            fix: "DROP POLICY platform_function ON private_items;
                  REVOKE SELECT ON private_items FROM example_platform;",
        },
        PlatformMistake {
            mistake: "a platform policy that reads the tenant through an operator",
            make: "CREATE POLICY platform_operator ON private_items TO example_platform
                       USING (~~~ tenant_id);
                   GRANT SELECT ON private_items TO example_platform;",
            refused: tenant_only(private, "example_platform", "SELECT"),
            probe: (None, "SELECT name FROM private_items"),
            leaks: true,
            fix: "DROP POLICY platform_operator ON private_items;
                  REVOKE SELECT ON private_items FROM example_platform;",
        },
        PlatformMistake {
            mistake: "a platform policy that reads the tenant through a view",
            make: "CREATE POLICY platform_view ON private_items TO example_platform
                       USING (tenant_id IN (SELECT tenant FROM current_tenant_view));
                   GRANT SELECT ON private_items TO example_platform;",
            refused: tenant_only(private, "example_platform", "SELECT"),
            probe: (None, "SELECT name FROM private_items"),
            leaks: true,
            fix: "DROP POLICY platform_view ON private_items;
                  REVOKE SELECT ON private_items FROM example_platform;",
        },
        PlatformMistake {
            mistake: "a platform policy that builds the setting's name",
            make: "CREATE POLICY platform_built_name ON private_items TO example_platform
                       USING (tenant_id = NULLIF(current_setting('trustiot.' || 'tenant_id', true), '')::uuid);
                   GRANT SELECT ON private_items TO example_platform;",
            refused: tenant_only(private, "example_platform", "SELECT"),
            probe: (None, "SELECT name FROM private_items"),
            leaks: true,
            fix: "DROP POLICY platform_built_name ON private_items;
                  REVOKE SELECT ON private_items FROM example_platform;",
        },
        PlatformMistake {
            // The check does not count restrictive policies: they can only narrow access, and
            // whether one closes everything the tenant condition opens is not something it
            // judges. The grant still gives PlatformDb nothing, so it is refused all the same.
            // (Every row has a tenant, so the restrictive policy lets none through.)
            mistake: "a grant that a restrictive policy closes",
            make: "CREATE POLICY platform_nothing ON private_items AS RESTRICTIVE
                       TO example_platform USING (tenant_id IS NULL);
                   GRANT SELECT ON private_items TO example_platform;",
            refused: tenant_only(private, "example_platform", "SELECT"),
            probe: (None, "SELECT name FROM private_items"),
            leaks: false,
            fix: "DROP POLICY platform_nothing ON private_items;
                  REVOKE SELECT ON private_items FROM example_platform;",
        },
    ];
    for case in mistakes {
        let PlatformMistake {
            mistake,
            make,
            refused,
            probe: (role, sql),
            leaks,
            fix,
        } = case;
        sqlx::raw_sql(make)
            .execute(&mut owner)
            .await
            .unwrap_or_else(|error| panic!("{mistake}: {error}"));
        assert_eq!(
            platform_check(&fixture).await,
            Err(refused.clone()),
            "{mistake}"
        );
        let without_tenant = rows_reached(&fixture, role, None, sql).await;
        let with_tenant = rows_reached(&fixture, role, Some(TENANT_A), sql).await;
        assert_eq!(without_tenant, 0, "{mistake}: nothing without a tenant");
        assert_eq!(
            with_tenant > 0,
            leaks,
            "{mistake}: {with_tenant} rows with tenant A set"
        );
        sqlx::raw_sql(fix)
            .execute(&mut owner)
            .await
            .unwrap_or_else(|error| panic!("{mistake}: {error}"));
        assert_eq!(platform_check(&fixture).await, Ok(()), "{mistake} fixed");
        println!(
            "{mistake}: refused with \"{refused}\"; a plain platform session reaches \
             {without_tenant} rows without a tenant and {with_tenant} with tenant A set; \
             accepted once fixed"
        );
    }

    // A grant on a table with row-level security and no policy at all reaches no row, with or
    // without a tenant, so it is accepted.
    sqlx::raw_sql("GRANT SELECT ON locked_items TO example_platform;")
        .execute(&mut owner)
        .await
        .expect("grant");
    assert_eq!(platform_check(&fixture).await, Ok(()), "no policy at all");
    for tenant in [None, Some(TENANT_A)] {
        let rows = rows_reached(&fixture, None, tenant, "SELECT name FROM locked_items").await;
        assert_eq!(rows, 0, "no policy, no row, tenant {tenant:?}");
    }
    println!("a grant on a table without any policy: accepted, no row with or without a tenant");

    // Built-in functions and operators record no dependency, so a platform policy that uses
    // them still counts as opening the table.
    sqlx::raw_sql(
        "CREATE POLICY platform_builtin ON private_items TO example_platform
             USING (length(name) > 0 AND lower(name) <> '');
         GRANT SELECT ON private_items TO example_platform;",
    )
    .execute(&mut owner)
    .await
    .expect("a platform policy with built-in functions");
    assert_eq!(
        platform_check(&fixture).await,
        Ok(()),
        "a platform policy with built-in functions"
    );
    let rows = rows_reached(&fixture, None, None, "SELECT name FROM private_items").await;
    assert_eq!(rows, 2, "the policy opens every row without a tenant");
    sqlx::raw_sql(
        "DROP POLICY platform_builtin ON private_items;
         REVOKE SELECT ON private_items FROM example_platform;",
    )
    .execute(&mut owner)
    .await
    .expect("drop");
    println!("a platform policy with built-in functions: accepted, {rows} rows without a tenant");

    // A platform policy that reads the tenant setting counts as opening nothing beyond the
    // tenant even when it opens every row: the check refuses rather than judge the expression,
    // so such a policy fails closed.
    sqlx::raw_sql(
        "CREATE POLICY platform_or_true ON private_items TO example_platform
             USING (tenant_id = NULLIF(current_setting('trustiot.tenant_id', true), '')::uuid
                    OR true);
         GRANT SELECT ON private_items TO example_platform;",
    )
    .execute(&mut owner)
    .await
    .expect("a platform policy that reads the tenant setting and opens every row");
    assert_eq!(
        platform_check(&fixture).await,
        Err(tenant_only(private, "example_platform", "SELECT")),
        "fails closed"
    );
    let rows = rows_reached(&fixture, None, None, "SELECT name FROM private_items").await;
    assert_eq!(rows, 2, "the policy does open every row");
    sqlx::raw_sql(
        "DROP POLICY platform_or_true ON private_items;
         REVOKE SELECT ON private_items FROM example_platform;",
    )
    .execute(&mut owner)
    .await
    .expect("drop");
    assert_eq!(platform_check(&fixture).await, Ok(()));
    println!(
        "<tenant condition> OR true for the platform role: refused (fails closed), {rows} rows"
    );

    // Laysi/trustiot-v2#366 verification F3: a role the platform role can neither inherit nor
    // become gives it nothing, so its grant is accepted...
    sqlx::raw_sql("GRANT SELECT ON private_items TO example_platform_unusable;")
        .execute(&mut owner)
        .await
        .expect("grant");
    assert_eq!(platform_check(&fixture).await, Ok(()), "an unusable role");
    for role in [None, Some("example_platform_unusable")] {
        let error = try_reach(
            &fixture,
            role,
            Some(TENANT_A),
            "SELECT name FROM private_items",
        )
        .await
        .expect_err("nothing reaches the rows");
        assert_eq!(
            sqlstate(&error).as_deref(),
            Some(INSUFFICIENT_PRIVILEGE),
            "{role:?}: {error}"
        );
        println!("an unusable role's grant, as {role:?} with tenant A set: refused ({error})");
    }
    sqlx::raw_sql("REVOKE SELECT ON private_items FROM example_platform_unusable;")
        .execute(&mut owner)
        .await
        .expect("revoke");
    // ...and so is a role it only inherits from, on a table platform_access opens to it: the
    // inherited DELETE is the platform role's own, and platform_access applies to it.
    sqlx::raw_sql("GRANT DELETE ON example_items TO example_platform_inherited;")
        .execute(&mut owner)
        .await
        .expect("grant");
    assert_eq!(
        platform_check(&fixture).await,
        Ok(()),
        "an inherit-only role"
    );
    let without_tenant = rows_reached(&fixture, None, None, "DELETE FROM example_items").await;
    let with_tenant =
        rows_reached(&fixture, None, Some(TENANT_A), "DELETE FROM example_items").await;
    assert_eq!(
        (without_tenant, with_tenant),
        (3, 3),
        "platform_access: every tenant, with or without a tenant set"
    );
    let error = try_reach(
        &fixture,
        Some("example_platform_inherited"),
        None,
        "DELETE FROM example_items",
    )
    .await
    .expect_err("no SET ROLE to an inherit-only role");
    assert_eq!(
        sqlstate(&error).as_deref(),
        Some(INSUFFICIENT_PRIVILEGE),
        "{error}"
    );
    sqlx::raw_sql("REVOKE DELETE ON example_items FROM example_platform_inherited;")
        .execute(&mut owner)
        .await
        .expect("revoke");
    println!(
        "an inherit-only role's DELETE on a table with platform_access: accepted, \
         {without_tenant} rows without a tenant and {with_tenant} with tenant A set; SET ROLE \
         refused ({error})"
    );

    // Through the kit's pool the refusal reaches the caller of PlatformDb, as for TenantDb.
    sqlx::raw_sql("GRANT SELECT ON private_items TO example_platform;")
        .execute(&mut owner)
        .await
        .expect("grant");
    let error = platform_refusal_through_pool(fixture.options("example_platform")).await;
    assert!(
        matches!(
            &error,
            TenantDbError::UnsafeSession(problem)
                if *problem == tenant_only(private, "example_platform", "SELECT")
        ),
        "{error}"
    );
    println!("PlatformDb: {error}");
}

/// Pings a `PlatformDb` for `options` until the pool reports the refusal of the session, like
/// [`refusal_through_pool`] does for `TenantDb`, and returns the error.
#[allow(
    clippy::print_stdout,
    reason = "evidence for the test report, shown with `--nocapture`"
)]
async fn platform_refusal_through_pool(options: PgConnectOptions) -> TenantDbError {
    for seconds in ACQUIRE_BUDGETS_SECONDS {
        let pool = PgPoolOptions::new().acquire_timeout(Duration::from_secs(seconds));
        let db = PlatformDb::connect_lazy(options.clone(), pool).expect("verify-full options");
        match db.ping().await {
            Err(TenantDbError::Database(sqlx::Error::PoolTimedOut)) => {
                println!("no session was checked within {seconds} s; trying a longer timeout");
            }
            Err(error) => return error,
            Ok(()) => panic!("a platform session that must be refused was used"),
        }
    }
    panic!(
        "the pool reported no refusal within {ACQUIRE_BUDGETS_SECONDS:?} s: no session was \
         checked in that time"
    );
}

/// A setup for [`sessions_are_checked_for_every_role_they_can_become`].
struct SetRoleCase {
    case: &'static str,
    /// The role the session logs in as, and the rules it is checked with.
    session: (&'static str, SessionRole),
    make: &'static str,
    /// The refusal, or `None` when the session is accepted.
    refused: Option<UnsafeSession>,
    /// What a plain session of the login role can do without a tenant: take this role with
    /// `SET ROLE` first (when given), then run the statement. `Some(n)`: it succeeds and reports
    /// `n` rows; `None`: Postgres refuses it for lack of privilege.
    probes: &'static [(Option<&'static str>, &'static str, Option<u64>)],
    fix: &'static str,
}

fn unprotected_table(table: &str, role: &str) -> UnsafeSession {
    UnsafeSession::UnprotectedTable {
        table: table.to_owned(),
        role: role.to_owned(),
    }
}

fn permissive_policy(policy: &str, role: &str) -> UnsafeSession {
    UnsafeSession::PermissivePolicy {
        policy: policy.to_owned(),
        role: role.to_owned(),
    }
}

const LOOSE_ITEMS: &str = "SELECT name FROM loose_items";
const SWITCH_ITEMS: &str = "SELECT name FROM switch_items";
const KEEP_ITEMS: &str = "CREATE TEMPORARY TABLE kept_items (tenant_id uuid, name text)";

/// Laysi/trustiot-v2#381: a session is checked for every role it can become with `SET ROLE`,
/// not only for its own: a privilege that a role it can become holds, through a direct `SET`
/// membership, a chain of them, or one of a group it inherits, is refused like the session's own
/// and names that role. Each refused setup shows on a plain session that `SET ROLE` reaches
/// other tenants' rows (or truncates, or creates a temporary table) without any tenant set.
/// Memberships the session can only inherit, or can neither inherit nor use for `SET ROLE`, keep
/// the results they had before.
#[tokio::test]
async fn sessions_are_checked_for_every_role_they_can_become() {
    let Some(fixture) =
        Fixture::new("CREATE DATABASE set_role OWNER example_owner", "set_role").await
    else {
        return;
    };
    let mut owner = fixture.connect("example_owner").await;
    sqlx::raw_sql(
        "CREATE TABLE loose_items (tenant_id uuid NOT NULL, name text NOT NULL);
         INSERT INTO loose_items VALUES
             ('01928f6e-7a3c-7d4e-8f00-00000000000a', 'loose-a'),
             ('01928f6e-7a3c-7d4e-8f00-00000000000b', 'loose-b');
         CREATE TABLE switch_items (tenant_id uuid NOT NULL, name text NOT NULL);
         ALTER TABLE switch_items ENABLE ROW LEVEL SECURITY;
         CREATE POLICY tenant_isolation ON switch_items
             USING (tenant_id = NULLIF(current_setting('trustiot.tenant_id', true), '')::uuid)
             WITH CHECK (tenant_id = NULLIF(current_setting('trustiot.tenant_id', true), '')::uuid);
         INSERT INTO switch_items VALUES
             ('01928f6e-7a3c-7d4e-8f00-00000000000a', 'switch-a'),
             ('01928f6e-7a3c-7d4e-8f00-00000000000b', 'switch-b');
         CREATE VIEW item_names AS SELECT tenant_id, name FROM example_items;
         CREATE MATERIALIZED VIEW item_counts AS
             SELECT tenant_id, count(*) AS items FROM example_items GROUP BY tenant_id;
         CREATE FUNCTION all_item_names() RETURNS SETOF text LANGUAGE sql SECURITY DEFINER
             AS 'SELECT name FROM example_items';
         REVOKE EXECUTE ON FUNCTION all_item_names() FROM PUBLIC;",
    )
    .execute(&mut owner)
    .await
    .expect("tenant data no role of the session may use yet");
    let app = ("example_app", SessionRole::Service);
    let platform = ("example_platform", SessionRole::Platform);
    assert_eq!(check_login(&fixture, app).await, Ok(()), "the service role");
    assert_eq!(
        check_login(&fixture, platform).await,
        Ok(()),
        "the platform role"
    );

    let loose = "public.loose_items";
    let switch = "example_app_switch";
    let cases = [
        SetRoleCase {
            case: "a table without row-level security that a SET ROLE target may use",
            session: app,
            make: "GRANT SELECT ON loose_items TO example_app_switch;",
            refused: Some(unprotected_table(loose, switch)),
            probes: &[
                (Some("example_app_switch"), LOOSE_ITEMS, Some(2)),
                (None, LOOSE_ITEMS, None),
            ],
            fix: "REVOKE SELECT ON loose_items FROM example_app_switch;",
        },
        SetRoleCase {
            case: "a role the session reaches through a chain of SET memberships",
            session: app,
            make: "GRANT SELECT ON loose_items TO example_app_switch_parent;",
            refused: Some(unprotected_table(loose, "example_app_switch_parent")),
            probes: &[
                (Some("example_app_switch_parent"), LOOSE_ITEMS, Some(2)),
                (Some("example_app_switch"), LOOSE_ITEMS, None),
            ],
            fix: "REVOKE SELECT ON loose_items FROM example_app_switch_parent;",
        },
        SetRoleCase {
            case: "a role the session reaches with SET through a group it inherits",
            session: app,
            make: "GRANT SELECT ON loose_items TO example_app_group_switch;",
            refused: Some(unprotected_table(loose, "example_app_group_switch")),
            probes: &[
                (Some("example_app_group_switch"), LOOSE_ITEMS, Some(2)),
                (None, LOOSE_ITEMS, None),
            ],
            fix: "REVOKE SELECT ON loose_items FROM example_app_group_switch;",
        },
        SetRoleCase {
            case: "a privilege a SET ROLE target inherits",
            session: app,
            make: "GRANT SELECT ON loose_items TO example_app_switch_group;",
            refused: Some(unprotected_table(loose, switch)),
            probes: &[
                (Some("example_app_switch"), LOOSE_ITEMS, Some(2)),
                (Some("example_app_switch_group"), LOOSE_ITEMS, None),
            ],
            fix: "REVOKE SELECT ON loose_items FROM example_app_switch_group;",
        },
        SetRoleCase {
            case: "a privilege for PUBLIC: the session's own role is named",
            session: app,
            make: "GRANT SELECT ON loose_items TO PUBLIC;",
            refused: Some(unprotected_table(loose, "example_app")),
            probes: &[
                (None, LOOSE_ITEMS, Some(2)),
                (Some("example_alias"), LOOSE_ITEMS, Some(2)),
            ],
            fix: "REVOKE SELECT ON loose_items FROM PUBLIC;",
        },
        SetRoleCase {
            case: "a privilege of a SET ROLE target whose name sorts before the session role's",
            session: app,
            make: "GRANT SELECT ON loose_items TO example_alias;",
            refused: Some(unprotected_table(loose, "example_alias")),
            probes: &[(Some("example_alias"), LOOSE_ITEMS, Some(2))],
            fix: "REVOKE SELECT ON loose_items FROM example_alias;",
        },
        SetRoleCase {
            case: "a permissive policy for a SET ROLE target",
            session: app,
            make: "GRANT SELECT ON switch_items TO example_app_switch;
                   CREATE POLICY switch_all ON switch_items FOR SELECT TO example_app_switch
                       USING (true);",
            refused: Some(permissive_policy(
                "public.switch_items policy switch_all",
                switch,
            )),
            probes: &[(Some("example_app_switch"), SWITCH_ITEMS, Some(2))],
            fix: "DROP POLICY switch_all ON switch_items;
                  REVOKE SELECT ON switch_items FROM example_app_switch;",
        },
        SetRoleCase {
            case: "a permissive policy for PUBLIC on a table only a SET ROLE target may use",
            session: app,
            make: "GRANT SELECT ON switch_items TO example_app_switch;
                   CREATE POLICY everyone ON switch_items FOR SELECT USING (true);",
            refused: Some(permissive_policy(
                "public.switch_items policy everyone",
                switch,
            )),
            probes: &[
                (Some("example_app_switch"), SWITCH_ITEMS, Some(2)),
                (None, SWITCH_ITEMS, None),
            ],
            fix: "DROP POLICY everyone ON switch_items;
                  REVOKE SELECT ON switch_items FROM example_app_switch;",
        },
        SetRoleCase {
            case: "a permissive policy for a role a SET ROLE target inherits",
            session: app,
            make: "GRANT SELECT ON switch_items TO example_app_switch;
                   CREATE POLICY via_group ON switch_items FOR SELECT TO example_app_switch_group
                       USING (true);",
            refused: Some(permissive_policy(
                "public.switch_items policy via_group",
                switch,
            )),
            probes: &[(Some("example_app_switch"), SWITCH_ITEMS, Some(2))],
            fix: "DROP POLICY via_group ON switch_items;
                  REVOKE SELECT ON switch_items FROM example_app_switch;",
        },
        SetRoleCase {
            // Postgres applies a policy to a role that inherits one of the policy's roles; the
            // SET ROLE target belongs to example_app_switch_parent without inheriting it, and
            // example_app_switch_parent itself may not use the table.
            case: "a permissive policy for a role a SET ROLE target belongs to without inheriting",
            session: app,
            make: "GRANT SELECT ON switch_items TO example_app_switch;
                   CREATE POLICY parent_only ON switch_items FOR SELECT
                       TO example_app_switch_parent USING (true);",
            refused: None,
            probes: &[
                (Some("example_app_switch"), SWITCH_ITEMS, Some(0)),
                (Some("example_app_switch_parent"), SWITCH_ITEMS, None),
            ],
            fix: "DROP POLICY parent_only ON switch_items;
                  REVOKE SELECT ON switch_items FROM example_app_switch;",
        },
        SetRoleCase {
            case: "a view without security_invoker that a SET ROLE target may use",
            session: app,
            make: "GRANT SELECT ON item_names TO example_app_switch;",
            refused: Some(UnsafeSession::UnprotectedView {
                view: "public.item_names".to_owned(),
                role: switch.to_owned(),
            }),
            probes: &[(
                Some("example_app_switch"),
                "SELECT name FROM item_names",
                Some(3),
            )],
            fix: "REVOKE SELECT ON item_names FROM example_app_switch;",
        },
        SetRoleCase {
            case: "a materialized view that a SET ROLE target may use",
            session: app,
            make: "GRANT SELECT ON item_counts TO example_app_switch;",
            refused: Some(UnsafeSession::Unprotectable {
                relation: "public.item_counts".to_owned(),
                role: switch.to_owned(),
            }),
            probes: &[(
                Some("example_app_switch"),
                "SELECT items FROM item_counts",
                Some(2),
            )],
            fix: "REVOKE SELECT ON item_counts FROM example_app_switch;",
        },
        SetRoleCase {
            case: "TRUNCATE for a SET ROLE target",
            session: app,
            make: "GRANT TRUNCATE ON example_items TO example_app_switch;",
            refused: Some(UnsafeSession::TruncateAllowed {
                table: "public.example_items".to_owned(),
                role: switch.to_owned(),
            }),
            probes: &[
                (
                    Some("example_app_switch"),
                    "TRUNCATE example_items",
                    Some(0),
                ),
                (None, "TRUNCATE example_items", None),
            ],
            fix: "REVOKE TRUNCATE ON example_items FROM example_app_switch;",
        },
        SetRoleCase {
            case: "a SECURITY DEFINER function a SET ROLE target may execute",
            session: app,
            make: "GRANT EXECUTE ON FUNCTION all_item_names() TO example_app_switch;",
            refused: Some(UnsafeSession::DefinerFunction {
                function: "all_item_names()".to_owned(),
                role: switch.to_owned(),
            }),
            probes: &[
                (
                    Some("example_app_switch"),
                    "SELECT all_item_names()",
                    Some(3),
                ),
                (None, "SELECT all_item_names()", None),
            ],
            fix: "REVOKE EXECUTE ON FUNCTION all_item_names() FROM example_app_switch;",
        },
        SetRoleCase {
            case: "temporary tables for a SET ROLE target",
            session: app,
            make: "DO $$ BEGIN
                       EXECUTE format('GRANT TEMPORARY ON DATABASE %I TO example_app_switch',
                                      current_database());
                   END $$;",
            refused: Some(UnsafeSession::TemporaryTables {
                role: switch.to_owned(),
            }),
            probes: &[
                (Some("example_app_switch"), KEEP_ITEMS, Some(0)),
                (None, KEEP_ITEMS, None),
            ],
            fix: "DO $$ BEGIN
                      EXECUTE format('REVOKE TEMPORARY ON DATABASE %I FROM example_app_switch',
                                     current_database());
                  END $$;",
        },
        SetRoleCase {
            case: "a table without row-level security that the platform role's SET ROLE target \
                   may use",
            session: platform,
            make: "GRANT SELECT ON loose_items TO example_platform_switch;",
            refused: Some(unprotected_table(loose, "example_platform_switch")),
            probes: &[
                (Some("example_platform_switch"), LOOSE_ITEMS, Some(2)),
                (None, LOOSE_ITEMS, None),
            ],
            fix: "REVOKE SELECT ON loose_items FROM example_platform_switch;",
        },
        // Memberships without SET keep the results the check gave before.
        SetRoleCase {
            case: "a privilege of a role the session only inherits (refused as before)",
            session: app,
            make: "GRANT SELECT ON loose_items TO example_app_inherited;",
            refused: Some(unprotected_table(loose, "example_app")),
            probes: &[
                (None, LOOSE_ITEMS, Some(2)),
                (Some("example_app_inherited"), LOOSE_ITEMS, None),
            ],
            fix: "REVOKE SELECT ON loose_items FROM example_app_inherited;",
        },
        SetRoleCase {
            case: "privileges of a role the session can neither inherit nor become (accepted as \
                   before)",
            session: app,
            make: "GRANT SELECT, TRUNCATE ON loose_items TO example_app_unusable;
                   GRANT SELECT ON item_names TO example_app_unusable;
                   GRANT EXECUTE ON FUNCTION all_item_names() TO example_app_unusable;
                   DO $$ BEGIN
                       EXECUTE format('GRANT TEMPORARY ON DATABASE %I TO example_app_unusable',
                                      current_database());
                   END $$;",
            refused: None,
            probes: &[
                (None, LOOSE_ITEMS, None),
                (None, "SELECT all_item_names()", None),
                (None, KEEP_ITEMS, None),
                (Some("example_app_unusable"), LOOSE_ITEMS, None),
            ],
            fix: "REVOKE SELECT, TRUNCATE ON loose_items FROM example_app_unusable;
                  REVOKE SELECT ON item_names FROM example_app_unusable;
                  REVOKE EXECUTE ON FUNCTION all_item_names() FROM example_app_unusable;
                  DO $$ BEGIN
                      EXECUTE format('REVOKE TEMPORARY ON DATABASE %I FROM example_app_unusable',
                                     current_database());
                  END $$;",
        },
        SetRoleCase {
            // SET ROLE needs the SET option on every membership of the chain.
            case: "a role behind a membership the session only inherits, with SET from there \
                   (accepted as before)",
            session: app,
            make: "GRANT SELECT ON loose_items TO example_app_inherited_switch;",
            refused: None,
            probes: &[
                (Some("example_app_inherited_switch"), LOOSE_ITEMS, None),
                (None, LOOSE_ITEMS, None),
            ],
            fix: "REVOKE SELECT ON loose_items FROM example_app_inherited_switch;",
        },
        SetRoleCase {
            // The policy does not apply to the session (it does not inherit the policy's role),
            // but the check has always counted any membership for the session's own role.
            case: "a permissive policy for a role the session can neither inherit nor become, on \
                   a table it may use (refused as before)",
            session: app,
            make: "CREATE POLICY unusable_all ON example_items TO example_app_unusable
                       USING (true);",
            refused: Some(permissive_policy(
                "public.example_items policy unusable_all",
                "example_app",
            )),
            probes: &[(None, "SELECT name FROM example_items", Some(0))],
            fix: "DROP POLICY unusable_all ON example_items;",
        },
    ];
    for case in cases {
        let SetRoleCase {
            case,
            session,
            make,
            refused,
            probes,
            fix,
        } = case;
        sqlx::raw_sql(make)
            .execute(&mut owner)
            .await
            .unwrap_or_else(|error| panic!("{case}: {error}"));
        assert_eq!(
            check_login(&fixture, session).await,
            refused.clone().map_or(Ok(()), Err),
            "{case}"
        );
        let mut reached = Vec::new();
        for &(role, sql, expected) in probes {
            let result = try_reach_as(&fixture, session.0, role, None, sql).await;
            match (&result, expected) {
                (Ok(rows), Some(expected)) => assert_eq!(*rows, expected, "{case}: {sql}"),
                (Err(error), None) => assert_eq!(
                    sqlstate(error).as_deref(),
                    Some(INSUFFICIENT_PRIVILEGE),
                    "{case}: {sql}: {error}"
                ),
                _ => panic!("{case}: as {role:?}, {sql}: {result:?}, expected {expected:?}"),
            }
            reached.push(format!(
                "as {}: {}",
                role.unwrap_or(session.0),
                result.map_or_else(|_| "refused".to_owned(), |rows| format!("{rows} rows"))
            ));
        }
        sqlx::raw_sql(fix)
            .execute(&mut owner)
            .await
            .unwrap_or_else(|error| panic!("{case}: {error}"));
        assert_eq!(check_login(&fixture, session).await, Ok(()), "{case} fixed");
        let verdict = refused.map_or_else(
            || "accepted".to_owned(),
            |problem| format!("refused with \"{problem}\""),
        );
        println!(
            "{case}: {verdict}; without a tenant {}; accepted once fixed",
            reached.join(", ")
        );
    }

    // A SECURITY DEFINER function owned by a role the session belongs to without inheriting or
    // becoming it runs with rights the check does not judge, so it is refused like one of any
    // other owner (before Laysi/trustiot-v2#381, any membership in the owner exempted it).
    let mut superuser = PgConnection::connect_with(&fixture.postgres.superuser("set_role"))
        .await
        .expect("connect as the superuser");
    sqlx::raw_sql(
        "GRANT SELECT ON loose_items TO example_app_unusable;
         CREATE FUNCTION unusable_names() RETURNS SETOF text LANGUAGE sql SECURITY DEFINER
             AS 'SELECT name FROM public.loose_items';
         ALTER FUNCTION unusable_names() OWNER TO example_app_unusable;",
    )
    .execute(&mut superuser)
    .await
    .expect("a definer function of a role the service role cannot use");
    let refused = check_login(&fixture, app).await;
    assert_eq!(
        refused,
        Err(UnsafeSession::DefinerFunction {
            function: "unusable_names()".to_owned(),
            role: "example_app".to_owned(),
        })
    );
    let through_function = try_reach_as(&fixture, app.0, None, None, "SELECT unusable_names()")
        .await
        .expect("the function runs with its owner's rights");
    assert_eq!(through_function, 2, "both tenants' rows");
    sqlx::raw_sql(
        "DROP FUNCTION unusable_names();
         REVOKE SELECT ON loose_items FROM example_app_unusable;",
    )
    .execute(&mut superuser)
    .await
    .expect("drop the function");
    assert_eq!(check_login(&fixture, app).await, Ok(()));
    println!(
        "a SECURITY DEFINER function of a role the session can neither inherit nor become: \
         refused with {refused:?}; through it the session reads {through_function} rows without \
         a tenant; accepted once dropped"
    );

    // The ADMIN option: a role that administers another can grant itself that role with SET
    // and take its rights, whatever the options of its own membership, so it is refused.
    sqlx::raw_sql("GRANT SELECT ON loose_items TO example_app_switch;")
        .execute(&mut owner)
        .await
        .expect("grant");
    let administrator = ("example_administrator", SessionRole::Service);
    let refused = check_login(&fixture, administrator).await;
    assert_eq!(
        refused,
        Err(UnsafeSession::AdministersRole {
            role: "example_app_switch".to_owned(),
        })
    );
    let before = try_reach_as(
        &fixture,
        administrator.0,
        Some("example_app_switch"),
        None,
        LOOSE_ITEMS,
    )
    .await
    .expect_err("its own membership has no SET option");
    assert_eq!(
        sqlstate(&before).as_deref(),
        Some(INSUFFICIENT_PRIVILEGE),
        "{before}"
    );
    let mut session = fixture.connect(administrator.0).await;
    let mut tx = session.begin().await.expect("begin");
    sqlx::raw_sql("GRANT example_app_switch TO example_administrator WITH INHERIT FALSE, SET TRUE")
        .execute(&mut *tx)
        .await
        .expect("the administrator grants itself the role");
    sqlx::query("SELECT set_config('role', 'example_app_switch', true)")
        .execute(&mut *tx)
        .await
        .expect("and takes it");
    let rows = sqlx::query(LOOSE_ITEMS)
        .execute(&mut *tx)
        .await
        .expect("read")
        .rows_affected();
    tx.rollback().await.expect("rollback");
    assert_eq!(rows, 2, "both tenants' rows");
    sqlx::raw_sql("REVOKE SELECT ON loose_items FROM example_app_switch;")
        .execute(&mut owner)
        .await
        .expect("revoke");
    println!(
        "a role with the ADMIN option on another: refused with {refused:?}; SET ROLE first \
         refused ({before}), then after granting itself the role it reads {rows} rows without a \
         tenant"
    );
}

/// The session check of a plain session of `login`, with the rules of `role`.
async fn check_login(
    fixture: &Fixture,
    (login, role): (&str, SessionRole),
) -> Result<(), UnsafeSession> {
    let mut session = fixture.connect(login).await;
    postgres::check_session(&mut session, role)
        .await
        .expect("the check runs")
}

/// The outbox template (`sql/outbox-event.sql`): the service role writes events of its tenant
/// only, platform-wide events come only from the platform role, which writes no tenant's events
/// even after setting a tenant itself, and a consumer's platform role reads every tenant's
/// events. The connection check accepts every role's session.
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

    // Nor after setting a tenant itself (Laysi/trustiot-v2#360): `tenant_isolation` applies to
    // every role, so only the template's restrictive `platform_events_only` stops this. First
    // as platform code that went wrong would do it, inside a PlatformDb transaction...
    for tenant in [TENANT_A, TENANT_B] {
        let mut tx = platform.begin(actor, "outbox-test").await.expect("begin");
        sqlx::query(SET_TRANSACTION_TENANT)
            .bind(tenant)
            .execute(&mut *tx)
            .await
            .expect("set a tenant in the platform transaction");
        let error = sqlx::query(PUBLISH)
            .bind(tenant)
            .execute(&mut *tx)
            .await
            .expect_err("the platform role writes no tenant's events after setting the tenant");
        assert_eq!(
            sqlstate(&error).as_deref(),
            Some(INSUFFICIENT_PRIVILEGE),
            "{error}"
        );
        println!("platform role, tenant set to {tenant} in its transaction: refused ({error})");
        drop(tx);
    }
    // ...and with no service-kit involved: a plain session of the platform role, with the tenant
    // set for the transaction or for the session.
    let mut session = fixture.connect("example_platform").await;
    for set in [SET_TRANSACTION_TENANT, SET_SESSION_TENANT] {
        let mut tx = session.begin().await.expect("begin");
        sqlx::query(set)
            .bind(TENANT_A)
            .execute(&mut *tx)
            .await
            .expect("set a tenant");
        let error = sqlx::query(PUBLISH)
            .bind(TENANT_A)
            .execute(&mut *tx)
            .await
            .expect_err("a plain platform session writes no tenant's events");
        assert_eq!(
            sqlstate(&error).as_deref(),
            Some(INSUFFICIENT_PRIVILEGE),
            "{set}: {error}"
        );
        tx.rollback().await.expect("rollback");
        println!("plain platform session, `{set}`: refused ({error})");
    }
    // With a tenant set, a platform-wide event is still what the platform role may write.
    let mut tx = platform.begin(actor, "outbox-test").await.expect("begin");
    sqlx::query(SET_TRANSACTION_TENANT)
        .bind(TENANT_A)
        .execute(&mut *tx)
        .await
        .expect("set a tenant");
    sqlx::query(PUBLISH)
        .bind(None::<&str>)
        .execute(&mut *tx)
        .await
        .expect("a platform-wide event with a tenant set");
    tx.rollback().await.expect("rollback");

    // Service-kit's connection check accepts every role's session under the template: the
    // restrictive policy only narrows what the platform role may write.
    for (role, as_role) in [
        ("example_app", SessionRole::Service),
        ("example_platform", SessionRole::Platform),
        ("example_reader", SessionRole::Platform),
    ] {
        let mut session = fixture.connect(role).await;
        assert_eq!(
            postgres::check_session(&mut session, as_role)
                .await
                .expect("check"),
            Ok(()),
            "{role} as {as_role:?}"
        );
    }

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
        Err(UnsafeSession::PermissivePolicy {
            policy: "public.outbox_event policy consumer_read".to_owned(),
            role: "example_reader".to_owned(),
        })
    );

    // A service that consumes its own events reads them as its own platform role: the template
    // with `example_reader` replaced by `example_platform` gives the producer's platform role
    // this policy and grant too. The session check accepts it as producer and reader at once
    // (Laysi/trustiot-v2#366: SELECT is opened by consumer_read_own, INSERT by platform_events).
    sqlx::raw_sql(
        "CREATE POLICY consumer_read_own ON outbox_event FOR SELECT TO example_platform
             USING (true);
         GRANT SELECT ON outbox_event TO example_platform;",
    )
    .execute(&mut owner)
    .await
    .expect("the producer's platform role reads its own events");
    for (role, as_role) in [
        ("example_app", SessionRole::Service),
        ("example_platform", SessionRole::Platform),
        ("example_reader", SessionRole::Platform),
    ] {
        let mut session = fixture.connect(role).await;
        assert_eq!(
            postgres::check_session(&mut session, as_role)
                .await
                .expect("check"),
            Ok(()),
            "{role} as {as_role:?}, the producer's platform role also reading"
        );
    }
    let own_reader = PlatformDb::connect_lazy(
        fixture.options("example_platform"),
        PgPoolOptions::new().max_connections(1),
    )
    .expect("verify-full options");
    let mut tx = own_reader
        .begin(actor, "outbox-delivery")
        .await
        .expect("begin");
    let tenants: Vec<Option<String>> =
        sqlx::query_scalar("SELECT tenant_id::text FROM outbox_event ORDER BY id")
            .fetch_all(&mut *tx)
            .await
            .expect("read events");
    sqlx::query(PUBLISH)
        .bind(None::<&str>)
        .execute(&mut *tx)
        .await
        .expect("a platform-wide event from the same session");
    tx.rollback().await.expect("rollback");
    assert_eq!(tenants, [Some(TENANT_A.to_owned()), None]);
    println!(
        "outbox: the producer's platform role that also reads its own events passes the session \
         check and reads every event"
    );
    println!(
        "outbox: tenant events by the service role, platform-wide by the platform role, all \
         read by the consumer's platform role"
    );
}

/// The outbox tables as crates/outbox created them before `sql/outbox-aggregate.sql`
/// (Laysi/trustiot-v2#413), with the columns the template reads, and what an older producer
/// wrote into them: two events of tenant A's aggregate, one of tenant B's, a platform-wide one,
/// and an aggregate without an event, which `publish` never leaves.
const OUTBOX_BEFORE_THE_AGGREGATE_TEMPLATE: &str = "
CREATE TABLE outbox_aggregate (
    aggregate_type text NOT NULL,
    aggregate_id text NOT NULL,
    last_seq bigint NOT NULL,
    last_tx bigint NOT NULL,
    PRIMARY KEY (aggregate_type, aggregate_id)
);
CREATE TABLE outbox_event (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    tenant_id uuid,
    aggregate_type text NOT NULL,
    aggregate_id text NOT NULL,
    aggregate_seq bigint NOT NULL,
    UNIQUE (aggregate_type, aggregate_id, aggregate_seq)
);
INSERT INTO outbox_event (tenant_id, aggregate_type, aggregate_id, aggregate_seq) VALUES
    ('01928f6e-7a3c-7d4e-8f00-00000000000a', 'widget', 'a', 1),
    ('01928f6e-7a3c-7d4e-8f00-00000000000a', 'widget', 'a', 2),
    ('01928f6e-7a3c-7d4e-8f00-00000000000b', 'widget', 'b', 1),
    (NULL, 'catalog', 'c', 1);
INSERT INTO outbox_aggregate (aggregate_type, aggregate_id, last_seq, last_tx) VALUES
    ('widget', 'a', 2, 2), ('widget', 'b', 1, 3), ('catalog', 'c', 1, 4), ('widget', 'eventless', 1, 5);
";

/// The upsert `outbox::publish` runs: `$1` kind, `$2` id, `$3` the event's tenant.
const UPSERT_AGGREGATE: &str = "INSERT INTO outbox_aggregate AS a
    (aggregate_type, aggregate_id, tenant_id, last_seq, last_tx)
    VALUES ($1, $2, $3::uuid, 1, pg_current_xact_id()::text::bigint)
    ON CONFLICT (aggregate_type, aggregate_id) DO UPDATE
    SET last_seq = a.last_seq + 1, last_tx = GREATEST(a.last_tx, EXCLUDED.last_tx)
    RETURNING last_seq";

/// The same upsert as crates/outbox wrote it before the tenant column existed.
const UPSERT_AGGREGATE_BEFORE_413: &str = "INSERT INTO outbox_aggregate AS a
    (aggregate_type, aggregate_id, last_seq, last_tx)
    VALUES ($1, $2, 1, pg_current_xact_id()::text::bigint)
    ON CONFLICT (aggregate_type, aggregate_id) DO UPDATE
    SET last_seq = a.last_seq + 1, last_tx = GREATEST(a.last_tx, EXCLUDED.last_tx)
    RETURNING last_seq";

const VISIBLE_AGGREGATES: &str =
    "SELECT aggregate_type || '/' || aggregate_id FROM outbox_aggregate ORDER BY 1";

/// Every aggregate as the owner sees it: kind/id, tenant, last sequence number.
async fn aggregates_as_owner(fixture: &Fixture) -> Vec<(String, Option<String>, i64)> {
    let mut owner = fixture.connect("example_owner").await;
    sqlx::query_as(
        "SELECT aggregate_type || '/' || aggregate_id, tenant_id::text, last_seq
         FROM outbox_aggregate ORDER BY 1",
    )
    .fetch_all(&mut owner)
    .await
    .expect("the owner reads every aggregate")
}

/// The outbox aggregate template (`sql/outbox-aggregate.sql`, Laysi/trustiot-v2#413), run on
/// tables that already hold aggregates, as in a database that ran the outbox before it: every
/// aggregate gets the tenant of its last event. Then the service role reads and advances only
/// its tenant's aggregates: the upsert `publish` runs is refused (42501) for another tenant's or
/// a platform aggregate instead of advancing it, and a writer that leaves the tenant out (a
/// producer still on the version before the template) gets the transaction's tenant. The
/// platform role reaches only platform aggregates, even after setting a tenant itself. The
/// connection check accepts every role's session.
#[tokio::test]
async fn outbox_aggregates_follow_the_tenant_policies() {
    let Some(fixture) = Fixture::new(
        "CREATE DATABASE outbox_aggregate OWNER example_owner",
        "outbox_aggregate",
    )
    .await
    else {
        return;
    };
    let mut owner = fixture.connect("example_owner").await;
    sqlx::raw_sql(OUTBOX_BEFORE_THE_AGGREGATE_TEMPLATE)
        .execute(&mut owner)
        .await
        .expect("the outbox tables before the template");
    sqlx::raw_sql(include_str!("../sql/outbox-aggregate.sql"))
        .execute(&mut owner)
        .await
        .expect("run the outbox aggregate template");
    let some = |id: &str| Some(id.to_owned());
    assert_eq!(
        aggregates_as_owner(&fixture).await,
        [
            ("catalog/c".to_owned(), None, 1),
            ("widget/a".to_owned(), some(TENANT_A), 2),
            ("widget/b".to_owned(), some(TENANT_B), 1),
            ("widget/eventless".to_owned(), None, 1),
        ],
        "each aggregate has the tenant of its last event"
    );
    let policies: Vec<(String, bool, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT polname::text, polpermissive, pg_get_expr(polqual, polrelid),
                pg_get_expr(polwithcheck, polrelid)
         FROM pg_policy WHERE polrelid = 'outbox_aggregate'::regclass ORDER BY 1",
    )
    .fetch_all(&mut owner)
    .await
    .expect("read the policies");
    let null = Some("(tenant_id IS NULL)".to_owned());
    let tenant_condition = Some(postgres::TENANT_CONDITION_AS_STORED.to_owned());
    assert_eq!(
        policies,
        [
            (
                "platform_aggregates".to_owned(),
                true,
                null.clone(),
                null.clone()
            ),
            (
                "platform_aggregates_only".to_owned(),
                false,
                null.clone(),
                null
            ),
            (
                "tenant_isolation".to_owned(),
                true,
                tenant_condition.clone(),
                tenant_condition
            ),
        ]
    );

    // The connection check accepts the producer's roles, and the consumer's platform role, which
    // gets nothing on this table.
    for (role, as_role) in [
        ("example_app", SessionRole::Service),
        ("example_platform", SessionRole::Platform),
        ("example_reader", SessionRole::Platform),
    ] {
        let mut session = fixture.connect(role).await;
        assert_eq!(
            postgres::check_session(&mut session, as_role)
                .await
                .expect("check"),
            Ok(()),
            "{role} as {as_role:?}"
        );
    }

    // The service role in tenant A.
    let identity = Identity::new();
    let db = fixture.tenant_db(1);
    let mut tx = db
        .begin(&identity.user(Some(TENANT_A), false))
        .await
        .expect("begin");
    let visible: Vec<String> = sqlx::query_scalar(VISIBLE_AGGREGATES)
        .fetch_all(&mut *tx)
        .await
        .expect("read as A");
    assert_eq!(visible, ["widget/a"], "A sees its own aggregate only");
    let seq: i64 = sqlx::query_scalar(UPSERT_AGGREGATE)
        .bind("widget")
        .bind("a")
        .bind(TENANT_A)
        .fetch_one(&mut *tx)
        .await
        .expect("A advances its aggregate");
    assert_eq!(seq, 3);
    let old: i64 = sqlx::query_scalar(UPSERT_AGGREGATE_BEFORE_413)
        .bind("widget")
        .bind("old-binary")
        .fetch_one(&mut *tx)
        .await
        .expect("a producer on the version before the template writes a new aggregate");
    assert_eq!(old, 1);
    tx.commit().await.expect("commit");
    for (aggregate, owned_by) in [("b", "tenant B"), ("eventless", "the platform")] {
        let mut tx = db
            .begin(&identity.user(Some(TENANT_A), false))
            .await
            .expect("begin");
        let error = sqlx::query_scalar::<_, i64>(UPSERT_AGGREGATE)
            .bind("widget")
            .bind(aggregate)
            .bind(TENANT_A)
            .fetch_one(&mut *tx)
            .await
            .expect_err("A advances an aggregate that is not A's");
        assert_eq!(
            sqlstate(&error).as_deref(),
            Some(INSUFFICIENT_PRIVILEGE),
            "{owned_by}: {error}"
        );
        println!(
            "service role in tenant A, upsert of an aggregate of {owned_by}: refused ({error})"
        );
        drop(tx);
    }
    let visible: Vec<String> = sqlx::query_scalar(VISIBLE_AGGREGATES)
        .fetch_all(db.pool())
        .await
        .expect("read without a tenant");
    assert!(
        visible.is_empty(),
        "no aggregate without a tenant: {visible:?}"
    );

    // The platform role: platform aggregates only, also after setting a tenant itself, in a
    // PlatformDb transaction and on a plain session (Laysi/trustiot-v2#360).
    let platform = PlatformDb::connect_lazy(
        fixture.options("example_platform"),
        PgPoolOptions::new().max_connections(1),
    )
    .expect("verify-full options");
    let actor = PlatformActor::service(ServiceIdentity::new(ServiceName::Identity));
    let mut tx = platform.begin(actor, "outbox-test").await.expect("begin");
    let seq: i64 = sqlx::query_scalar(UPSERT_AGGREGATE)
        .bind("catalog")
        .bind("c")
        .bind(None::<&str>)
        .fetch_one(&mut *tx)
        .await
        .expect("the platform role advances a platform aggregate");
    assert_eq!(seq, 2);
    tx.commit().await.expect("commit");
    for set in [None, Some(SET_TRANSACTION_TENANT)] {
        let mut tx = platform.begin(actor, "outbox-test").await.expect("begin");
        if let Some(set) = set {
            sqlx::query(set)
                .bind(TENANT_A)
                .execute(&mut *tx)
                .await
                .expect("set a tenant in the platform transaction");
        }
        let visible: Vec<String> = sqlx::query_scalar(VISIBLE_AGGREGATES)
            .fetch_all(&mut *tx)
            .await
            .expect("read as the platform role");
        assert_eq!(
            visible,
            ["catalog/c", "widget/eventless"],
            "the platform role, tenant set: {}",
            set.is_some()
        );
        let error = sqlx::query_scalar::<_, i64>(UPSERT_AGGREGATE)
            .bind("widget")
            .bind("a")
            .bind(set.map(|_| TENANT_A))
            .fetch_one(&mut *tx)
            .await
            .expect_err("the platform role advances tenant A's aggregate");
        assert_eq!(
            sqlstate(&error).as_deref(),
            Some(INSUFFICIENT_PRIVILEGE),
            "{error}"
        );
        println!(
            "platform role, tenant set: {}, upsert of tenant A's aggregate: refused ({error})",
            set.is_some()
        );
        drop(tx);
    }
    let mut session = fixture.connect("example_platform").await;
    for set in [SET_TRANSACTION_TENANT, SET_SESSION_TENANT] {
        let mut tx = session.begin().await.expect("begin");
        sqlx::query(set)
            .bind(TENANT_A)
            .execute(&mut *tx)
            .await
            .expect("set a tenant");
        let visible: Vec<String> = sqlx::query_scalar(VISIBLE_AGGREGATES)
            .fetch_all(&mut *tx)
            .await
            .expect("read on a plain platform session");
        assert_eq!(visible, ["catalog/c", "widget/eventless"], "`{set}`");
        let error = sqlx::query_scalar::<_, i64>(UPSERT_AGGREGATE)
            .bind("widget")
            .bind("new")
            .bind(TENANT_A)
            .fetch_one(&mut *tx)
            .await
            .expect_err("a plain platform session creates a tenant's aggregate");
        assert_eq!(
            sqlstate(&error).as_deref(),
            Some(INSUFFICIENT_PRIVILEGE),
            "`{set}`: {error}"
        );
        tx.rollback().await.expect("rollback");
        println!("plain platform session, `{set}`: tenant A's aggregates hidden, writes refused");
    }

    // Only the writes that were allowed changed anything.
    assert_eq!(
        aggregates_as_owner(&fixture).await,
        [
            ("catalog/c".to_owned(), None, 2),
            ("widget/a".to_owned(), some(TENANT_A), 3),
            ("widget/b".to_owned(), some(TENANT_B), 1),
            ("widget/eventless".to_owned(), None, 1),
            ("widget/old-binary".to_owned(), some(TENANT_A), 1),
        ],
        "a writer that leaves the tenant out gets the transaction's tenant"
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
            .wait_for_message("invalid configuration; not starting", OUTPUT_DRAIN_TIMEOUT)
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

/// Without a CA for the database server the example service refuses to start: no public or
/// system root would stand in for it (Laysi/trustiot-v2#272).
#[test]
fn a_service_without_a_postgres_ca_refuses_to_start() {
    for ca_file in [None, Some("")] {
        let mut env = vec![
            ("TRUSTIOT__DATABASE__USERNAME", "example_app"),
            ("TRUSTIOT__DATABASE__PASSWORD", "service-kit-test"),
            ("TRUSTIOT_POSTGRES_HOST", "postgres.example.internal"),
            ("TRUSTIOT_POSTGRES_PORT", "5432"),
            ("TRUSTIOT_POSTGRES_DATABASE", "trustiot"),
            ("TRUSTIOT_POSTGRES_SSLMODE", "verify-full"),
        ];
        if let Some(ca_file) = ca_file {
            env.push(("TRUSTIOT_POSTGRES_CA_FILE", ca_file));
        }
        let (mut child, stdout, _stderr) = spawn_example(&env);
        let status =
            wait_exit(&mut child, Duration::from_secs(10)).expect("the service exits by itself");
        assert_eq!(status.code(), Some(1), "CA file {ca_file:?}");
        let error = stdout
            .wait_for_message(
                "invalid configuration; not starting",
                Duration::from_secs(2),
            )
            .unwrap_or_else(|| panic!("CA file {ca_file:?}: the error is logged"));
        let text = error["fields"]["error"].as_str().expect("error text");
        assert!(text.contains("platform.postgres_ca_file"), "{text}");
        assert!(text.contains("is required"), "{text}");
        assert!(
            !stdout.text().contains("service-kit-test"),
            "the password is logged"
        );
        println!("CA file {ca_file:?}: {status}; error: {text}");
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
    let keys_file = support::keys_file(&identity.jwks());
    let port = fixture.postgres.address.port().to_string();
    let env = |ca_file: &std::path::Path| {
        vec![
            (support::KEYS_FILE_VARIABLE.to_owned(), keys_file.clone()),
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
    let (stdout, stderr) = (service.stdout.clone(), service.stderr.clone());
    let tokens = [
        identity.token(Some(TENANT_A), false),
        identity.token(Some(TENANT_B), false),
        identity.token(None, false),
    ];
    let responses = tokio::task::spawn_blocking(move || {
        wait_until_ready(address, &stdout, &stderr);
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
