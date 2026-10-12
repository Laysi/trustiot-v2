//! The outbox wiring (crates/outbox, crates/service-kit/sql/outbox-event.sql) and the per-tenant
//! role hook, with a test registry: a producer, its consumers, and a hook over the producer's
//! tenant table. The product has no producer yet (Laysi/trustiot-v2#65 adds the first).
//!
//! Needs podman or docker; skipped without one unless `CI` is set.

mod support;

use migrate::registry::{Migration, Outbox, Registry, Service, TenantRoleHook};
use service_kit::postgres::{SessionRole, check_session};
use sqlx::{Connection, PgConnection};
use support::postgres::tls_postgres;
use support::{Database, MIGRATOR, MIGRATOR_INIT, random_hex, snapshot};

const CREATE: &str = "CREATE DATABASE migrate_outbox OWNER trustiot_owner";

const PRODUCER_MIGRATIONS: &[Migration] = &[Migration {
    name: "0001_tenants.sql",
    sql: "CREATE TABLE tenants (id uuid PRIMARY KEY);",
}];

const HOOK: TenantRoleHook = TenantRoleHook {
    name: "test-tenants",
    service: "obx-producer",
    tenants: "SELECT id::text FROM obx_producer.tenants ORDER BY id",
    role_prefix: "obx_tenant_",
    template: "GRANT USAGE ON SCHEMA obx_producer TO example_tenant_role;
SELECT example_tenant_id::uuid",
};

const PRODUCER: Service = Service {
    name: "obx-producer",
    platform_role: true,
    outbox: Outbox {
        producer: true,
        consumes: &[],
    },
    migrations: PRODUCER_MIGRATIONS,
};

const CONSUMER: Service = Service {
    name: "obx-consumer",
    platform_role: true,
    outbox: Outbox {
        producer: false,
        consumes: &["obx-producer"],
    },
    migrations: &[],
};

const SECOND: Service = Service {
    name: "obx-second",
    platform_role: true,
    outbox: Outbox {
        producer: false,
        consumes: &["obx-producer"],
    },
    migrations: &[],
};

const ONE_CONSUMER: Registry = Registry {
    services: &[PRODUCER, CONSUMER],
    clickhouse: &[],
    tenant_role_hooks: &[HOOK],
};

const TWO_CONSUMERS: Registry = Registry {
    services: &[PRODUCER, CONSUMER, SECOND],
    clickhouse: &[],
    tenant_role_hooks: &[HOOK],
};

const LAST_CONSUMER: Registry = Registry {
    services: &[PRODUCER, SECOND],
    clickhouse: &[],
    tenant_role_hooks: &[HOOK],
};

const LOGIN_ROLES: [&str; 6] = [
    "obx_producer_app",
    "obx_producer_platform",
    "obx_consumer_app",
    "obx_consumer_platform",
    "obx_second_app",
    "obx_second_platform",
];

const POLICIES: &str = "SELECT polname::text FROM pg_policy
WHERE polrelid = 'obx_producer.outbox_event'::regclass ORDER BY 1";
const READERS: &str = "SELECT ARRAY(SELECT rolname::text FROM pg_roles r JOIN pg_policy p
    ON r.oid = ANY (p.polroles)
    WHERE p.polrelid = 'obx_producer.outbox_event'::regclass AND p.polname = 'consumer_read'
    ORDER BY 1)";
const CAN_READ_EVENTS: &str =
    "SELECT has_table_privilege($1, 'obx_producer.outbox_event', 'SELECT')";
const TABLE_OWNER: &str =
    "SELECT tableowner::text FROM pg_tables WHERE schemaname = $1 AND tablename = $2";
const CONSUMER_WRITES_INBOX: &str =
    "SELECT has_table_privilege('obx_consumer_app', 'obx_consumer.outbox_inbox', 'INSERT, DELETE')";
const SEED_TENANTS: &str = "INSERT INTO obx_producer.tenants (id) VALUES
    ('00000000-0000-0000-0000-00000000000a'), ('00000000-0000-0000-0000-00000000000b')";
const TENANT_A: &str = "obx_tenant_0000000000000000000000000000000a";
const TENANT_B: &str = "obx_tenant_0000000000000000000000000000000b";
const TENANT_USAGE: &str = "SELECT has_schema_privilege($1, 'obx_producer', 'USAGE')";
const DROP_TENANT_A: &str = "DROP OWNED BY obx_tenant_0000000000000000000000000000000a;
DROP ROLE obx_tenant_0000000000000000000000000000000a";

async fn check_sessions(db: &Database, passwords: &[(&str, String)]) {
    for (role, password) in passwords {
        let mut conn = db
            .connect_with_password(role, password)
            .await
            .unwrap_or_else(|error| panic!("{role} logs in: {error}"));
        let kind = if role.ends_with("_platform") {
            SessionRole::Platform
        } else {
            SessionRole::Service
        };
        check_session(&mut conn, kind)
            .await
            .expect("run the session check")
            .unwrap_or_else(|problem| panic!("service-kit refuses {role}: {problem:?}"));
        conn.close().await.expect("close");
    }
}

async fn readers(admin: &mut PgConnection) -> Vec<String> {
    sqlx::query_scalar(READERS)
        .fetch_one(admin)
        .await
        .expect("read the readers")
}

#[tokio::test(flavor = "multi_thread")]
async fn outbox_wiring_and_tenant_roles() {
    let Some(postgres) = tls_postgres(MIGRATOR_INIT).await else {
        return;
    };
    let db = Database::create(postgres, CREATE, "migrate_outbox").await;
    let passwords: Vec<(&str, String)> = LOGIN_ROLES
        .iter()
        .map(|role| (*role, random_hex()))
        .collect();
    for (role, password) in &passwords {
        db.set_password_file(role, password);
    }

    // 1. Producer and one consumer.
    migrate::run(&db.config(MIGRATOR), &ONE_CONSUMER)
        .await
        .expect("the first run");
    let mut admin = PgConnection::connect_with(&db.postgres.superuser(&db.name))
        .await
        .expect("connect as the superuser");
    let policies: Vec<String> = sqlx::query_scalar(POLICIES)
        .fetch_all(&mut admin)
        .await
        .expect("read the policies");
    assert_eq!(
        policies,
        [
            "consumer_read",
            "platform_events",
            "platform_events_only",
            "tenant_isolation"
        ],
        "the outbox-event.sql template, as it is on main"
    );
    assert_eq!(readers(&mut admin).await, ["obx_consumer_platform"]);
    for (schema, table, owner) in [
        ("obx_producer", "outbox_event", "obx_producer_owner"),
        ("obx_producer", "outbox_aggregate", "obx_producer_owner"),
        ("obx_producer", "tenants", "obx_producer_owner"),
        ("obx_consumer", "outbox_inbox", "obx_consumer_owner"),
    ] {
        let found: String = sqlx::query_scalar(TABLE_OWNER)
            .bind(schema)
            .bind(table)
            .fetch_one(&mut admin)
            .await
            .unwrap_or_else(|error| panic!("{schema}.{table}: {error}"));
        assert_eq!(found, owner, "{schema}.{table}");
    }
    let writes: bool = sqlx::query_scalar(CONSUMER_WRITES_INBOX)
        .fetch_one(&mut admin)
        .await
        .expect("read the consumer's grants");
    assert!(writes, "the consumer's service role writes its inbox");
    check_sessions(&db, &passwords[..4]).await;

    // 2. A second consumer joins: the policy names both, and both read the events.
    migrate::run(&db.config(MIGRATOR), &TWO_CONSUMERS)
        .await
        .expect("the run with two consumers");
    assert_eq!(
        readers(&mut admin).await,
        ["obx_consumer_platform", "obx_second_platform"]
    );
    check_sessions(&db, &passwords).await;

    // 3. Tenants appear: the hook gives each a role on the next run; a run after that changes
    //    nothing.
    sqlx::raw_sql(SEED_TENANTS)
        .execute(&mut admin)
        .await
        .expect("seed tenants");
    let report = migrate::run(&db.config(MIGRATOR), &TWO_CONSUMERS)
        .await
        .expect("the run with tenants");
    assert_eq!(
        report.changes,
        [
            format!("created role {TENANT_A}"),
            format!("created role {TENANT_B}")
        ]
    );
    for role in [TENANT_A, TENANT_B] {
        let usage: bool = sqlx::query_scalar(TENANT_USAGE)
            .bind(role)
            .fetch_one(&mut admin)
            .await
            .expect("read the tenant role's grant");
        assert!(usage, "{role} got the hook's grant");
    }
    let before = snapshot(&db).await;
    let report = migrate::run(&db.config(MIGRATOR), &TWO_CONSUMERS)
        .await
        .expect("the repeated run");
    assert!(report.changes.is_empty(), "{:?}", report.changes);
    assert_eq!(snapshot(&db).await, before);

    // 4. A restored database without its cluster-wide roles: the next run rebuilds them (M8).
    sqlx::raw_sql(DROP_TENANT_A)
        .execute(&mut admin)
        .await
        .expect("drop a tenant role");
    let report = migrate::run(&db.config(MIGRATOR), &TWO_CONSUMERS)
        .await
        .expect("the run that rebuilds the role");
    assert_eq!(report.changes, [format!("created role {TENANT_A}")]);
    let usage: bool = sqlx::query_scalar(TENANT_USAGE)
        .bind(TENANT_A)
        .fetch_one(&mut admin)
        .await
        .expect("read the rebuilt role's grant");
    assert!(usage);

    // 5. The first consumer leaves: the policy and the grants follow.
    migrate::run(&db.config(MIGRATOR), &LAST_CONSUMER)
        .await
        .expect("the run with one consumer left");
    assert_eq!(readers(&mut admin).await, ["obx_second_platform"]);
    let former: bool = sqlx::query_scalar(CAN_READ_EVENTS)
        .bind("obx_consumer_platform")
        .fetch_one(&mut admin)
        .await
        .expect("read the former reader's grant");
    assert!(!former, "a former consumer no longer reads the events");
    let report = migrate::run(&db.config(MIGRATOR), &LAST_CONSUMER)
        .await
        .expect("the repeated run");
    assert!(report.changes.is_empty(), "{:?}", report.changes);
}
