//! The ClickHouse schema against a real server: the migrations apply (twice), the tables have
//! the settings decision 8 and 14 need, and tenant isolation holds against the attack types of
//! the 2026-10-09 feasibility test (docs/feasibility/2026-10-09-cloud-poc.md), the independent
//! verification of Laysi/trustiot-v2#59, and the table functions and system tables that reach a
//! table without applying row policies (Laysi/trustiot-v2#260).
//!
//! Uses `TEST_CLICKHOUSE_URL` (with `TEST_CLICKHOUSE_USER` and `TEST_CLICKHOUSE_PASSWORD`) when
//! set; otherwise starts a throwaway ClickHouse container with podman or docker; skipped
//! without either unless `CI` is set. Every account and tenant is unique to the run, so a
//! shared server works too.
//!
//! Run with `--nocapture` to see each attack and its outcome.

mod common;

use std::cell::Cell;
use std::collections::BTreeSet;

use common::{Account, Answer, ClickHouse, QueryError};
use uuid::Uuid;

/// The only relation tenant accounts may read: a DEFINER view over the telemetry table
/// (0005_tenant_view.sql).
const TENANT_VIEW: &str = "trustiot.tenant_telemetry";
/// The telemetry table behind the view; tenant accounts must not reach it at all.
const TELEMETRY: &str = "trustiot.telemetry";
/// The tables of the trustiot database, none of which tenant accounts may reach.
const TABLES: [&str; 2] = [TELEMETRY, "trustiot.batch_commits"];
/// The user the tenant view runs as.
const VIEW_DEFINER: &str = "trustiot_tenant_view_definer";

/// Accounts of tenant A that the long checks rotate through: the quota allows 60 errors and 120
/// queries per account and minute, and an exhausted quota would hide an attack's outcome.
const POOL_ACCOUNTS: usize = 16;
const QUERIES_PER_POOL_ACCOUNT: usize = 40;

/// The accounts and data of one run.
struct World {
    ch: ClickHouse,
    suffix: String,
    tenant_a: Uuid,
    tenant_b: Uuid,
    /// Text only tenant B's rows contain.
    b_secret: String,
    ingest: Account,
    a: Account,
    /// A second account of tenant A, so the error quota of `a` cannot hide an attack's outcome.
    a_extra: Account,
    b: Account,
    /// Has the tenant role but no SQL_tenant_id setting.
    no_tenant: Account,
    /// Can SELECT the table through a role that no row policy names.
    unscoped: Account,
    quota_1: Account,
    quota_2: Account,
    /// Tenant A accounts with stricter per-query limits than the profile.
    rows_limited: Account,
    memory_limited: Account,
    /// More tenant A accounts, see `pool_account`.
    pool: Vec<Account>,
    pool_queries: Cell<usize>,
}

const A_ROWS: usize = 3;
const B_ROWS: usize = 4;

fn password() -> String {
    Uuid::new_v4().simple().to_string()
}

/// Creates a tenant account with the two statements documented in 0004_access.sql.
async fn create_tenant_account(ch: &ClickHouse, user: &str, tenant: Option<Uuid>) -> Account {
    create_tenant_account_with(ch, user, tenant, "").await
}

/// The same two statements with `extra` user-level settings after the tenant setting.
async fn create_tenant_account_with(
    ch: &ClickHouse,
    user: &str,
    tenant: Option<Uuid>,
    extra: &str,
) -> Account {
    let account = Account {
        user: user.to_owned(),
        password: password(),
    };
    let tenant_setting = tenant
        .map(|tenant| format!(", SQL_tenant_id = '{tenant}' CONST"))
        .unwrap_or_default();
    ch.admin_ok(&format!(
        "CREATE USER {user} IDENTIFIED WITH sha256_password BY '{}' \
         SETTINGS PROFILE 'trustiot_tenant_reader'{tenant_setting}{extra}",
        account.password
    ))
    .await;
    ch.admin_ok(&format!("GRANT trustiot_tenant_reader TO {user}"))
        .await;
    account
}

async fn create_account_with_role(ch: &ClickHouse, user: &str, role: &str) -> Account {
    let account = Account {
        user: user.to_owned(),
        password: password(),
    };
    ch.admin_ok(&format!(
        "CREATE USER {user} IDENTIFIED WITH sha256_password BY '{}'",
        account.password
    ))
    .await;
    ch.admin_ok(&format!("GRANT {role} TO {user}")).await;
    account
}

impl World {
    async fn new(ch: ClickHouse) -> Self {
        let suffix: String = Uuid::new_v4()
            .simple()
            .to_string()
            .chars()
            .take(12)
            .collect();
        let tenant_a = Uuid::new_v4();
        let tenant_b = Uuid::new_v4();
        let ingest =
            create_account_with_role(&ch, &format!("ingest_{suffix}"), "trustiot_ingest").await;
        let a = create_tenant_account(&ch, &format!("tenant_a_{suffix}"), Some(tenant_a)).await;
        let a_extra =
            create_tenant_account(&ch, &format!("tenant_a_extra_{suffix}"), Some(tenant_a)).await;
        let b = create_tenant_account(&ch, &format!("tenant_b_{suffix}"), Some(tenant_b)).await;
        let no_tenant = create_tenant_account(&ch, &format!("no_tenant_{suffix}"), None).await;
        let unscoped_role = format!("unscoped_reader_{suffix}");
        ch.admin_ok(&format!("CREATE ROLE {unscoped_role}")).await;
        ch.admin_ok(&format!(
            "GRANT SELECT ON trustiot.telemetry TO {unscoped_role}"
        ))
        .await;
        let unscoped =
            create_account_with_role(&ch, &format!("unscoped_{suffix}"), &unscoped_role).await;
        let quota_1 =
            create_tenant_account(&ch, &format!("quota_1_{suffix}"), Some(Uuid::new_v4())).await;
        let quota_2 =
            create_tenant_account(&ch, &format!("quota_2_{suffix}"), Some(Uuid::new_v4())).await;
        // Stricter user-level values than the profile's, to see that the calling account's
        // limits cover the rows read inside the view.
        let rows_limited = create_tenant_account_with(
            &ch,
            &format!("rows_limited_{suffix}"),
            Some(tenant_a),
            ", max_rows_to_read = 1 CONST",
        )
        .await;
        let memory_limited = create_tenant_account_with(
            &ch,
            &format!("memory_limited_{suffix}"),
            Some(tenant_a),
            ", max_memory_usage = 50000000 CONST",
        )
        .await;
        let mut pool = Vec::new();
        for n in 0..POOL_ACCOUNTS {
            pool.push(
                create_tenant_account(&ch, &format!("tenant_a_pool{n}_{suffix}"), Some(tenant_a))
                    .await,
            );
        }
        let b_secret = format!("B-SECRET-{suffix}");
        Self {
            ch,
            suffix,
            tenant_a,
            tenant_b,
            b_secret,
            ingest,
            a,
            a_extra,
            b,
            no_tenant,
            unscoped,
            quota_1,
            quota_2,
            rows_limited,
            memory_limited,
            pool,
            pool_queries: Cell::new(0),
        }
    }

    /// The next tenant A account of the pool: each serves `QUERIES_PER_POOL_ACCOUNT` queries, so
    /// no account reaches its per-minute quota during a run.
    fn pool_account(&self) -> &Account {
        let used = self.pool_queries.get();
        self.pool_queries.set(used + 1);
        self.pool
            .get(used / QUERIES_PER_POOL_ACCOUNT)
            .expect("the pool of tenant A accounts is used up; raise POOL_ACCOUNTS")
    }

    /// Writes tenant A's and tenant B's rows as the ingest service. Both tenants share the device
    /// and point, so a broken policy would let a JOIN on them reach B's rows.
    async fn insert_rows(&self) {
        let device = Uuid::new_v4();
        let mut values = Vec::new();
        for n in 0..A_ROWS {
            values.push(self.row(self.tenant_a, device, n, &format!("a-row-{n}")));
        }
        for n in 0..B_ROWS {
            values.push(self.row(self.tenant_b, device, n, &self.b_secret));
        }
        self.ch
            .query(
                &self.ingest,
                &format!(
                    "INSERT INTO trustiot.telemetry (tenant_id, gateway_id, stream_id, batch_seq, \
                     device_id, point, ts, received_at, value_type, value_string) VALUES {}",
                    values.join(", ")
                ),
            )
            .await
            .unwrap_or_else(|error| panic!("ingest insert failed: {}", error.body));
    }

    fn row(&self, tenant: Uuid, device: Uuid, n: usize, value: &str) -> String {
        format!(
            "('{tenant}', '{}', '{}', 1, '{device}', 'shared-point', \
             toDateTime64('2026-10-10 00:00:00', 3, 'UTC') + {n}, now64(3, 'UTC'), 'string', '{value}')",
            Uuid::new_v4(),
            Uuid::new_v4()
        )
    }

    fn leaks_b(&self, text: &str) -> bool {
        text.contains(&self.tenant_b.to_string()) || text.contains(&self.b_secret)
    }

    /// Removes this run's rows and accounts, so runs against a shared server leave nothing
    /// behind. Every row the test writes belongs to tenant A or B.
    async fn cleanup(&self) {
        let (a, b) = (self.tenant_a, self.tenant_b);
        for table in ["telemetry", "batch_commits"] {
            // A mutation as the admin: row policies hide these rows from the admin's SELECT and
            // lightweight DELETE, but not from ALTER TABLE ... DELETE.
            self.ch
                .admin_ok(&format!(
                    "ALTER TABLE trustiot.{table} DELETE WHERE tenant_id IN ('{a}', '{b}') \
                     SETTINGS mutations_sync = 2"
                ))
                .await;
            let left = self
                .ch
                .query(
                    &self.ingest,
                    &format!(
                        "SELECT count() FROM trustiot.{table} WHERE tenant_id IN ('{a}', '{b}')"
                    ),
                )
                .await
                .unwrap_or_else(failed);
            assert_eq!(left.trim(), "0", "test rows left in trustiot.{table}");
        }
        for user in [
            &self.ingest,
            &self.a,
            &self.a_extra,
            &self.b,
            &self.no_tenant,
            &self.unscoped,
            &self.quota_1,
            &self.quota_2,
            &self.rows_limited,
            &self.memory_limited,
        ]
        .into_iter()
        .chain(&self.pool)
        {
            let _ = self
                .ch
                .admin(&format!("DROP USER IF EXISTS {}", user.user))
                .await;
        }
        let _ = self
            .ch
            .admin(&format!(
                "DROP ROLE IF EXISTS unscoped_reader_{}",
                self.suffix
            ))
            .await;
    }
}

/// For `unwrap_or_else` on a query that must succeed.
fn failed(error: QueryError) -> String {
    panic!("query failed ({}): {}", error.status, error.body)
}

fn first_line(body: &str) -> String {
    body.lines().next().unwrap_or_default().trim().to_owned()
}

enum Expect {
    /// The exact answer (TSV, trimmed).
    Answer(String),
    /// Any error.
    Denied,
}

struct Attack {
    name: &'static str,
    sql: String,
    params: Vec<(&'static str, String)>,
    expect: Expect,
}

fn attack(name: &'static str, sql: impl Into<String>, expect: Expect) -> Attack {
    Attack {
        name,
        sql: sql.into(),
        params: Vec::new(),
        expect,
    }
}

fn answer(text: impl Into<String>) -> Expect {
    Expect::Answer(text.into())
}

/// The 2026-10-09 attack types as tenant A against tenant B: direct queries, JOIN, subqueries,
/// UNION, FINAL, merge(), view(), system tables, plus the ways around them the feasibility test
/// tried (setting overrides, table functions, impersonation, writes). Reads go to the tenant
/// view, the only relation a tenant account may read; `on_telemetry_table` repeats them on the
/// table behind it.
fn attacks(world: &World) -> Vec<Attack> {
    let b = world.tenant_b;
    let a = world.tenant_a;
    let v = TENANT_VIEW;
    let b_secret = &world.b_secret;
    let a_count = A_ROWS.to_string();
    let a_values = (0..A_ROWS)
        .map(|n| format!("a-row-{n}"))
        .collect::<Vec<_>>()
        .join("\n");
    vec![
        // Direct queries.
        attack(
            "direct: count every row",
            format!("SELECT count() FROM {v}"),
            answer(&a_count),
        ),
        attack(
            "direct: read every value",
            format!("SELECT value_string FROM {v} ORDER BY value_string"),
            answer(&a_values),
        ),
        attack(
            "direct: filter on tenant B",
            format!("SELECT count() FROM {v} WHERE tenant_id = '{b}'"),
            answer("0"),
        ),
        attack(
            "direct: tenant B OR true",
            format!("SELECT count() FROM {v} WHERE tenant_id = '{b}' OR 1 = 1"),
            answer(&a_count),
        ),
        // A view takes no PREWHERE clause; ClickHouse still moves WHERE conditions into
        // PREWHERE on the table behind it.
        attack(
            "direct: PREWHERE tenant B",
            format!("SELECT count() FROM {v} PREWHERE tenant_id = '{b}'"),
            Expect::Denied,
        ),
        attack(
            "direct: group by tenant",
            format!("SELECT tenant_id, count() FROM {v} GROUP BY tenant_id"),
            answer(format!("{a}\t{a_count}")),
        ),
        // A view has no virtual columns such as _part.
        attack(
            "direct: virtual columns",
            format!("SELECT count() FROM {v} WHERE _part != '' AND _partition_id != ''"),
            Expect::Denied,
        ),
        attack(
            "direct: throwIf error oracle",
            format!("SELECT count() FROM {v} WHERE throwIf(tenant_id = '{b}', 'oracle') = 0"),
            answer(&a_count),
        ),
        // The caller's conditions must never run on another tenant's rows, not even on rows in
        // the same granule. A tenant filter that sat only on the view would allow that
        // (0005_tenant_view.sql); the test data puts both tenants in one granule.
        attack(
            "direct: throwIf on another tenant's value",
            format!(
                "SELECT count() FROM {v} WHERE throwIf(value_string = '{b_secret}', 'oracle') = 0"
            ),
            answer(&a_count),
        ),
        attack(
            "direct: throwIf on any row of another tenant",
            format!("SELECT count() FROM {v} WHERE throwIf(tenant_id != '{a}', 'oracle') = 0"),
            answer(&a_count),
        ),
        // The conversion error quotes the first value it fails on: always one of tenant A's.
        attack(
            "direct: conversion error quotes a value",
            format!("SELECT count() FROM {v} WHERE toUInt64(value_string) = 1"),
            Expect::Denied,
        ),
        // JOIN.
        attack(
            "JOIN: self join on the shared device",
            format!(
                "SELECT count() FROM {v} AS x INNER JOIN {v} AS y ON x.device_id = y.device_id WHERE y.tenant_id = '{b}'"
            ),
            answer("0"),
        ),
        attack(
            "JOIN: cross join",
            format!("SELECT count() FROM {v} AS x CROSS JOIN {v} AS y"),
            answer((A_ROWS * A_ROWS).to_string()),
        ),
        attack(
            "JOIN: LEFT join to tenant B",
            format!(
                "SELECT countIf(y.tenant_id = '{b}') FROM {v} AS x LEFT JOIN {v} AS y ON x.point = y.point"
            ),
            answer("0"),
        ),
        // Subqueries.
        attack(
            "subquery: IN",
            format!(
                "SELECT count() FROM {v} WHERE tenant_id IN (SELECT tenant_id FROM {v} WHERE tenant_id = '{b}')"
            ),
            answer("0"),
        ),
        attack(
            "subquery: scalar",
            format!("SELECT (SELECT count() FROM {v})"),
            answer(&a_count),
        ),
        attack(
            "subquery: FROM",
            format!("SELECT count() FROM (SELECT * FROM {v})"),
            answer(&a_count),
        ),
        attack(
            "subquery: CTE",
            format!("WITH t AS (SELECT * FROM {v}) SELECT count() FROM t"),
            answer(&a_count),
        ),
        // UNION.
        attack(
            "UNION ALL with tenant B filter",
            format!(
                "SELECT count() FROM (SELECT tenant_id FROM {v} UNION ALL SELECT tenant_id FROM {v} WHERE tenant_id = '{b}')"
            ),
            answer(&a_count),
        ),
        attack(
            "UNION DISTINCT of tenants",
            format!("SELECT tenant_id FROM {v} UNION DISTINCT SELECT tenant_id FROM {v}"),
            answer(a.to_string()),
        ),
        // FINAL: the view hands it to the table, whose rows stay filtered.
        attack(
            "FINAL",
            format!("SELECT count() FROM {v} FINAL"),
            answer(&a_count),
        ),
        // Table functions.
        attack(
            "merge(): the tenant view",
            "SELECT count() FROM merge('trustiot', '^tenant_telemetry$')",
            answer(&a_count),
        ),
        attack(
            "merge(): every table in the database",
            "SELECT count() FROM merge('trustiot', '.*')",
            answer(&a_count),
        ),
        attack(
            "merge(): the telemetry table",
            "SELECT count() FROM merge('trustiot', '^telemetry$')",
            Expect::Denied,
        ),
        attack(
            "view()",
            format!("SELECT count() FROM view(SELECT * FROM {v})"),
            answer(&a_count),
        ),
        attack(
            "view() filtered on tenant B",
            format!("SELECT count() FROM view(SELECT * FROM {v} WHERE tenant_id = '{b}')"),
            answer("0"),
        ),
        attack(
            "mergeTreeIndex()",
            "SELECT * FROM mergeTreeIndex('trustiot', 'telemetry', with_marks = true)",
            Expect::Denied,
        ),
        attack(
            "mergeTreeAnalyzeIndexes() with tenant B's key",
            format!(
                "SELECT * FROM mergeTreeAnalyzeIndexes('trustiot', 'telemetry', tenant_id = '{b}')"
            ),
            Expect::Denied,
        ),
        attack(
            "remote() to this server",
            format!("SELECT count() FROM remote('127.0.0.1', {v})"),
            Expect::Denied,
        ),
        attack(
            "cluster()",
            format!("SELECT count() FROM cluster('default', {v})"),
            Expect::Denied,
        ),
        attack(
            "loop()",
            format!("SELECT count() FROM loop({v}) LIMIT 1"),
            Expect::Denied,
        ),
        attack(
            "file()",
            "SELECT count() FROM file('/etc/passwd', 'LineAsString')",
            Expect::Denied,
        ),
        attack(
            "url() (SSRF)",
            "SELECT count() FROM url('http://127.0.0.1:8123/?query=SELECT%201', 'LineAsString')",
            Expect::Denied,
        ),
        // System tables.
        attack(
            "system.query_log",
            "SELECT count() FROM system.query_log",
            Expect::Denied,
        ),
        attack(
            "system.processes",
            "SELECT count() FROM system.processes",
            Expect::Denied,
        ),
        attack(
            "system.parts",
            "SELECT count() FROM system.parts",
            Expect::Denied,
        ),
        attack(
            "system.part_log",
            "SELECT count() FROM system.part_log",
            Expect::Denied,
        ),
        attack(
            "system.text_log",
            "SELECT count() FROM system.text_log",
            Expect::Denied,
        ),
        attack(
            "system.users",
            "SELECT count() FROM system.users",
            Expect::Denied,
        ),
        attack(
            "system.row_policies",
            "SELECT count() FROM system.row_policies",
            Expect::Denied,
        ),
        attack(
            "system.settings_profile_elements",
            "SELECT count() FROM system.settings_profile_elements",
            Expect::Denied,
        ),
        attack(
            "system.mutations",
            "SELECT count() FROM system.mutations",
            Expect::Denied,
        ),
        // Changing the tenant setting or the filters.
        attack(
            "query SETTINGS SQL_tenant_id = B",
            format!("SELECT count() FROM {v} SETTINGS SQL_tenant_id = '{b}'"),
            Expect::Denied,
        ),
        Attack {
            name: "URL parameter SQL_tenant_id = B",
            sql: format!("SELECT count() FROM {v}"),
            params: vec![("SQL_tenant_id", b.to_string())],
            expect: Expect::Denied,
        },
        attack(
            "query SETTINGS additional_table_filters",
            format!(
                "SELECT count() FROM {v} SETTINGS additional_table_filters = {{'trustiot.telemetry': '1'}}"
            ),
            Expect::Denied,
        ),
        attack(
            "raise max_threads",
            "SELECT 1 SETTINGS max_threads = 64",
            Expect::Denied,
        ),
        attack(
            "EXECUTE AS the ingest account",
            format!(
                "EXECUTE AS {} SELECT count() FROM {TELEMETRY}",
                world.ingest.user
            ),
            Expect::Denied,
        ),
        attack(
            "EXECUTE AS the view definer",
            format!("EXECUTE AS {VIEW_DEFINER} SELECT count() FROM {TELEMETRY}"),
            Expect::Denied,
        ),
        // Tables and writes the tenant has no grant for.
        attack(
            "read batch_commits",
            "SELECT count() FROM trustiot.batch_commits",
            Expect::Denied,
        ),
        attack(
            "tenant INSERT of a tenant B row",
            format!(
                "INSERT INTO {TELEMETRY} (tenant_id, point, value_type) VALUES ('{b}', 'x', 'float')"
            ),
            Expect::Denied,
        ),
        attack(
            "tenant INSERT into the view",
            format!("INSERT INTO {v} (tenant_id, point, value_type) VALUES ('{b}', 'x', 'float')"),
            Expect::Denied,
        ),
        attack(
            "tenant lightweight DELETE",
            format!("DELETE FROM {TELEMETRY} WHERE tenant_id = '{b}'"),
            Expect::Denied,
        ),
        attack(
            "tenant ALTER TABLE DELETE",
            format!("ALTER TABLE {TELEMETRY} DELETE WHERE tenant_id = '{b}'"),
            Expect::Denied,
        ),
        attack(
            "CREATE VIEW",
            format!("CREATE VIEW trustiot.leak AS SELECT * FROM {TELEMETRY}"),
            Expect::Denied,
        ),
        attack(
            "CREATE ROW POLICY",
            format!("CREATE ROW POLICY leak ON {v} USING 1 TO CURRENT_USER"),
            Expect::Denied,
        ),
        attack(
            "GRANT to self",
            "GRANT SELECT ON *.* TO CURRENT_USER",
            Expect::Denied,
        ),
    ]
}

/// Attacks added by the independent verification of Laysi/trustiot-v2#59 (see
/// docs/test-reports/2026-10-11-59-verify.md): settings in nested queries, URL parameters that
/// select a role, table functions and introspection that a tenant account reaches, and
/// statements that must stay denied to a read-only account. Run as the second account of
/// tenant A.
fn extra_attacks(world: &World) -> Vec<Attack> {
    let a = world.tenant_a;
    let b = world.tenant_b;
    let v = TENANT_VIEW;
    let a_count = A_ROWS.to_string();
    let a_self_join = (A_ROWS * A_ROWS).to_string();
    let mut attacks = vec![
        // ClickHouse accepts a SETTINGS clause on a nested query but must not apply it to the
        // row policy: the read-only profile only checks the outermost one.
        attack(
            "nested SETTINGS: SQL_tenant_id = B in a subquery",
            format!("SELECT count() FROM (SELECT * FROM {v} SETTINGS SQL_tenant_id = '{b}')"),
            answer(&a_count),
        ),
        attack(
            "nested SETTINGS: readonly = 0 and SQL_tenant_id = B in a subquery",
            format!(
                "SELECT count() FROM (SELECT * FROM {v} SETTINGS readonly = 0, SQL_tenant_id = '{b}')"
            ),
            answer(&a_count),
        ),
        attack(
            "nested SETTINGS: CTE",
            format!(
                "WITH t AS (SELECT * FROM {v} SETTINGS SQL_tenant_id = '{b}') SELECT count() FROM t"
            ),
            answer(&a_count),
        ),
        attack(
            "nested SETTINGS: JOIN subquery",
            format!(
                "SELECT count() FROM {v} AS x JOIN (SELECT * FROM {v} SETTINGS SQL_tenant_id = '{b}') AS y ON x.point = y.point"
            ),
            answer(&a_self_join),
        ),
        attack(
            "nested SETTINGS: scalar subquery",
            format!("SELECT (SELECT count() FROM {v} SETTINGS SQL_tenant_id = '{b}')"),
            answer(&a_count),
        ),
        attack(
            "nested SETTINGS: view()",
            format!("SELECT count() FROM view(SELECT * FROM {v} SETTINGS SQL_tenant_id = '{b}')"),
            answer(&a_count),
        ),
        attack(
            "nested SETTINGS: getSetting still returns the account's tenant",
            format!(
                "SELECT * FROM (SELECT getSetting('SQL_tenant_id') SETTINGS SQL_tenant_id = '{b}')"
            ),
            answer(a.to_string()),
        ),
        attack(
            "nested SETTINGS: readonly stays 1",
            "SELECT * FROM (SELECT getSetting('readonly') SETTINGS readonly = 0)",
            answer("1"),
        ),
        attack(
            "nested SETTINGS: max_rows_to_read = 0 does not lift the profile limit",
            "SELECT count() FROM (SELECT number FROM numbers(2000000000) SETTINGS max_rows_to_read = 0)",
            Expect::Denied,
        ),
        // Selecting another role or settings profile through the HTTP interface.
        Attack {
            name: "URL parameter role = trustiot_ingest",
            sql: format!("SELECT count() FROM {v}"),
            params: vec![("role", "trustiot_ingest".to_owned())],
            expect: Expect::Denied,
        },
        Attack {
            name: "URL parameters readonly = 0 and SQL_tenant_id = B",
            sql: format!("SELECT count() FROM {v}"),
            params: vec![
                ("readonly", "0".to_owned()),
                ("SQL_tenant_id", b.to_string()),
            ],
            expect: Expect::Denied,
        },
        attack(
            "SETTINGS profile plus max_threads",
            "SELECT 1 SETTINGS profile = 'trustiot_tenant_reader', max_threads = 64",
            Expect::Denied,
        ),
        // Table functions and virtual tables that read table metadata or other servers.
        attack(
            "mergeTreeCodecBlockCounts()",
            "SELECT * FROM mergeTreeCodecBlockCounts('trustiot', 'telemetry')",
            Expect::Denied,
        ),
        attack(
            "mergeTreeIndex() on batch_commits",
            "SELECT * FROM mergeTreeIndex('trustiot', 'batch_commits')",
            Expect::Denied,
        ),
        attack(
            "remote('localhost')",
            format!("SELECT count() FROM remote('localhost', {v})"),
            Expect::Denied,
        ),
        attack(
            "remote() with the default account",
            "SELECT count() FROM remote('127.0.0.1:9000', system.one, 'default', '')",
            Expect::Denied,
        ),
        attack(
            "clusterAllReplicas()",
            format!("SELECT count() FROM clusterAllReplicas('default', {v})"),
            Expect::Denied,
        ),
        attack(
            "s3()",
            "SELECT count() FROM s3('http://127.0.0.1:9000/bucket/key.csv', 'CSV', 'a UInt8')",
            Expect::Denied,
        ),
        attack(
            "urlCluster()",
            "SELECT count() FROM urlCluster('default', 'http://127.0.0.1:8123/?query=SELECT%201', 'LineAsString')",
            Expect::Denied,
        ),
        attack(
            "executable()",
            "SELECT * FROM executable('cat', 'LineAsString', 'line String')",
            Expect::Denied,
        ),
    ];
    // Introspection and administration statements a read-only account must not run.
    attacks.push(attack(
        "SHOW CREATE USER of tenant B",
        format!("SHOW CREATE USER {}", world.b.user),
        Expect::Denied,
    ));
    for (name, sql) in [
        (
            "SHOW GRANTS FOR the ingest role",
            "SHOW GRANTS FOR trustiot_ingest",
        ),
        ("SHOW ROW POLICIES", "SHOW ROW POLICIES"),
        (
            "SHOW CREATE ROW POLICY",
            "SHOW CREATE ROW POLICY tenant_isolation ON trustiot.telemetry",
        ),
        ("SHOW QUOTAS", "SHOW QUOTAS"),
        ("SHOW ACCESS", "SHOW ACCESS"),
        (
            "KILL QUERY of other accounts",
            "KILL QUERY WHERE user != currentUser() SYNC",
        ),
        ("SYSTEM FLUSH LOGS", "SYSTEM FLUSH LOGS"),
        ("SYSTEM RELOAD CONFIG", "SYSTEM RELOAD CONFIG"),
        ("OPTIMIZE TABLE", "OPTIMIZE TABLE trustiot.telemetry FINAL"),
        (
            "ALTER TABLE UPDATE",
            "ALTER TABLE trustiot.telemetry UPDATE value_string = 'x' WHERE 1",
        ),
        (
            "ALTER TABLE DROP PARTITION",
            "ALTER TABLE trustiot.telemetry DROP PARTITION ID 'all'",
        ),
        (
            "CREATE TEMPORARY TABLE",
            "CREATE TEMPORARY TABLE scratch (x UInt8) ENGINE = Memory",
        ),
        (
            "INSERT SELECT into telemetry",
            "INSERT INTO trustiot.telemetry SELECT * FROM trustiot.telemetry",
        ),
        (
            "SHOW CREATE TABLE batch_commits",
            "SHOW CREATE TABLE trustiot.batch_commits",
        ),
        (
            "SHOW CREATE TABLE telemetry",
            "SHOW CREATE TABLE trustiot.telemetry",
        ),
        ("DESCRIBE telemetry", "DESCRIBE TABLE trustiot.telemetry"),
        ("ALTER USER itself", "ALTER USER CURRENT_USER SETTINGS NONE"),
        (
            "SET DEFAULT ROLE trustiot_ingest",
            "SET DEFAULT ROLE trustiot_ingest TO CURRENT_USER",
        ),
        (
            "ALTER the view's definer",
            "ALTER TABLE trustiot.tenant_telemetry MODIFY SQL SECURITY INVOKER",
        ),
    ] {
        attacks.push(attack(name, sql, Expect::Denied));
    }
    attacks
}

/// The attacks that read the tenant view, run on the telemetry table instead: a tenant account
/// has no grant on the table, so each one must be refused.
fn on_telemetry_table(attacks: Vec<Attack>) -> Vec<Attack> {
    attacks
        .into_iter()
        .filter(|attack| attack.sql.contains(TENANT_VIEW))
        .map(|attack| Attack {
            sql: attack.sql.replace(TENANT_VIEW, TELEMETRY),
            expect: Expect::Denied,
            ..attack
        })
        .collect()
}

/// Whether `result` holds no rows: refused (for any reason but an exhausted quota), or an
/// empty answer or a count of 0.
fn reads_no_rows(result: &Result<String, QueryError>) -> bool {
    match result {
        Ok(body) => matches!(body.trim(), "" | "0"),
        Err(error) => error.code() != "QUOTA_EXCEEDED",
    }
}

/// A brand-new account with no role reads nothing, and an account that is granted `SELECT`
/// directly (not through a role that a policy names) reads no rows from the tables or the view:
/// the `deny_by_default` policies apply to every account, including ones created later.
async fn deny_by_default_covers_new_accounts(world: &World) {
    let ch = &world.ch;
    let tenant = world.tenant_a;
    // One completion record, so a count of 0 below does not come from an empty table.
    ch.query(
        &world.ingest,
        &format!(
            "INSERT INTO trustiot.batch_commits (tenant_id, gateway_id, stream_id, seq, row_count, \
             committed_at) VALUES ('{tenant}', generateUUIDv4(), generateUUIDv4(), 1, 1, now64(3, 'UTC'))"
        ),
    )
    .await
    .unwrap_or_else(failed);
    let bare = Account {
        user: format!("bare_{}", world.suffix),
        password: password(),
    };
    let direct = Account {
        user: format!("direct_{}", world.suffix),
        password: password(),
    };
    // Has tenant A's setting and SELECT on the view, but not the tenant role.
    let direct_tenant = Account {
        user: format!("direct_tenant_{}", world.suffix),
        password: password(),
    };
    for account in [&bare, &direct] {
        ch.admin_ok(&format!(
            "CREATE USER {} IDENTIFIED WITH sha256_password BY '{}'",
            account.user, account.password
        ))
        .await;
    }
    ch.admin_ok(&format!(
        "CREATE USER {} IDENTIFIED WITH sha256_password BY '{}' \
         SETTINGS readonly = 1 CONST, SQL_tenant_id = '{tenant}' CONST",
        direct_tenant.user, direct_tenant.password
    ))
    .await;
    ch.admin_ok(&format!("GRANT SELECT ON trustiot.* TO {}", direct.user))
        .await;
    ch.admin_ok(&format!(
        "GRANT SELECT ON {TENANT_VIEW} TO {}",
        direct_tenant.user
    ))
    .await;
    for relation in TABLES.into_iter().chain([TENANT_VIEW]) {
        let sql = format!("SELECT count() FROM {relation}");
        let error = ch
            .query(&bare, &sql)
            .await
            .expect_err("an account without a role must not read the table");
        assert_eq!(error.code(), "ACCESS_DENIED", "{relation}: {}", error.body);
        // On the tables: 0 rows. On the view: 0 rows, or the error of the definer's policy,
        // which needs the SQL_tenant_id setting these accounts do not have.
        let result = ch.query(&direct, &sql).await;
        assert!(
            reads_no_rows(&result),
            "an account granted SELECT directly must read no rows of {relation}: {result:?}"
        );
        // The bootstrap admin is an account too: the policies hide the rows from it as well.
        let result = ch.admin(&sql).await;
        assert!(
            reads_no_rows(&result),
            "the admin reads no rows of {relation}: {result:?}"
        );
    }
    assert_eq!(
        ch.query(
            &direct_tenant,
            &format!("SELECT count() FROM {TENANT_VIEW}")
        )
        .await
        .unwrap_or_else(failed)
        .trim(),
        "0",
        "the view's deny_by_default policy must hide every row from an account outside the tenant role"
    );
    // Tenant accounts have no grant on either table.
    for account in [&world.a, &world.b] {
        for table in TABLES {
            let error = ch
                .query(account, &format!("SELECT count() FROM {table}"))
                .await
                .expect_err("a tenant account must not read the tables");
            assert_eq!(error.code(), "ACCESS_DENIED", "{table}: {}", error.body);
        }
    }
    for account in [&bare, &direct, &direct_tenant] {
        ch.admin_ok(&format!("DROP USER IF EXISTS {}", account.user))
            .await;
    }
}

async fn tenant_attacks_read_nothing_of_another_tenant(world: &World) {
    run_attacks(world, Some(&world.a), attacks(world)).await;
    run_attacks(world, Some(&world.a_extra), extra_attacks(world)).await;
    run_attacks(world, None, on_telemetry_table(attacks(world))).await;
    run_attacks(world, None, on_telemetry_table(extra_attacks(world))).await;
}

/// Runs every attack as `account`, or as accounts of the pool when `None`, and fails when one
/// leaks tenant B or does not end as expected.
async fn run_attacks(world: &World, account: Option<&Account>, attacks: Vec<Attack>) {
    let mut failures = Vec::new();
    #[allow(clippy::print_stdout, reason = "the attack list is the test evidence")]
    {
        println!("attack | outcome");
        for attack in attacks {
            let params: Vec<(&str, &str)> = attack
                .params
                .iter()
                .map(|(name, value)| (*name, value.as_str()))
                .collect();
            let account = account.unwrap_or_else(|| world.pool_account());
            let result = world.ch.query_with(account, &attack.sql, &params).await;
            let outcome = match &result {
                Ok(body) => format!("answer {:?}", body.trim()),
                Err(error) => format!("refused {}", error.code()),
            };
            println!("{} [{}] | {outcome}", attack.name, attack.sql);
            let text = match &result {
                Ok(body) => body.as_str(),
                Err(error) => error.body.as_str(),
            };
            let as_expected = match (&attack.expect, &result) {
                (Expect::Answer(expected), Ok(body)) => body.trim() == expected,
                // An exhausted quota would hide whether the statement itself is refused.
                (Expect::Denied, Err(error)) => error.code() != "QUOTA_EXCEEDED",
                _ => false,
            };
            if world.leaks_b(text) || !as_expected {
                failures.push(format!("{}: {outcome}\n{text}", attack.name));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "attacks that misbehaved:\n{}",
        failures.join("\n")
    );
}

/// How the Laysi/trustiot-v2#260 inventory treats each table function of ClickHouse 26.8.
#[derive(Clone, Copy)]
enum TableFunction {
    /// Names a table in its arguments. The probe runs on each table of the trustiot database and
    /// on the tenant view; `{db}`, `{table}` and `{uuid}` name the relation and `{key}` is a
    /// tenant ID.
    ReadsTable(&'static str),
    /// Takes no table: generators, literal data, query-text fuzzing, INSERT input.
    NoTable,
    /// Reads only an external source (object storage, files, URLs, other database servers,
    /// programs). ClickHouse does not allow these for read-only accounts, and tenant accounts
    /// hold neither CREATE TEMPORARY TABLE nor any source grant.
    External,
}

use TableFunction::{External, NoTable, ReadsTable};

/// Every table function of ClickHouse 26.8.22, as `system.table_functions` lists them. The test
/// fails when the server has one that is not here, so an upgrade cannot add an unreviewed way to
/// reach a table.
const TABLE_FUNCTIONS: &[(&str, TableFunction)] = &[
    ("SQLStandardValues", NoTable),
    ("arrowFlight", External),
    ("arrowflight", External),
    ("azureBlobStorage", External),
    ("azureBlobStorageCluster", External),
    ("bigquery", External),
    (
        "cluster",
        ReadsTable(
            "SELECT count() FROM cluster('default', {db}.{table}) WHERE tenant_id = '{key}'",
        ),
    ),
    (
        "clusterAllReplicas",
        ReadsTable(
            "SELECT count() FROM clusterAllReplicas('default', {db}.{table}) WHERE tenant_id = '{key}'",
        ),
    ),
    ("cosn", External),
    ("deltaLake", External),
    ("deltaLakeAzure", External),
    ("deltaLakeAzureCluster", External),
    ("deltaLakeCluster", External),
    ("deltaLakeLocal", External),
    ("deltaLakeS3", External),
    ("deltaLakeS3Cluster", External),
    (
        "dictionary",
        ReadsTable("SELECT count() FROM dictionary('{db}.{table}')"),
    ),
    (
        "eval",
        ReadsTable(
            "SELECT * FROM eval('SELECT count() FROM {db}.{table} WHERE tenant_id = ''{key}''')",
        ),
    ),
    ("executable", External),
    ("file", External),
    ("fileCluster", External),
    ("filesystem", External),
    ("format", NoTable),
    ("fuzzJSON", NoTable),
    ("fuzzQuery", NoTable),
    ("gcs", External),
    ("generateRandom", NoTable),
    ("generateSeries", NoTable),
    ("generate_series", NoTable),
    ("hdfs", External),
    ("hdfsCluster", External),
    ("hive", External),
    ("hudi", External),
    ("hudiCluster", External),
    ("iceberg", External),
    ("icebergAzure", External),
    ("icebergAzureCluster", External),
    ("icebergCluster", External),
    ("icebergHDFS", External),
    ("icebergHDFSCluster", External),
    ("icebergLocal", External),
    ("icebergLocalCluster", External),
    ("icebergS3", External),
    ("icebergS3Cluster", External),
    ("input", NoTable),
    ("jdbc", External),
    (
        "loop",
        ReadsTable(
            "SELECT count() FROM (SELECT tenant_id FROM loop('{db}', '{table}') LIMIT 10) WHERE tenant_id = '{key}'",
        ),
    ),
    (
        "merge",
        ReadsTable("SELECT count() FROM merge('{db}', '^{table}$') WHERE tenant_id = '{key}'"),
    ),
    (
        "mergeTreeAnalyzeIndexes",
        ReadsTable("SELECT * FROM mergeTreeAnalyzeIndexes('{db}', '{table}', tenant_id = '{key}')"),
    ),
    (
        "mergeTreeAnalyzeIndexesUUID",
        ReadsTable("SELECT * FROM mergeTreeAnalyzeIndexesUUID('{uuid}', tenant_id = '{key}')"),
    ),
    (
        "mergeTreeCodecBlockCounts",
        ReadsTable("SELECT * FROM mergeTreeCodecBlockCounts('{db}', '{table}')"),
    ),
    (
        "mergeTreeIndex",
        ReadsTable("SELECT * FROM mergeTreeIndex('{db}', '{table}', with_marks = true)"),
    ),
    (
        "mergeTreeProjection",
        ReadsTable("SELECT * FROM mergeTreeProjection('{db}', '{table}', 'any_projection')"),
    ),
    (
        "mergeTreeTextIndex",
        ReadsTable("SELECT * FROM mergeTreeTextIndex('{db}', '{table}', 'any_index')"),
    ),
    ("mongodb", External),
    ("mysql", External),
    ("null", NoTable),
    ("numbers", NoTable),
    ("numbers_mt", NoTable),
    ("odbc", External),
    ("oss", External),
    ("paimon", External),
    ("paimonAzure", External),
    ("paimonAzureCluster", External),
    ("paimonCluster", External),
    ("paimonHDFS", External),
    ("paimonHDFSCluster", External),
    ("paimonLocal", External),
    ("paimonS3", External),
    ("paimonS3Cluster", External),
    ("postgresql", External),
    ("primes", NoTable),
    (
        "prometheusQuery",
        ReadsTable("SELECT * FROM prometheusQuery({db}.{table}, 'up', now())"),
    ),
    (
        "prometheusQueryRange",
        ReadsTable("SELECT * FROM prometheusQueryRange({db}.{table}, 'up', now() - 60, now(), 10)"),
    ),
    ("redis", External),
    (
        "remote",
        ReadsTable(
            "SELECT count() FROM remote('127.0.0.1', {db}.{table}) WHERE tenant_id = '{key}'",
        ),
    ),
    (
        "remoteSecure",
        ReadsTable(
            "SELECT count() FROM remoteSecure('127.0.0.1', {db}.{table}) WHERE tenant_id = '{key}'",
        ),
    ),
    ("s3", External),
    ("s3Cluster", External),
    ("sqlite", External),
    (
        "timeSeriesData",
        ReadsTable("SELECT * FROM timeSeriesData({db}.{table})"),
    ),
    (
        "timeSeriesMetrics",
        ReadsTable("SELECT * FROM timeSeriesMetrics({db}.{table})"),
    ),
    (
        "timeSeriesSamples",
        ReadsTable("SELECT * FROM timeSeriesSamples({db}.{table})"),
    ),
    (
        "timeSeriesSelector",
        ReadsTable("SELECT * FROM timeSeriesSelector({db}.{table}, 'up', now() - 60, now())"),
    ),
    (
        "timeSeriesTags",
        ReadsTable("SELECT * FROM timeSeriesTags({db}.{table})"),
    ),
    ("url", External),
    ("urlCluster", External),
    ("values", NoTable),
    (
        "view",
        ReadsTable(
            "SELECT count() FROM view(SELECT * FROM {db}.{table} WHERE tenant_id = '{key}')",
        ),
    ),
    (
        "viewExplain",
        ReadsTable(
            "SELECT * FROM (EXPLAIN indexes = 1 SELECT * FROM {db}.{table} WHERE tenant_id = '{key}')",
        ),
    ),
    (
        "viewIfPermitted",
        ReadsTable(
            "SELECT count() FROM viewIfPermitted(SELECT tenant_id FROM {db}.{table} WHERE tenant_id = '{key}' ELSE null('tenant_id UUID'))",
        ),
    ),
    ("ytsaurus", External),
    ("zeros", NoTable),
    ("zeros_mt", NoTable),
];

/// Statements and system tables that describe a table's data without being table functions.
const OTHER_PROBES: &[(&str, &str)] = &[
    (
        "EXPLAIN indexes",
        "EXPLAIN indexes = 1 SELECT * FROM {db}.{table} WHERE tenant_id = '{key}'",
    ),
    (
        "EXPLAIN ESTIMATE",
        "EXPLAIN ESTIMATE SELECT * FROM {db}.{table} WHERE tenant_id = '{key}'",
    ),
    (
        "rows read (X-ClickHouse-Summary)",
        "SELECT count() FROM {db}.{table} WHERE tenant_id = '{key}'",
    ),
    (
        "hasColumnInTable()",
        "SELECT hasColumnInTable('{db}', '{table}', 'tenant_id')",
    ),
    (
        "system.tables statistics",
        "SELECT total_rows, total_bytes FROM system.tables WHERE database = '{db}' AND name = '{table}'",
    ),
    (
        "system.columns statistics",
        "SELECT sum(data_compressed_bytes) FROM system.columns WHERE database = '{db}' AND table = '{table}'",
    ),
];

/// An answer with the probed tenant ID replaced, so the answers for two IDs can be compared.
fn normalized(answer: &Answer, key: Uuid) -> String {
    let key = key.to_string();
    let outcome = match &answer.result {
        Ok(body) => format!("answer {}", body.trim().replace(&key, "<key>")),
        Err(error) => format!("refused {}", error.code()),
    };
    format!("{outcome} (read_rows {:?})", answer.read_rows)
}

/// Laysi/trustiot-v2#260: no table function, statement or system table that names a table gives
/// a tenant account a signal about another tenant's rows. Every probe runs twice, with tenant B's
/// ID and with an ID no tenant has; the answers (and the rows read) must be the same. On the
/// tables a tenant account must get nothing at all, since it has no grant on them.
async fn introspection_tells_nothing_of_other_tenants(world: &World) {
    let ch = &world.ch;
    let mut failures = Vec::new();

    // The list above is complete for this server, and the external functions stay closed to
    // read-only accounts.
    let server = ch
        .admin_ok(
            "SELECT name, allow_readonly FROM system.table_functions ORDER BY name FORMAT TSV",
        )
        .await;
    let mut on_server = BTreeSet::new();
    for line in server.lines() {
        let (name, allow_readonly) = line.split_once('\t').unwrap_or((line, ""));
        on_server.insert(name.to_owned());
        match TABLE_FUNCTIONS.iter().find(|(known, _)| *known == name) {
            None => failures.push(format!(
                "table function {name} is not classified in TABLE_FUNCTIONS: decide whether it \
                 reaches a table and add it"
            )),
            Some((_, External)) if allow_readonly != "0" => failures.push(format!(
                "external table function {name} is now allowed for read-only accounts"
            )),
            Some(_) => {}
        }
    }
    for (name, _) in TABLE_FUNCTIONS {
        if !on_server.contains(*name) {
            failures.push(format!(
                "table function {name} no longer exists: remove it from TABLE_FUNCTIONS"
            ));
        }
    }

    // An ID no tenant has that sorts before every random (version 4) UUID, so it falls in no
    // index range of the test data. A random ID can fall inside the last granule's open-ended
    // range and look like an existing key.
    let other = Uuid::from_u128(1);
    let mut relations = Vec::new();
    for relation in TABLES.into_iter().chain([TENANT_VIEW]) {
        let (db, table) = relation.split_once('.').expect("db.table");
        let uuid = ch
            .admin_ok(&format!(
                "SELECT uuid FROM system.tables WHERE database = '{db}' AND name = '{table}'"
            ))
            .await;
        relations.push((relation, db, table, uuid.trim().to_owned()));
    }
    let probes = TABLE_FUNCTIONS
        .iter()
        .filter_map(|(name, kind)| match kind {
            ReadsTable(template) => Some((*name, *template)),
            NoTable | External => None,
        })
        .chain(OTHER_PROBES.iter().copied());
    #[allow(clippy::print_stdout, reason = "the probe list is the test evidence")]
    {
        println!("probe | relation | with tenant B's ID | with an unused ID");
        for (name, template) in probes {
            for (relation, db, table, uuid) in &relations {
                let relation = *relation;
                let sql = template
                    .replace("{db}", db)
                    .replace("{table}", table)
                    .replace("{uuid}", uuid);
                let with_b = ch
                    .query_with_stats(
                        world.pool_account(),
                        &sql.replace("{key}", &world.tenant_b.to_string()),
                    )
                    .await;
                let with_other = ch
                    .query_with_stats(
                        world.pool_account(),
                        &sql.replace("{key}", &other.to_string()),
                    )
                    .await;
                let (b_outcome, other_outcome) = (
                    normalized(&with_b, world.tenant_b),
                    normalized(&with_other, other),
                );
                println!("{name} | {relation} | {b_outcome} | {other_outcome}");
                for answer in [&with_b, &with_other] {
                    if let Err(error) = &answer.result
                        && error.code() == "QUOTA_EXCEEDED"
                    {
                        failures.push(format!("{name} on {relation}: quota exceeded"));
                    }
                }
                if b_outcome != other_outcome {
                    failures.push(format!(
                        "{name} on {relation} depends on tenant B's rows: {b_outcome} / {other_outcome}"
                    ));
                }
                let b_text = match &with_b.result {
                    Ok(body) => body,
                    Err(error) => &error.body,
                };
                if b_text.contains(&world.b_secret) {
                    failures.push(format!("{name} on {relation} shows tenant B's data"));
                }
                if relation != TENANT_VIEW && !reads_no_rows(&with_b.result) {
                    failures.push(format!(
                        "{name} on {relation} answers a tenant account: {b_outcome}"
                    ));
                }
            }
        }
    }
    assert!(
        failures.is_empty(),
        "introspection reaches other tenants:\n{}",
        failures.join("\n")
    );
}

/// System tables a tenant account can read on ClickHouse 26.8: reference data compiled into the
/// server, or data about the account itself and the objects it may read. The test fails when a
/// tenant account can read any other system or information_schema table.
const READABLE_SYSTEM_TABLES: &[&str] = &[
    "system.aggregate_function_combinators",
    "system.collations",
    "system.columns",
    "system.completions",
    "system.contributors",
    "system.current_roles",
    "system.data_type_families",
    "system.database_engines",
    "system.databases",
    "system.dictionary_layouts",
    "system.disk_types",
    "system.enabled_roles",
    "system.formats",
    "system.functions",
    "system.licenses",
    "system.one",
    "system.privileges",
    "system.quota_usage",
    "system.settings",
    "system.table_engines",
    "system.table_functions",
    "system.tables",
    "system.time_zones",
    "system.user_query_log",
];

/// Laysi/trustiot-v2#260: the system tables a tenant account can read are the expected ones, and
/// none of them shows the trustiot tables or another tenant's data.
async fn system_tables_tell_tenants_nothing(world: &World) {
    let ch = &world.ch;
    let all = ch
        .admin_ok(
            "SELECT database || '.' || name FROM system.tables \
             WHERE database IN ('system', 'information_schema', 'INFORMATION_SCHEMA') \
             ORDER BY 1 FORMAT TSV",
        )
        .await;
    let mut readable = BTreeSet::new();
    let mut failures = Vec::new();
    for table in all.lines() {
        // Bounded, so a readable endless table such as system.numbers cannot hang the test.
        match ch
            .query(
                world.pool_account(),
                &format!("SELECT count() FROM (SELECT 1 FROM {table} LIMIT 1)"),
            )
            .await
        {
            Ok(_) => {
                readable.insert(table.to_owned());
            }
            Err(error) if error.code() == "QUOTA_EXCEEDED" => {
                failures.push(format!("{table}: quota exceeded"));
            }
            Err(_) => {}
        }
    }
    for table in &readable {
        if !READABLE_SYSTEM_TABLES.contains(&table.as_str()) {
            failures.push(format!("a tenant account can read {table}"));
            continue;
        }
        let sql = match table.as_str() {
            // Large license and contributor texts compiled into the server.
            "system.licenses" | "system.contributors" => continue,
            // The account's own queries, whose text names tenant B in this test.
            "system.user_query_log" => {
                "SELECT DISTINCT user FROM system.user_query_log FORMAT TSV".to_owned()
            }
            _ => format!("SELECT * FROM {table} FORMAT TSV"),
        };
        let account = world.pool_account();
        match ch.query(account, &sql).await {
            Ok(body) if world.leaks_b(&body) => {
                failures.push(format!("{table} shows tenant B's data"));
            }
            Ok(body) if table == "system.user_query_log" => {
                if body.lines().any(|user| user != account.user) {
                    failures.push(format!(
                        "system.user_query_log shows other accounts: {body}"
                    ));
                }
            }
            Ok(_) => {}
            Err(error) => failures.push(format!("{table}: {}", error.body)),
        }
    }
    // Only the view is visible in the trustiot databases.
    let visible = ch
        .query(
            world.pool_account(),
            "SELECT database || '.' || name FROM system.tables \
             WHERE database LIKE 'trustiot%' ORDER BY 1 FORMAT TSV",
        )
        .await
        .unwrap_or_else(failed);
    if visible.trim() != TENANT_VIEW {
        failures.push(format!(
            "system.tables shows a tenant account more than the view: {visible}"
        ));
    }
    #[allow(clippy::print_stdout, reason = "test evidence")]
    {
        println!(
            "system tables: {} on the server, readable by a tenant account: {}",
            all.lines().count(),
            readable.iter().cloned().collect::<Vec<_>>().join(", ")
        );
    }
    assert!(
        failures.is_empty(),
        "system tables tell a tenant account too much:\n{}",
        failures.join("\n")
    );
}

/// The tenant role reads only the view, and the view runs as a definer that cannot log in.
async fn tenant_read_path_is_a_definer_view(world: &World) {
    let ch = &world.ch;
    assert_eq!(
        ch.admin_ok("SHOW GRANTS FOR trustiot_tenant_reader")
            .await
            .trim(),
        format!("GRANT SELECT ON {TENANT_VIEW} TO trustiot_tenant_reader")
    );
    assert_eq!(
        ch.admin_ok(&format!("SHOW GRANTS FOR {VIEW_DEFINER}"))
            .await
            .trim(),
        format!("GRANT SELECT ON {TELEMETRY} TO {VIEW_DEFINER}")
    );
    let view = ch
        .admin_ok(
            "SELECT engine, create_table_query FROM system.tables \
             WHERE database = 'trustiot' AND name = 'tenant_telemetry' FORMAT TSV",
        )
        .await;
    assert!(
        view.starts_with("View\t")
            && view.contains(&format!("DEFINER = {VIEW_DEFINER} SQL SECURITY DEFINER")),
        "the tenant view must run as {VIEW_DEFINER}: {view}"
    );
    // No ingest-internal columns.
    assert!(
        !view.contains("stream_id") && !view.contains("batch_seq"),
        "{view}"
    );
    for password in ["", "x"] {
        let error = ch
            .query(
                &Account {
                    user: VIEW_DEFINER.to_owned(),
                    password: password.to_owned(),
                },
                "SELECT 1",
            )
            .await
            .expect_err("the view's definer must not log in");
        assert_eq!(error.code(), "AUTHENTICATION_FAILED", "{}", error.body);
    }
}

/// Dropping the tenant role in a session removes the SELECT grant with it.
async fn set_role_none_loses_access(world: &World) {
    let session = format!("attack-{}", world.suffix);
    let params = [("session_id", session.as_str())];
    world
        .ch
        .query_with(&world.a, "SET ROLE NONE", &params)
        .await
        .unwrap_or_else(|error| panic!("SET ROLE NONE: {}", error.body));
    let error = world
        .ch
        .query_with(
            &world.a,
            &format!("SELECT count() FROM {TENANT_VIEW}"),
            &params,
        )
        .await
        .expect_err("a tenant without its role must not read the view");
    assert_eq!(error.code(), "ACCESS_DENIED", "{}", error.body);
    // The settings profile, attached to the user as well as the role, still applies.
    let error = world
        .ch
        .query_with(&world.a, "SET max_threads = 64", &params)
        .await
        .expect_err("profile constraints must survive SET ROLE NONE");
    assert_eq!(error.code(), "READONLY", "{}", error.body);
}

async fn each_account_sees_its_own_rows(world: &World) {
    let (a, b) = (world.tenant_a, world.tenant_b);
    let count = format!("SELECT count() FROM {TENANT_VIEW}");
    assert_eq!(
        world
            .ch
            .query(&world.a, &count)
            .await
            .unwrap_or_else(failed)
            .trim(),
        A_ROWS.to_string()
    );
    assert_eq!(
        world
            .ch
            .query(&world.b, &count)
            .await
            .unwrap_or_else(failed)
            .trim(),
        B_ROWS.to_string()
    );
    // The service policy (USING 1) lets the ingest account read every tenant.
    let both = format!("SELECT count() FROM {TELEMETRY} WHERE tenant_id IN ('{a}', '{b}')");
    assert_eq!(
        world
            .ch
            .query(&world.ingest, &both)
            .await
            .unwrap_or_else(failed)
            .trim(),
        (A_ROWS + B_ROWS).to_string()
    );
    // A role no policy names reads nothing, although the server default
    // (users_without_row_policies_can_read_rows) would let it read everything.
    assert_eq!(
        world
            .ch
            .query(&world.unscoped, &format!("SELECT count() FROM {TELEMETRY}"))
            .await
            .unwrap_or_else(failed)
            .trim(),
        "0"
    );
}

async fn account_without_tenant_setting_gets_an_error(world: &World) {
    for sql in [
        format!("SELECT count() FROM {TENANT_VIEW}"),
        format!("SELECT * FROM {TENANT_VIEW} LIMIT 1"),
        "SELECT count() FROM merge('trustiot', '^tenant_telemetry$')".to_owned(),
    ] {
        let error: QueryError = world
            .ch
            .query(&world.no_tenant, &sql)
            .await
            .expect_err("a tenant account without SQL_tenant_id must not get rows");
        assert_eq!(error.code(), "UNKNOWN_SETTING", "{sql}: {}", error.body);
        assert!(error.body.contains("SQL_tenant_id"), "{}", error.body);
    }
}

/// The calling account's per-query limits cover the rows read inside the view: the view runs as
/// its definer, but the outer query's read and memory limits still apply.
async fn limits_apply_through_the_view(world: &World) {
    let error = world
        .ch
        .query(
            &world.rows_limited,
            &format!("SELECT count() FROM {TENANT_VIEW} WHERE value_string != ''"),
        )
        .await
        .expect_err("max_rows_to_read = 1 must stop a read of tenant A's rows");
    assert_eq!(error.code(), "TOO_MANY_ROWS", "{}", error.body);
    assert_eq!(
        world
            .ch
            .query(
                &world.memory_limited,
                &format!("SELECT count() FROM {TENANT_VIEW}")
            )
            .await
            .unwrap_or_else(failed)
            .trim(),
        A_ROWS.to_string()
    );
    let error = world
        .ch
        .query(
            &world.memory_limited,
            // An 80 MB array per row; ClickHouse refuses the allocation before making it.
            &format!(
                "SELECT sum(length(range(10000000 + length(value_string)))) FROM {TENANT_VIEW}"
            ),
        )
        .await
        .expect_err("80 MB arrays must exceed max_memory_usage = 50 MB");
    assert_eq!(error.code(), "MEMORY_LIMIT_EXCEEDED", "{}", error.body);
}

/// The quota counts the rows a query reads inside the view for the calling account.
async fn quota_counts_reads_through_the_view(world: &World) {
    let read_so_far = || async {
        world
            .ch
            .query(
                &world.b,
                "SELECT read_rows FROM system.quota_usage WHERE duration = 3600",
            )
            .await
            .unwrap_or_else(failed)
            .trim()
            .parse::<u64>()
            .expect("read_rows of the hourly quota interval")
    };
    let before = read_so_far().await;
    let answer = world
        .ch
        .query_with_stats(
            &world.b,
            &format!("SELECT count() FROM {TENANT_VIEW} WHERE value_string != ''"),
        )
        .await;
    assert_eq!(
        answer.result.unwrap_or_else(failed).trim(),
        B_ROWS.to_string()
    );
    let read = answer
        .read_rows
        .expect("the summary header names the rows read");
    assert!(read >= B_ROWS as u64, "read {read} rows");
    let after = read_so_far().await;
    assert!(
        after >= before + read,
        "the quota counted {} of the {read} rows read through the view",
        after - before
    );
}

/// The quota is keyed by user name: one tenant using up its quota does not block another.
/// The query reads a table: ClickHouse 26.8 does not count constant queries such as `SELECT 1`
/// against a quota.
async fn quota_is_per_user(world: &World) {
    let count_rows = format!("SELECT count() FROM {TENANT_VIEW}");
    assert_eq!(
        world
            .ch
            .admin_ok("SELECT keys FROM system.quotas WHERE name = 'trustiot_tenant_reader'")
            .await
            .trim(),
        "['user_name']"
    );
    let mut exceeded = None;
    for attempt in 1..=400 {
        if let Err(error) = world.ch.query(&world.quota_1, &count_rows).await {
            assert_eq!(error.code(), "QUOTA_EXCEEDED", "{}", error.body);
            exceeded = Some(attempt);
            break;
        }
    }
    let exceeded = exceeded.expect("400 queries within a few seconds must exceed the quota");
    #[allow(clippy::print_stdout, reason = "test evidence")]
    {
        println!("quota: the first account was stopped at query {exceeded}");
    }
    assert_eq!(
        world
            .ch
            .query(&world.quota_2, &count_rows)
            .await
            .unwrap_or_else(failed)
            .trim(),
        "0"
    );
    assert!(world.ch.query(&world.quota_1, &count_rows).await.is_err());
    // The quota is keyed by the user name: a client-supplied key or a fresh session does not
    // start a new counter.
    for params in [
        [("quota_key", "another-key")],
        [("session_id", "fresh-session")],
    ] {
        let error = world
            .ch
            .query_with(&world.quota_1, &count_rows, &params)
            .await
            .expect_err("the exhausted quota must not be bypassed");
        assert_eq!(error.code(), "QUOTA_EXCEEDED", "{params:?}: {}", error.body);
    }
}

/// The ingest account writes and deletes data but cannot change the schema or access control.
async fn ingest_account_cannot_change_schema(world: &World) {
    let stream = Uuid::new_v4();
    let tenant = world.tenant_a;
    let insert = format!(
        "INSERT INTO trustiot.telemetry (tenant_id, gateway_id, stream_id, batch_seq, device_id, \
         point, ts, received_at, value_type, value_int) \
         SELECT '{tenant}', generateUUIDv4(), '{stream}', 7, generateUUIDv4(), 'counter', \
         now64(3, 'UTC'), now64(3, 'UTC'), 'int', number FROM numbers(5)"
    );
    world
        .ch
        .query(&world.ingest, &insert)
        .await
        .unwrap_or_else(failed);
    let in_batch = format!(
        "SELECT count() FROM trustiot.telemetry WHERE stream_id = '{stream}' AND batch_seq = 7"
    );
    assert_eq!(
        world
            .ch
            .query(&world.ingest, &in_batch)
            .await
            .unwrap_or_else(failed)
            .trim(),
        "5"
    );
    // Tenant A sees the batch through its view, and stops seeing it once it is deleted.
    let tenant_sees = format!("SELECT count() FROM {TENANT_VIEW} WHERE point = 'counter'");
    assert_eq!(
        world
            .ch
            .query(&world.a_extra, &tenant_sees)
            .await
            .unwrap_or_else(failed)
            .trim(),
        "5"
    );
    // Rows of a batch without a completion record are removed before the batch is written
    // again (I1): a lightweight DELETE FROM, which needs the ALTER DELETE grant; runtime code
    // never writes ALTER TABLE (tools/repo-check).
    world
        .ch
        .query_with(
            &world.ingest,
            &format!(
                "DELETE FROM trustiot.telemetry WHERE stream_id = '{stream}' AND batch_seq = 7"
            ),
            &[("lightweight_deletes_sync", "2")],
        )
        .await
        .unwrap_or_else(|error| panic!("lightweight delete: {}", error.body));
    assert_eq!(
        world
            .ch
            .query(&world.ingest, &in_batch)
            .await
            .unwrap_or_else(failed)
            .trim(),
        "0"
    );
    assert_eq!(
        world
            .ch
            .query(&world.a_extra, &tenant_sees)
            .await
            .unwrap_or_else(failed)
            .trim(),
        "0"
    );
    world
        .ch
        .query(
            &world.ingest,
            &format!(
                "INSERT INTO trustiot.batch_commits (tenant_id, gateway_id, stream_id, seq, \
                 row_count, committed_at) VALUES ('{tenant}', generateUUIDv4(), '{stream}', 7, 5, \
                 now64(3, 'UTC'))"
            ),
        )
        .await
        .unwrap_or_else(failed);
    assert_eq!(
        world
            .ch
            .query(
                &world.ingest,
                &format!(
                    "SELECT max(seq) FROM trustiot.batch_commits WHERE stream_id = '{stream}'"
                )
            )
            .await
            .unwrap_or_else(failed)
            .trim(),
        "7"
    );

    for sql in [
        "CREATE TABLE trustiot.injected (a UInt8) ENGINE = MergeTree ORDER BY a",
        "ALTER TABLE trustiot.telemetry ADD COLUMN injected UInt8",
        "ALTER TABLE trustiot.telemetry MODIFY TTL toDateTime(received_at) + INTERVAL 1 DAY",
        "ALTER TABLE trustiot.telemetry MODIFY SETTING fsync_after_insert = 0",
        "DROP TABLE trustiot.telemetry",
        "TRUNCATE TABLE trustiot.telemetry",
        "CREATE USER injected IDENTIFIED WITH no_password",
        "ALTER ROW POLICY tenant_isolation ON trustiot.telemetry USING 1",
        "CREATE DATABASE injected",
        "CREATE VIEW trustiot.injected_view AS SELECT * FROM trustiot.telemetry",
        "CREATE ROW POLICY injected ON trustiot.telemetry USING 1 TO CURRENT_USER",
        "DROP DATABASE trustiot",
        "RENAME TABLE trustiot.telemetry TO trustiot.telemetry_old",
        "EXCHANGE TABLES trustiot.telemetry AND trustiot.batch_commits",
        "ALTER TABLE trustiot.telemetry DROP COLUMN value_string",
        "ALTER TABLE trustiot.telemetry MODIFY COLUMN value_string UInt8",
        "ALTER TABLE trustiot.telemetry DROP CONSTRAINT tenant_id_set",
        "ALTER TABLE trustiot.telemetry ADD INDEX injected value_string TYPE bloom_filter GRANULARITY 1",
        "ALTER TABLE trustiot.telemetry ADD PROJECTION injected (SELECT * ORDER BY tenant_id)",
        "ALTER TABLE trustiot.telemetry MODIFY ORDER BY (tenant_id, ts)",
        "ALTER TABLE trustiot.telemetry UPDATE tenant_id = generateUUIDv4() WHERE 1",
        "ALTER TABLE trustiot.telemetry FREEZE",
        "ALTER TABLE trustiot.telemetry MATERIALIZE TTL",
        "OPTIMIZE TABLE trustiot.telemetry FINAL",
        "GRANT SELECT ON *.* TO CURRENT_USER",
        "GRANT trustiot_tenant_reader TO CURRENT_USER",
        "ALTER TABLE trustiot.tenant_telemetry MODIFY QUERY SELECT * FROM trustiot.telemetry",
        "ALTER TABLE trustiot.tenant_telemetry MODIFY SQL SECURITY INVOKER",
    ] {
        let error = world
            .ch
            .query(&world.ingest, sql)
            .await
            .expect_err("the ingest account must not change the schema");
        assert_eq!(error.code(), "ACCESS_DENIED", "{sql}: {}", error.body);
    }

    // Constraints reject rows without a tenant and rows whose value sits in the wrong column.
    for (values, constraint) in [
        (
            "('00000000-0000-0000-0000-000000000000', 'p', 'float', 1, 0)",
            "tenant_id_set",
        ),
        (
            &*format!("('{tenant}', 'p', 'float', 1, 2)"),
            "value_matches_type",
        ),
    ] {
        let error = world
            .ch
            .query(
                &world.ingest,
                &format!(
                    "INSERT INTO trustiot.telemetry (tenant_id, point, value_type, value_float, \
                     value_int) VALUES {values}"
                ),
            )
            .await
            .expect_err("the constraint must reject the row");
        assert!(error.body.contains(constraint), "{}", error.body);
    }
}

#[derive(Debug)]
struct TableInfo {
    engine: String,
    engine_full: String,
}

async fn table(ch: &ClickHouse, name: &str) -> TableInfo {
    let row = ch
        .admin_ok(&format!(
            "SELECT engine, engine_full FROM system.tables \
             WHERE database = 'trustiot' AND name = '{name}' FORMAT JSONEachRow"
        ))
        .await;
    let row: serde_json::Value =
        serde_json::from_str(row.trim()).unwrap_or_else(|error| panic!("{name}: {error}: {row}"));
    TableInfo {
        engine: row["engine"].as_str().expect("a string column").to_owned(),
        engine_full: row["engine_full"]
            .as_str()
            .expect("a string column")
            .to_owned(),
    }
}

/// The TTL interval in days of `TTL toDateTime(<column>) + toIntervalDay(<days>)`.
fn ttl_days(info: &TableInfo, column: &str) -> u64 {
    let prefix = format!("TTL toDateTime({column}) + toIntervalDay(");
    let (_, rest) = info
        .engine_full
        .split_once(&prefix)
        .unwrap_or_else(|| panic!("no TTL on {column}: {}", info.engine_full));
    rest.split(')')
        .next()
        .and_then(|days| days.parse().ok())
        .unwrap_or_else(|| panic!("unreadable TTL: {}", info.engine_full))
}

/// Reads the table settings back from the server: fsync is on, engines are MergeTree, partitions
/// and retention follow the server receive time, and completion records outlive the rows.
async fn table_settings_are_in_place(ch: &ClickHouse) {
    let telemetry = table(ch, "telemetry").await;
    let commits = table(ch, "batch_commits").await;
    for info in [&telemetry, &commits] {
        assert!(info.engine.ends_with("MergeTree"), "{info:?}");
        for setting in ["fsync_after_insert = 1", "fsync_part_directory = 1"] {
            assert!(
                info.engine_full.contains(setting),
                "{setting} missing: {info:?}"
            );
        }
    }
    assert!(
        telemetry
            .engine_full
            .contains("non_replicated_deduplication_window = 1000"),
        "{telemetry:?}"
    );
    assert!(
        telemetry
            .engine_full
            .contains("PARTITION BY toYYYYMM(received_at)"),
        "{telemetry:?}"
    );
    let telemetry_days = ttl_days(&telemetry, "received_at");
    let commit_days = ttl_days(&commits, "committed_at");
    assert!(
        commit_days >= telemetry_days,
        "batch_commits keeps records {commit_days} days, shorter than telemetry's {telemetry_days}"
    );
    #[allow(clippy::print_stdout, reason = "test evidence")]
    {
        println!("telemetry: {}", telemetry.engine_full);
        println!("batch_commits: {}", commits.engine_full);
    }
    // Only MergeTree-family tables in the TrustIoT databases, plus the tenant view
    // (tenant_read_path_is_a_definer_view checks it).
    let others = ch
        .admin_ok(
            "SELECT database || '.' || name FROM system.tables \
             WHERE database IN ('trustiot', 'trustiot_logs') AND NOT endsWith(engine, 'MergeTree') \
             FORMAT TSV",
        )
        .await;
    assert_eq!(others.trim(), TENANT_VIEW);
    assert_eq!(
        ch.admin_ok("SELECT count() FROM system.databases WHERE name = 'trustiot_logs'")
            .await
            .trim(),
        "1"
    );
}

/// Access entities live in ClickHouse's own access storage, created by SQL, not in server
/// configuration files (decision 14).
async fn access_entities_come_from_sql(ch: &ClickHouse) {
    let rows = ch
        .admin_ok(
            "SELECT 'role', name, storage FROM system.roles WHERE startsWith(name, 'trustiot_') \
             UNION ALL SELECT 'row policy', short_name || ' ON ' || table, storage \
                 FROM system.row_policies WHERE database = 'trustiot' \
             UNION ALL SELECT 'settings profile', name, storage \
                 FROM system.settings_profiles WHERE startsWith(name, 'trustiot_') \
             UNION ALL SELECT 'quota', name, storage FROM system.quotas \
                 WHERE startsWith(name, 'trustiot_') \
             UNION ALL SELECT 'user', name, storage FROM system.users \
                 WHERE name = 'trustiot_tenant_view_definer' \
             ORDER BY 1, 2",
        )
        .await;
    #[allow(clippy::print_stdout, reason = "test evidence")]
    {
        println!("access entities:\n{}", rows.trim());
    }
    let lines: Vec<&str> = rows.lines().collect();
    // 2 roles, 8 row policies (5 on the tables for the roles, 1 for the view's definer, 2 on the
    // view), 1 settings profile, 1 quota, 1 user (the view's definer).
    assert_eq!(lines.len(), 13, "{rows}");
    for line in lines {
        assert!(
            line.ends_with("\tlocal_directory"),
            "not created by SQL: {line}"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn clickhouse_schema_and_tenant_isolation() {
    let Some(ch) = ClickHouse::start().await else {
        return;
    };
    // Twice: the migration Job may apply the files again on every deploy.
    ch.apply_migrations().await;
    ch.apply_migrations().await;

    let world = World::new(ch).await;
    world.insert_rows().await;
    // Create the system log tables (query_log, part_log, text_log), so the attacks on them meet
    // the access check rather than a missing table.
    world.ch.admin_ok("SYSTEM FLUSH LOGS").await;
    // The Laysi/trustiot-v2#260 checks come first: they list every way to reach the tables,
    // rather than stopping at the first, and on a schema without the tenant view they are the
    // first to fail.
    introspection_tells_nothing_of_other_tenants(&world).await;
    system_tables_tell_tenants_nothing(&world).await;
    table_settings_are_in_place(&world.ch).await;
    access_entities_come_from_sql(&world.ch).await;
    tenant_read_path_is_a_definer_view(&world).await;
    each_account_sees_its_own_rows(&world).await;
    tenant_attacks_read_nothing_of_another_tenant(&world).await;
    deny_by_default_covers_new_accounts(&world).await;
    set_role_none_loses_access(&world).await;
    account_without_tenant_setting_gets_an_error(&world).await;
    limits_apply_through_the_view(&world).await;
    quota_counts_reads_through_the_view(&world).await;
    ingest_account_cannot_change_schema(&world).await;
    quota_is_per_user(&world).await;
    #[allow(clippy::print_stdout, reason = "test evidence")]
    {
        println!(
            "server: {}",
            first_line(&world.ch.admin_ok("SELECT version()").await)
        );
    }
    world.cleanup().await;
}
