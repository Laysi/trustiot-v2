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
//! letter); an account without its queryable period reads nothing, and the next query sets the
//! period again, as it does for one with another period (Laysi/trustiot-v2#143); an account
//! whose default role was taken away serves no query, is logged on each call and is not
//! repaired (Laysi/trustiot-v2#143); and nothing logs a password.
//!
//! Needs podman or docker (a TLS Postgres and a ClickHouse); skipped without one unless `CI` is
//! set. One test: the binary shares one product database. `--nocapture` prints every attack.

mod support;

use std::time::Duration;

use contract::{ServiceName, TenantId, TenantRole};
use outbox::{Position, Replay};
use sqlx::Connection;
use support::clickhouse::Account;
use support::poc::{A_MARKER, ATTACKS, B_MARKER, HEAVY, SYSTEM_TABLES, VICTIM, VICTIM_MARKER};
use support::{
    ClickHouseTarget, EVENT_LIMIT, Readings, Service, Stack, random_password, wait_account_ready,
    wait_for,
};
use testkit::identity::TestIssuer;
use testkit::product::DATABASE;

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
/// The account's queryable period: raw days, one-minute months, one-hour months.
const WINDOWS: &str = "SELECT getSetting('SQL_queryable_raw_days'),
  getSetting('SQL_queryable_1m_months'), getSetting('SQL_queryable_1h_months')
FORMAT TSV";
/// The period every account gets until per-plan periods (Laysi/trustiot-v2#453).
const EXPECTED_WINDOWS: &str = "7\t3\t3\n";
/// The account's own rows: all rows it can see, and those of its tenant.
const OWN_ROWS: &str = "SELECT count(), countIf(tenant_id = {me:UUID})
FROM trustiot.tenant_telemetry FORMAT TSV";

/// Admin changes standing for someone who changed the account behind the service's back.
const CHANGE_PASSWORD: &str =
    "ALTER USER {user:Identifier} IDENTIFIED WITH sha256_password BY {password:String}";
const CHANGE_TENANT: &str = "ALTER USER {user:Identifier}
SETTINGS PROFILE 'trustiot_tenant_reader', SQL_tenant_id = '0199a0b0-c0d0-7e0f-8a1b-2c3d4e5f6a7b' CONST";

/// Admin change of the raw window alone, as an older plan or a change by hand would leave it.
const CHANGE_WINDOW: &str =
    "ALTER USER {user:Identifier} MODIFY SETTINGS SQL_queryable_raw_days = 30 CONST";

/// Admin change that leaves the tenant role granted but no longer the account's default role,
/// so a login has no active role; and the change back.
const DROP_DEFAULT_ROLE: &str = "ALTER USER {user:Identifier} DEFAULT ROLE NONE";
const RESTORE_DEFAULT_ROLE: &str =
    "ALTER USER {user:Identifier} DEFAULT ROLE trustiot_tenant_reader";

/// The service's error log when it refuses an account.
const REFUSED_LOG: &str = "the tenant's ClickHouse account is not as the service creates it, \
or refused its password and could not be repaired; no query runs as it";

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
    reason = "prints each system table's outcome for the test report"
)]
async fn run_system_tables(stack: &Stack, account: &Account) {
    admin_query(stack, "SYSTEM FLUSH LOGS", &[]).await;
    for sql in SYSTEM_TABLES {
        let error = stack.clickhouse.query(account, sql).await.expect_err(sql);
        assert_eq!(error.code(), "ACCESS_DENIED", "{sql}: {}", error.body);
        let table = sql.trim_start_matches("SELECT count() FROM ");
        eprintln!("attack {table:>40}: refused: {}", error.code());
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
    assert_eq!(
        stack
            .clickhouse
            .query(&account_a, WINDOWS)
            .await
            .unwrap_or_else(|error| panic!("read the windows: {}", error.body)),
        EXPECTED_WINDOWS,
        "the queryable period, set when the account was created"
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
    run_system_tables(&stack, &account_a).await;
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
    // ALTER USER ... SETTINGS replaces all of the account's settings, so the windows are gone
    // too: the account reads nothing (the row policies need them) until they are set again.
    for sql in [WINDOWS, OWN_ROWS] {
        let error = stack
            .clickhouse
            .query_with(&account_a, sql, &[("param_me", &a_text)])
            .await
            .expect_err("an account without its queryable period reads nothing");
        assert_eq!(error.code(), "UNKNOWN_SETTING", "{sql}: {}", error.body);
    }
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
    let period_set = "set the tenant account's queryable period";
    assert_eq!(service.stdout.messages(period_set).len(), 1);
    assert_eq!(
        stack
            .clickhouse
            .query(&account_a, WINDOWS)
            .await
            .unwrap_or_else(|error| panic!("read the windows: {}", error.body)),
        EXPECTED_WINDOWS,
        "the API set the queryable period again before it served the account"
    );

    // 5b. Another period on the account (a plan change, or a change by hand): the next query
    //     sets the service's period again before it runs.
    admin_query(&stack, CHANGE_WINDOW, &[("param_user", &user_a)]).await;
    assert_eq!(
        stack
            .clickhouse
            .query(&account_a, WINDOWS)
            .await
            .unwrap_or_else(|error| panic!("read the windows: {}", error.body)),
        "30\t3\t3\n"
    );
    service.get_ok(&path, &token_a).await;
    assert_eq!(service.stdout.messages(period_set).len(), 2);
    assert_eq!(
        stack
            .clickhouse
            .query(&account_a, WINDOWS)
            .await
            .unwrap_or_else(|error| panic!("read the windows: {}", error.body)),
        EXPECTED_WINDOWS,
        "the service's period, from the query after the change"
    );

    // 5c. The tenant role is no longer the account's default role (the owner's decision of
    //     2026-10-26 on Laysi/trustiot-v2#143): every call answers 503 with nothing of the
    //     tenant and logs why, and the service does not set the role again; an administrator
    //     does, and the next call is served.
    admin_query(&stack, DROP_DEFAULT_ROLE, &[("param_user", &user_a)]).await;
    let other_roles = || {
        service
            .stdout
            .messages(REFUSED_LOG)
            .into_iter()
            .filter(|line| {
                line["fields"]["error"]
                    .as_str()
                    .is_some_and(|error| error.contains("(other roles)"))
            })
            .count()
    };
    for call in 1..=2 {
        let (status, body) = service.get(&path, Some(&token_a)).await;
        assert_eq!(status, 503, "call {call}: {body}");
        assert!(!body.contains(A_MARKER), "call {call}: {body}");
        wait_for(
            &format!("call {call}'s refusal of the account without its role in the log"),
            EVENT_LIMIT,
            || async { (other_roles() == call).then_some(()) },
        )
        .await;
        assert_eq!(
            admin_query(&stack, ACCOUNT_AS_ADMIN, &[("param_user", &user_a)]).await,
            "[]\t['::/0']\t['sha256_password']\n",
            "call {call}: the service did not set the default role again"
        );
    }
    admin_query(&stack, RESTORE_DEFAULT_ROLE, &[("param_user", &user_a)]).await;
    let latest = service.get_ok(&path, &token_a).await;
    assert!(
        latest.to_string().contains(A_MARKER),
        "served again once an administrator set the role: {latest}"
    );
    assert_eq!(
        service.stdout.messages(period_set).len(),
        2,
        "the role change left the queryable period as it was"
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
