//! The outbox wiring (crates/outbox, crates/service-kit/sql/outbox-event.sql and
//! outbox-aggregate.sql) and the per-tenant role hook, with a test registry: a producer, its
//! consumers, and a hook over the producer's tenant table. It also upgrades a producer schema
//! from before outbox-aggregate.sql (Laysi/trustiot-v2#413).
//!
//! Needs podman or docker; skipped without one unless `CI` is set.

mod support;

use migrate::registry::{Migration, Outbox, Registry, Service, TenantRoleHook};
use service_kit::postgres::{SessionRole, check_session};
use sqlx::{Connection, PgConnection};
use support::postgres::tls_postgres;
use support::{Database, MIGRATOR, MIGRATOR_INIT, snapshot};

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
const AGGREGATE_POLICIES: &str = "SELECT polname::text FROM pg_policy
WHERE polrelid = 'obx_producer.outbox_aggregate'::regclass ORDER BY 1";
const AGGREGATE_RLS: &str =
    "SELECT relrowsecurity FROM pg_class WHERE oid = 'obx_producer.outbox_aggregate'::regclass";
/// Every aggregate with its tenant, as the superuser sees them.
const AGGREGATE_TENANTS: &str = "SELECT aggregate_type || '/' || aggregate_id, tenant_id::text
FROM obx_producer.outbox_aggregate ORDER BY 1";
/// What the producer's service role sees outside a tenant transaction.
const VISIBLE_AGGREGATES: &str = "SELECT count(*) FROM obx_producer.outbox_aggregate";
/// Turns the producer schema back into what the Job left before outbox-aggregate.sql existed
/// (Laysi/trustiot-v2#413): `outbox_aggregate` without the tenant column, its policies, or
/// row-level security, and the file not recorded. The grants stay, as the Job's
/// `PRODUCER_GRANTS` gave them then.
const BEFORE_THE_AGGREGATE_TEMPLATE: &str = "
DROP POLICY tenant_isolation ON obx_producer.outbox_aggregate;
DROP POLICY platform_aggregates ON obx_producer.outbox_aggregate;
DROP POLICY platform_aggregates_only ON obx_producer.outbox_aggregate;
ALTER TABLE obx_producer.outbox_aggregate DISABLE ROW LEVEL SECURITY;
ALTER TABLE obx_producer.outbox_aggregate DROP COLUMN tenant_id;
DELETE FROM trustiot_migrate.applied
WHERE component = 'obx-producer.outbox' AND name = 'outbox-aggregate.sql';
";
/// What a producer on that version wrote: events of tenant A's and tenant B's aggregates and a
/// platform-wide one, each aggregate's sequence without a tenant.
const OLD_EVENTS: &str = "
INSERT INTO obx_producer.outbox_event
    (tx_order, event_id, event_type, tenant_id, aggregate_type, aggregate_id, aggregate_seq, envelope)
VALUES
    (1, gen_random_uuid(), 'test.event', '00000000-0000-0000-0000-00000000000a', 'widget', 'a', 1, '{}'),
    (2, gen_random_uuid(), 'test.event', '00000000-0000-0000-0000-00000000000a', 'widget', 'a', 2, '{}'),
    (3, gen_random_uuid(), 'test.event', '00000000-0000-0000-0000-00000000000b', 'widget', 'b', 1, '{}'),
    (4, gen_random_uuid(), 'test.event', NULL, 'catalog', 'c', 1, '{}');
INSERT INTO obx_producer.outbox_aggregate (aggregate_type, aggregate_id, last_seq, last_tx) VALUES
    ('widget', 'a', 2, 2), ('widget', 'b', 1, 3), ('catalog', 'c', 1, 4);
";
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
const CAN_LOG_IN: &str = "SELECT rolcanlogin FROM pg_roles WHERE rolname = $1";
/// Login roles the Job does not manage: one with the Job's marker that the migration role does
/// not administer (made by the superuser, as a customer's account or CloudNativePG's would be),
/// and one the migration role made by hand without the marker.
const FOREIGN_LOGIN: &str = "CREATE ROLE obx_foreign_login LOGIN;
COMMENT ON ROLE obx_foreign_login IS 'trustiot-migrate:login-role'";
const UNMARKED_LOGIN: &str = "CREATE ROLE obx_unmarked_login LOGIN";

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

async fn aggregate_policies(admin: &mut PgConnection) -> Vec<String> {
    sqlx::query_scalar(AGGREGATE_POLICIES)
        .fetch_all(admin)
        .await
        .expect("read the aggregate policies")
}

#[tokio::test(flavor = "multi_thread")]
async fn outbox_wiring_and_tenant_roles() {
    let Some(postgres) = tls_postgres(MIGRATOR_INIT).await else {
        return;
    };
    let db = Database::create(postgres, CREATE, "migrate_outbox").await;
    // The login roles of the services in the registry, as the chart binds the enabled ones.
    for role in &LOGIN_ROLES[..4] {
        db.bind(role);
    }
    let passwords = |roles: &[&'static str]| -> Vec<(&'static str, String)> {
        roles
            .iter()
            .map(|role| (*role, db.password(role)))
            .collect()
    };

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
    assert_eq!(
        aggregate_policies(&mut admin).await,
        [
            "platform_aggregates",
            "platform_aggregates_only",
            "tenant_isolation"
        ],
        "the outbox-aggregate.sql template"
    );
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
    check_sessions(&db, &passwords(&LOGIN_ROLES[..4])).await;

    // 1b. A database that ran the Job before outbox-aggregate.sql existed, with aggregates in
    //     it (Laysi/trustiot-v2#413): the next run applies the new file and only that, gives
    //     every aggregate the tenant of its last event, and the run after it changes nothing.
    //     outbox-event.sql, applied before, is not applied again.
    sqlx::raw_sql(BEFORE_THE_AGGREGATE_TEMPLATE)
        .execute(&mut admin)
        .await
        .expect("the producer schema before outbox-aggregate.sql");
    sqlx::raw_sql(OLD_EVENTS)
        .execute(&mut admin)
        .await
        .expect("events of the version before outbox-aggregate.sql");
    assert!(aggregate_policies(&mut admin).await.is_empty());
    let report = migrate::run(&db.config(MIGRATOR), &ONE_CONSUMER)
        .await
        .expect("the run on the database from before outbox-aggregate.sql");
    assert_eq!(
        report.changes,
        ["applied obx-producer.outbox/outbox-aggregate.sql"]
    );
    let tenants: Vec<(String, Option<String>)> = sqlx::query_as(AGGREGATE_TENANTS)
        .fetch_all(&mut admin)
        .await
        .expect("read the aggregates");
    assert_eq!(
        tenants,
        [
            ("catalog/c".to_owned(), None),
            (
                "widget/a".to_owned(),
                Some("00000000-0000-0000-0000-00000000000a".to_owned())
            ),
            (
                "widget/b".to_owned(),
                Some("00000000-0000-0000-0000-00000000000b".to_owned())
            ),
        ],
        "each aggregate has the tenant of its last event"
    );
    let rls: bool = sqlx::query_scalar(AGGREGATE_RLS)
        .fetch_one(&mut admin)
        .await
        .expect("read the table");
    assert!(rls, "row-level security on outbox_aggregate");
    assert_eq!(
        aggregate_policies(&mut admin).await,
        [
            "platform_aggregates",
            "platform_aggregates_only",
            "tenant_isolation"
        ]
    );
    check_sessions(&db, &passwords(&LOGIN_ROLES[..4])).await;
    let mut app = db
        .connect_with_password("obx_producer_app", &db.password("obx_producer_app"))
        .await
        .expect("obx_producer_app logs in");
    let visible: i64 = sqlx::query_scalar(VISIBLE_AGGREGATES)
        .fetch_one(&mut app)
        .await
        .expect("read as the service role");
    assert_eq!(visible, 0, "no aggregate without a tenant");
    app.close().await.expect("close");
    let before = snapshot(&db).await;
    let report = migrate::run(&db.config(MIGRATOR), &ONE_CONSUMER)
        .await
        .expect("the repeated run");
    assert!(report.changes.is_empty(), "{:?}", report.changes);
    assert_eq!(snapshot(&db).await, before);

    // 2. A second consumer joins: the policy names both, and both read the events.
    for role in &LOGIN_ROLES[4..] {
        db.bind(role);
    }
    migrate::run(&db.config(MIGRATOR), &TWO_CONSUMERS)
        .await
        .expect("the run with two consumers");
    assert_eq!(
        readers(&mut admin).await,
        ["obx_consumer_platform", "obx_second_platform"]
    );
    check_sessions(&db, &passwords(&LOGIN_ROLES)).await;

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

    // 5. The first consumer leaves: the policy and the grants follow, and its login roles,
    //    which no service of the registry has any more, can no longer log in (they stay, NOLOGIN).
    let former_passwords = passwords(&LOGIN_ROLES[2..4]);
    for role in &LOGIN_ROLES[2..4] {
        db.unbind(role);
    }
    let report = migrate::run(&db.config(MIGRATOR), &LAST_CONSUMER)
        .await
        .expect("the run with one consumer left");
    for role in &LOGIN_ROLES[2..4] {
        let expected = format!(
            "revoked the login of role {role}: no service in tools/migrate's registry has it"
        );
        assert!(
            report.changes.contains(&expected),
            "missing {expected:?} in {:?}",
            report.changes
        );
    }
    for (role, password) in &former_passwords {
        assert!(
            db.connect_with_password(role, password).await.is_err(),
            "{role} no longer logs in"
        );
    }
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

    // 6. A service turned off in the deployment (still in the registry, no Secret bound): its
    //    roles lose LOGIN. Turned on again, each gets its Secret's password, then LOGIN: the
    //    service role through the Secret Helm kept and adopts again (its password, nothing
    //    generated), the platform role through a Secret declared anew (empty, so the Job
    //    generates its password first).
    for role in &LOGIN_ROLES[4..] {
        db.unbind(role);
    }
    let report = migrate::run(&db.config(MIGRATOR), &LAST_CONSUMER)
        .await
        .expect("the run with the second consumer off");
    assert_eq!(
        report.changes,
        LOGIN_ROLES[4..]
            .iter()
            .map(|role| format!("revoked the login of role {role}: no Secret is bound to it"))
            .collect::<Vec<_>>()
    );
    assert_eq!(report.roles_without_password, LOGIN_ROLES[4..]);
    for role in &LOGIN_ROLES[4..] {
        let login: bool = sqlx::query_scalar(CAN_LOG_IN)
            .bind(role)
            .fetch_one(&mut admin)
            .await
            .expect("read the role");
        assert!(!login, "{role} is NOLOGIN");
    }
    let kept = db.password("obx_second_app");
    db.rebind("obx_second_app");
    db.bind("obx_second_platform");
    let report = migrate::run(&db.config(MIGRATOR), &LAST_CONSUMER)
        .await
        .expect("the run with the second consumer on again");
    assert_eq!(
        report.changes,
        [
            "generated the password of obx_second_platform in Secret obx-second-platform-postgres (it had none)",
            "set the password of obx_second_app",
            "let role obx_second_app log in",
            "set the password of obx_second_platform",
            "let role obx_second_platform log in",
        ]
    );
    assert_eq!(
        db.password("obx_second_app"),
        kept,
        "the kept Secret's password is the one the role gets back"
    );
    check_sessions(&db, &passwords(&LOGIN_ROLES[4..])).await;

    // 7. Login roles the Job does not manage keep their login, whatever their name or comment.
    sqlx::raw_sql(FOREIGN_LOGIN)
        .execute(&mut admin)
        .await
        .expect("create a role the migration role does not administer");
    let mut as_migrator = db.connect(MIGRATOR).await;
    sqlx::raw_sql(UNMARKED_LOGIN)
        .execute(&mut as_migrator)
        .await
        .expect("create an unmarked role as the migration role");
    let report = migrate::run(&db.config(MIGRATOR), &LAST_CONSUMER)
        .await
        .expect("the run beside roles it does not manage");
    assert!(report.changes.is_empty(), "{:?}", report.changes);
    for role in ["obx_foreign_login", "obx_unmarked_login"] {
        let login: bool = sqlx::query_scalar(CAN_LOG_IN)
            .bind(role)
            .fetch_one(&mut admin)
            .await
            .expect("read the role");
        assert!(login, "{role} keeps its login");
    }
}
