//! Each new tenant gets a ClickHouse account that sees only its own readings
//! (Laysi/trustiot-v2#119, acceptance 1), and that account's queries stop at the limits of its
//! settings profile (acceptance 2).
//!
//! The service runs as a real process on the product's Postgres schema (`testkit::product`) and
//! a throwaway ClickHouse with the product's ClickHouse schema; tenants are created by the
//! identity service's own code, and the service creates their accounts from
//! `identity.tenant_created`. Then, logged in as tenant A's account, the attack types of the
//! 2026-10-09 PoC (decision 14; services/ingest/tests/clickhouse.rs runs the full list against
//! the role itself) try to read tenant B's readings and those of a third tenant ID that only
//! exists in ClickHouse, to change the data, the accounts or the settings, and to bypass the
//! limits. Afterwards: a changed password is repaired on the next query; an account whose query
//! limits an administrator weakened, or which was changed to another tenant, serves no query
//! (503, nothing of the other tenant) and the latter is refused when its event comes again (dead
//! letter); and nothing logs a password.
//!
//! Needs podman or docker (a TLS Postgres and a ClickHouse); skipped without one unless `CI` is
//! set. One test: the binary shares one product database. `--nocapture` prints every attack.

mod support;

use std::time::Duration;

use contract::{ServiceName, TenantId, TenantRole};
use outbox::{Position, Replay};
use sqlx::Connection;
use support::clickhouse::Account;
use support::{
    ClickHouseTarget, EVENT_LIMIT, Readings, Service, Stack, random_password, wait_account_ready,
    wait_for,
};
use testkit::identity::TestIssuer;
use testkit::product::DATABASE;

/// A tenant that exists only in ClickHouse: its readings are written directly, as if it were
/// another customer. Its ID is a constant so the attacks can name it in SQL text.
const VICTIM: &str = "0199a0b0-c0d0-7e0f-8a1b-2c3d4e5f6a7b";
/// What every text reading of the victim, and of tenant B, starts with.
const VICTIM_MARKER: &str = "victim-marker";
const B_MARKER: &str = "tenant-b-marker";
const A_MARKER: &str = "tenant-a-marker";

/// Readings each tenant gets.
const READINGS: u64 = 40;

const SUBSCRIPTION: &str = "telemetry-query.identity-events";
const DEAD_LETTERS: &str = "SELECT count(*) FROM telemetry_query.outbox_dead_letter";
const ACCOUNT_ROWS: &str =
    "SELECT count(*) FROM telemetry_query.tenant_accounts WHERE tenant_id = $1::uuid";

/// What the account is, as the admin sees it.
const ACCOUNT_AS_ADMIN: &str = "SELECT default_roles_list, host_ip, auth_type
FROM system.users WHERE name = {user:String} FORMAT TSV";
const ACCOUNT_ROLES: &str = "SELECT granted_role_name, with_admin_option
FROM system.role_grants WHERE user_name = {user:String} FORMAT TSV";
const ACCOUNT_GRANTS: &str = "SELECT count() FROM system.grants WHERE user_name = {user:String}";

/// What the account sees of itself.
const WHO_AM_I: &str = "SELECT getSetting('SQL_tenant_id'), getSetting('readonly'),
  getSetting('max_execution_time'), getSetting('max_memory_usage'), getSetting('max_rows_to_read'),
  currentRoles()
FORMAT TSV";
/// The account's own rows: all rows it can see, and those of its tenant.
const OWN_ROWS: &str = "SELECT count(), countIf(tenant_id = {me:UUID})
FROM trustiot.tenant_telemetry FORMAT TSV";

/// Admin changes standing for someone who changed the account behind the service's back.
const CHANGE_PASSWORD: &str =
    "ALTER USER {user:Identifier} IDENTIFIED WITH sha256_password BY {password:String}";
const CHANGE_TENANT: &str = "ALTER USER {user:Identifier}
SETTINGS PROFILE 'trustiot_tenant_reader', SQL_tenant_id = '0199a0b0-c0d0-7e0f-8a1b-2c3d4e5f6a7b' CONST";

/// Admin change that weakens one of the account's limits on the account itself, after the
/// profile, and leaves `readonly` as it is: `TENANT` is replaced with the account's own tenant.
const WEAKEN_LIMITS: &str = "ALTER USER {user:Identifier}
SETTINGS PROFILE 'trustiot_tenant_reader', SQL_tenant_id = 'TENANT' CONST, max_execution_time = 3600";
/// Admin change back to the account as the service creates it.
const RESTORE_SETTINGS: &str = "ALTER USER {user:Identifier}
SETTINGS PROFILE 'trustiot_tenant_reader', SQL_tenant_id = 'TENANT' CONST";

/// The queries the hourly interval of the tenant quota has counted for an account, and when that
/// interval ends, as the admin reads them.
const HOURLY_QUERIES: &str = "SELECT toString(end_time), queries FROM system.quotas_usage
WHERE quota_name = 'trustiot_tenant_reader' AND quota_key = {user:String} AND duration = 3600
FORMAT TSV";

/// API calls between two reads of the quota.
const QUOTA_CALLS: u64 = 3;

/// The rows of the victim and of tenant B, as the reference account counts them.
const VICTIM_ROWS: &str = "SELECT count() FROM trustiot.telemetry
WHERE tenant_id IN ({victim:UUID}, {b:UUID}) FORMAT TSV";

/// One attack: a name, the statement, and settings sent in the URL.
struct Attack {
    name: &'static str,
    sql: &'static str,
    url: &'static [(&'static str, &'static str)],
}

const fn attack(name: &'static str, sql: &'static str) -> Attack {
    Attack {
        name,
        sql,
        url: &[],
    }
}

/// The PoC's attack types, as tenant A's account. Each must be refused or answer without a
/// reading of another tenant.
const ATTACKS: &[Attack] = &[
    // Direct reads of the base tables, and filters on the view.
    attack("base table", "SELECT value_string FROM trustiot.telemetry"),
    attack("batch commits", "SELECT * FROM trustiot.batch_commits"),
    attack(
        "view filtered to another tenant",
        "SELECT value_string FROM trustiot.tenant_telemetry WHERE tenant_id = '0199a0b0-c0d0-7e0f-8a1b-2c3d4e5f6a7b'",
    ),
    attack(
        "view with an always-true filter",
        "SELECT value_string FROM trustiot.tenant_telemetry WHERE 1 OR tenant_id != tenant_id",
    ),
    attack(
        "explicit PREWHERE",
        "SELECT value_string FROM trustiot.tenant_telemetry PREWHERE tenant_id = '0199a0b0-c0d0-7e0f-8a1b-2c3d4e5f6a7b'",
    ),
    attack(
        "FINAL",
        "SELECT value_string FROM trustiot.tenant_telemetry FINAL",
    ),
    // Joins, subqueries and set operations that reach the base table.
    attack(
        "join with the base table",
        "SELECT t.value_string FROM trustiot.tenant_telemetry AS v JOIN trustiot.telemetry AS t ON 1",
    ),
    attack(
        "scalar subquery",
        "SELECT (SELECT groupArray(value_string) FROM trustiot.telemetry)",
    ),
    attack(
        "IN subquery",
        "SELECT value_string FROM trustiot.tenant_telemetry WHERE device_id IN (SELECT device_id FROM trustiot.telemetry)",
    ),
    attack(
        "UNION ALL",
        "SELECT value_string FROM trustiot.tenant_telemetry UNION ALL SELECT value_string FROM trustiot.telemetry",
    ),
    attack(
        "common table expression",
        "WITH x AS (SELECT value_string FROM trustiot.telemetry) SELECT * FROM x",
    ),
    // Table functions.
    attack(
        "merge()",
        "SELECT value_string FROM merge('trustiot', '^tele')",
    ),
    attack(
        "view()",
        "SELECT * FROM view(SELECT value_string FROM trustiot.telemetry)",
    ),
    attack(
        "remote() to the same server",
        "SELECT value_string FROM remote('127.0.0.1:9000', trustiot.telemetry)",
    ),
    attack(
        "clusterAllReplicas()",
        "SELECT value_string FROM clusterAllReplicas('default', trustiot.telemetry)",
    ),
    attack(
        "url() back to the HTTP interface",
        "SELECT * FROM url('http://127.0.0.1:8123/?query=SELECT+value_string+FROM+trustiot.telemetry', 'LineAsString')",
    ),
    attack(
        "file()",
        "SELECT * FROM file('/etc/passwd', 'LineAsString')",
    ),
    // System tables.
    attack(
        "system.query_log",
        "SELECT groupArray(query) FROM system.query_log",
    ),
    attack(
        "system.parts",
        "SELECT groupArray(name) FROM system.parts WHERE database = 'trustiot'",
    ),
    attack("system.users", "SELECT groupArray(name) FROM system.users"),
    // Another tenant through the setting, and the limits through settings.
    attack(
        "SETTINGS clause with another tenant",
        "SELECT value_string FROM trustiot.tenant_telemetry SETTINGS SQL_tenant_id = '0199a0b0-c0d0-7e0f-8a1b-2c3d4e5f6a7b'",
    ),
    Attack {
        name: "URL setting with another tenant",
        sql: "SELECT value_string FROM trustiot.tenant_telemetry",
        url: &[("SQL_tenant_id", VICTIM)],
    },
    attack(
        "SET another tenant",
        "SET SQL_tenant_id = '0199a0b0-c0d0-7e0f-8a1b-2c3d4e5f6a7b'",
    ),
    attack(
        "SETTINGS readonly = 0",
        "SELECT value_string FROM trustiot.tenant_telemetry SETTINGS readonly = 0",
    ),
    attack(
        "SETTINGS without the row limit",
        "SELECT count() FROM numbers(2000000000) SETTINGS max_rows_to_read = 0",
    ),
    Attack {
        name: "URL setting without the time limit",
        sql: "SELECT 1",
        url: &[("max_execution_time", "0")],
    },
    // Other identities.
    attack(
        "EXECUTE AS the view's definer",
        "EXECUTE AS trustiot_tenant_view_definer SELECT value_string FROM trustiot.telemetry",
    ),
    Attack {
        name: "URL role of the ingest service",
        sql: "SELECT value_string FROM trustiot.telemetry",
        url: &[("role", "trustiot_ingest")],
    },
    // Writes and access management.
    attack(
        "insert into the base table",
        "INSERT INTO trustiot.telemetry (tenant_id, point, value_type, value_string) VALUES ('0199a0b0-c0d0-7e0f-8a1b-2c3d4e5f6a7b', 'x', 'string', 'written')",
    ),
    attack(
        "delete from the base table",
        "ALTER TABLE trustiot.telemetry DELETE WHERE 1",
    ),
    attack(
        "truncate the base table",
        "TRUNCATE TABLE trustiot.telemetry",
    ),
    attack(
        "create a table",
        "CREATE TABLE default.copy ENGINE = Memory AS SELECT * FROM trustiot.tenant_telemetry",
    ),
    attack(
        "create a tenant account",
        "CREATE USER trustiot_tenant_account_ffffffffffffffffffffffffffffffff",
    ),
    attack(
        "change the service's account",
        "ALTER USER trustiot_telemetry_query IDENTIFIED WITH no_password",
    ),
    attack(
        "grant the base table",
        "GRANT SELECT ON trustiot.telemetry TO trustiot_tenant_reader",
    ),
    attack(
        "drop the row policy",
        "DROP ROW POLICY tenant_isolation ON trustiot.tenant_telemetry",
    ),
];

/// Heavy queries the settings profile stops (0004_access.sql), with the error each must end in.
const HEAVY: &[(&str, &str, &str)] = &[
    (
        "reads more than max_rows_to_read",
        "SELECT count() FROM numbers(2000000000)",
        "TOO_MANY_ROWS",
    ),
    (
        "holds more than max_memory_usage",
        "SELECT length(groupArray(toString(number))) FROM numbers(200000000)",
        "MEMORY_LIMIT_EXCEEDED",
    ),
    (
        "runs longer than max_execution_time",
        "SELECT count() FROM numbers_mt(999000000) WHERE NOT ignore(SHA256(SHA256(SHA256(toString(number)))))",
        "TIMEOUT_EXCEEDED",
    ),
];

#[allow(
    clippy::print_stderr,
    reason = "prints each attack's outcome for the test report"
)]
async fn run_attacks(stack: &Stack, account: &Account) {
    let clickhouse = &stack.clickhouse;
    for attack in ATTACKS {
        let outcome = clickhouse.query_with(account, attack.sql, attack.url).await;
        let shown = match &outcome {
            Ok(body) => {
                for marker in [VICTIM_MARKER, B_MARKER] {
                    assert!(
                        !body.contains(marker),
                        "{}: another tenant's reading leaked: {body}",
                        attack.name
                    );
                }
                "answered without another tenant's readings".to_owned()
            }
            Err(error) => {
                assert!(
                    !error.code().is_empty() && error.status != 0,
                    "{}: no answer: {}",
                    attack.name,
                    error.body
                );
                format!("refused: {}", error.code())
            }
        };
        eprintln!("attack {:>40}: {shown}", attack.name);
    }
}

#[allow(
    clippy::print_stderr,
    reason = "prints each heavy query's outcome for the test report"
)]
async fn run_heavy(stack: &Stack, account: &Account) {
    for (name, sql, expected) in HEAVY {
        let error = stack.clickhouse.query(account, sql).await.expect_err(name);
        assert_eq!(error.code(), *expected, "{name}: {}", error.body);
        eprintln!("heavy query {name:>40}: stopped with {}", error.code());
    }
}

async fn admin_query(stack: &Stack, sql: &str, params: &[(&str, &str)]) -> String {
    stack
        .clickhouse
        .query_with(&stack.clickhouse.admin, sql, params)
        .await
        .unwrap_or_else(|error| panic!("{sql}: {}", error.body))
}

#[tokio::test(flavor = "multi_thread")]
async fn each_tenant_account_sees_its_own_readings_only() {
    let Some(stack) = Stack::start().await else {
        return;
    };
    let issuer = TestIssuer::new();
    let service = Service::start(
        &stack.product,
        &ClickHouseTarget {
            url: &stack.clickhouse.url,
            password: &stack.service_password,
        },
        &issuer.jwks(),
    )
    .await;

    // 1. Two new tenants: the consumer creates both accounts, and only then does the API serve
    //    them.
    let a = stack.create_tenant(&issuer, "Plant A").await;
    let b = stack.create_tenant(&issuer, "Plant B").await;
    let token_a = issuer.tenant_token(ServiceName::TelemetryQuery, a, TenantRole::Viewer);
    let token_b = issuer.tenant_token(ServiceName::TelemetryQuery, b, TenantRole::Viewer);
    wait_account_ready(&service, &token_a, a).await;
    wait_account_ready(&service, &token_b, b).await;
    let account_a = stack.tenant_account(a);
    let account_b = stack.tenant_account(b);
    let user_a = account_a.user.clone();
    assert_eq!(
        admin_query(&stack, ACCOUNT_AS_ADMIN, &[("param_user", &user_a)]).await,
        "['trustiot_tenant_reader']\t['::/0']\t['sha256_password']\n",
        "the account's default role, hosts and password type"
    );
    assert_eq!(
        admin_query(&stack, ACCOUNT_ROLES, &[("param_user", &user_a)]).await,
        "trustiot_tenant_reader\t0\n",
        "the tenant role only, without the right to pass it on"
    );
    assert_eq!(
        admin_query(&stack, ACCOUNT_GRANTS, &[("param_user", &user_a)]).await,
        "0\n",
        "no grant of its own beyond the role"
    );
    let who = stack
        .clickhouse
        .query(&account_a, WHO_AM_I)
        .await
        .unwrap_or_else(|error| panic!("log in as tenant A: {}", error.body));
    assert_eq!(
        who,
        format!("{a}\t1\t30\t536870912\t1000000000\t['trustiot_tenant_reader']\n"),
        "tenant, readonly, limits and role of the account"
    );

    // 2. Readings of A, B and the victim.
    let device = uuid::Uuid::new_v4();
    let start_ms = chrono::Utc::now().timestamp_millis() - 3_600_000;
    for (tenant, marker) in [
        (a, A_MARKER),
        (b, B_MARKER),
        (VICTIM.parse::<TenantId>().expect("an ID"), VICTIM_MARKER),
    ] {
        stack
            .insert(&Readings {
                tenant,
                device,
                point: "status",
                kind: "string",
                start_ms,
                step_ms: 60_000,
                count: READINGS,
                marker,
            })
            .await;
    }
    let (b_text, a_text) = (b.to_string(), a.to_string());
    let others = stack
        .reference(VICTIM_ROWS, &[("victim", VICTIM), ("b", &b_text)])
        .await;
    assert_eq!(others, format!("{}\n", 2 * READINGS));
    for (account, me) in [(&account_a, &a_text), (&account_b, &b_text)] {
        let own = stack
            .clickhouse
            .query_with(account, OWN_ROWS, &[("param_me", me)])
            .await
            .unwrap_or_else(|error| panic!("read own rows: {}", error.body));
        assert_eq!(
            own,
            format!("{READINGS}\t{READINGS}\n"),
            "{me}: its rows only"
        );
    }

    // 3. The attacks, as A's account, then the limits.
    run_attacks(&stack, &account_a).await;
    assert_eq!(
        stack
            .reference(VICTIM_ROWS, &[("victim", VICTIM), ("b", &b_text)])
            .await,
        others,
        "no attack changed the other tenants' readings"
    );
    run_heavy(&stack, &account_a).await;

    // 4. A password changed behind the service's back: the next query sets it again.
    admin_query(
        &stack,
        CHANGE_PASSWORD,
        &[
            ("param_user", &user_a),
            ("param_password", &random_password()),
        ],
    )
    .await;
    let path = format!("/api/telemetry/v1/devices/{device}/latest");
    let latest = service.get_ok(&path, &token_a).await;
    assert_eq!(latest["points"][0]["point"], "status", "{latest}");
    assert!(latest.to_string().contains(A_MARKER), "{latest}");
    assert_eq!(
        service
            .stdout
            .messages("set the tenant account's password again")
            .len(),
        1
    );

    // 4b. The account check before each query reads no table, so the quota counts only the
    //     query itself: each API call is one query of the account's quota. Measured within one
    //     hourly interval; a run that crosses its end measures again.
    let mut measured = None;
    for _ in 0..3 {
        let before = admin_query(&stack, HOURLY_QUERIES, &[("param_user", &user_a)]).await;
        for _ in 0..QUOTA_CALLS {
            service.get_ok(&path, &token_a).await;
        }
        let after = admin_query(&stack, HOURLY_QUERIES, &[("param_user", &user_a)]).await;
        let read = |text: &str| -> (String, u64) {
            let (end, queries) = text
                .trim_end()
                .split_once('\t')
                .unwrap_or_else(|| panic!("the hourly quota usage: {text:?}"));
            (end.to_owned(), queries.parse().expect("a count"))
        };
        let ((end_before, before), (end_after, after)) = (read(&before), read(&after));
        if end_before == end_after {
            measured = after.checked_sub(before);
            break;
        }
    }
    assert_eq!(
        measured,
        Some(QUOTA_CALLS),
        "each API call counts once against the tenant quota"
    );

    // 5. A limit weakened on the account itself, with readonly unchanged: the account would run
    //    longer queries than its profile allows, so the API runs no query as it until the
    //    account is as the service creates it again.
    let refused = "the tenant's ClickHouse account is not as the service creates it";
    admin_query(
        &stack,
        &WEAKEN_LIMITS.replace("TENANT", &a.to_string()),
        &[("param_user", &user_a)],
    )
    .await;
    let who = stack
        .clickhouse
        .query(&account_a, WHO_AM_I)
        .await
        .unwrap_or_else(|error| panic!("log in as tenant A: {}", error.body));
    assert_eq!(
        who,
        format!("{a}\t1\t3600\t536870912\t1000000000\t['trustiot_tenant_reader']\n"),
        "the weakened limit is in force for the account, readonly is not changed"
    );
    let (status, body) = service.get(&path, Some(&token_a)).await;
    assert_eq!(status, 503, "{body}");
    assert!(!body.contains(A_MARKER), "{body}");
    wait_for("the refusal in the log", EVENT_LIMIT, || async {
        let found =
            service.stdout.text().contains(refused) || service.stderr.text().contains(refused);
        found.then_some(())
    })
    .await;
    admin_query(
        &stack,
        &RESTORE_SETTINGS.replace("TENANT", &a.to_string()),
        &[("param_user", &user_a)],
    )
    .await;
    let latest = service.get_ok(&path, &token_a).await;
    assert!(
        latest.to_string().contains(A_MARKER),
        "served again: {latest}"
    );

    // 6. An account changed to another tenant: the API refuses it and shows nothing of the other
    //    tenant (every statement also filters on the caller's tenant, a second layer), and the
    //    consumer refuses the account when the event comes again.
    admin_query(&stack, CHANGE_TENANT, &[("param_user", &user_a)]).await;
    let (status, body) = service.get(&path, Some(&token_a)).await;
    assert_eq!(status, 503, "{body}");
    for marker in [VICTIM_MARKER, B_MARKER, A_MARKER] {
        assert!(!body.contains(marker), "{body}");
    }
    let mut app = stack
        .product
        .postgres
        .connect("telemetry_query_app", DATABASE)
        .await;
    let mut tx = app.begin().await.expect("begin");
    outbox::replay_from(&mut tx, SUBSCRIPTION, Position::START, Replay::Reprocess)
        .await
        .expect("replay the subscription");
    tx.commit().await.expect("commit the replay");
    wait_for(
        "the consumer setting the changed account's event aside",
        EVENT_LIMIT,
        || async {
            let mut admin = stack.product.superuser().await;
            let dead: i64 = sqlx::query_scalar(DEAD_LETTERS)
                .fetch_one(&mut admin)
                .await
                .expect("count dead letters");
            admin.close().await.expect("close");
            (dead == 1).then_some(())
        },
    )
    .await;
    let mut admin = stack.product.superuser().await;
    let rows: i64 = sqlx::query_scalar(ACCOUNT_ROWS)
        .bind(&b_text)
        .fetch_one(&mut admin)
        .await
        .expect("count B's row");
    assert_eq!(rows, 1, "B's account is still recorded once");

    // 7. No password in the log.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let log = format!("{}\n{}", service.stdout.text(), service.stderr.text());
    for secret in [
        stack.service_password.as_str(),
        account_a.password.as_str(),
        account_b.password.as_str(),
    ] {
        assert!(!log.contains(secret), "a password is in the log");
    }
    admin.close().await.expect("close");
    app.close().await.expect("close");
}
