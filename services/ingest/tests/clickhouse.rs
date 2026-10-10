//! The ClickHouse schema against a real server: the migrations apply (twice), the tables have
//! the settings decision 8 and 14 need, and tenant isolation holds against the attack types of
//! the 2026-10-09 feasibility test (docs/feasibility/2026-10-09-cloud-poc.md).
//!
//! Uses `TEST_CLICKHOUSE_URL` (with `TEST_CLICKHOUSE_USER` and `TEST_CLICKHOUSE_PASSWORD`) when
//! set; otherwise starts a throwaway ClickHouse container with podman or docker; skipped
//! without either unless `CI` is set. Every account and tenant is unique to the run, so a
//! shared server works too.
//!
//! Run with `--nocapture` to see each attack and its outcome.

mod common;

use common::{Account, ClickHouse, QueryError};
use uuid::Uuid;

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
    b: Account,
    /// Has the tenant role but no SQL_tenant_id setting.
    no_tenant: Account,
    /// Can SELECT the table through a role that no row policy names.
    unscoped: Account,
    quota_1: Account,
    quota_2: Account,
}

const A_ROWS: usize = 3;
const B_ROWS: usize = 4;

fn password() -> String {
    Uuid::new_v4().simple().to_string()
}

/// Creates a tenant account with the two statements documented in 0004_access.sql.
async fn create_tenant_account(ch: &ClickHouse, user: &str, tenant: Option<Uuid>) -> Account {
    let account = Account {
        user: user.to_owned(),
        password: password(),
    };
    let tenant_setting = tenant
        .map(|tenant| format!(", SQL_tenant_id = '{tenant}' CONST"))
        .unwrap_or_default();
    ch.admin_ok(&format!(
        "CREATE USER {user} IDENTIFIED WITH sha256_password BY '{}' \
         SETTINGS PROFILE 'trustiot_tenant_reader'{tenant_setting}",
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
        let b_secret = format!("B-SECRET-{suffix}");
        Self {
            ch,
            suffix,
            tenant_a,
            tenant_b,
            b_secret,
            ingest,
            a,
            b,
            no_tenant,
            unscoped,
            quota_1,
            quota_2,
        }
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
            &self.b,
            &self.no_tenant,
            &self.unscoped,
            &self.quota_1,
            &self.quota_2,
        ] {
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
/// tried (setting overrides, table functions, impersonation, writes).
fn attacks(world: &World) -> Vec<Attack> {
    let b = world.tenant_b;
    let a = world.tenant_a;
    let a_count = A_ROWS.to_string();
    let a_values = (0..A_ROWS)
        .map(|n| format!("a-row-{n}"))
        .collect::<Vec<_>>()
        .join("\n");
    vec![
        // Direct queries.
        attack(
            "direct: count every row",
            "SELECT count() FROM trustiot.telemetry",
            answer(&a_count),
        ),
        attack(
            "direct: read every value",
            "SELECT value_string FROM trustiot.telemetry ORDER BY value_string",
            answer(&a_values),
        ),
        attack(
            "direct: filter on tenant B",
            format!("SELECT count() FROM trustiot.telemetry WHERE tenant_id = '{b}'"),
            answer("0"),
        ),
        attack(
            "direct: tenant B OR true",
            format!("SELECT count() FROM trustiot.telemetry WHERE tenant_id = '{b}' OR 1 = 1"),
            answer(&a_count),
        ),
        attack(
            "direct: PREWHERE tenant B",
            format!("SELECT count() FROM trustiot.telemetry PREWHERE tenant_id = '{b}'"),
            answer("0"),
        ),
        attack(
            "direct: group by tenant",
            "SELECT tenant_id, count() FROM trustiot.telemetry GROUP BY tenant_id",
            answer(format!("{a}\t{a_count}")),
        ),
        attack(
            "direct: virtual columns",
            "SELECT count() FROM trustiot.telemetry WHERE _part != '' AND _partition_id != ''",
            answer(&a_count),
        ),
        attack(
            "direct: throwIf error oracle",
            format!(
                "SELECT count() FROM trustiot.telemetry WHERE throwIf(tenant_id = '{b}', 'oracle') = 0"
            ),
            answer(&a_count),
        ),
        // JOIN.
        attack(
            "JOIN: self join on the shared device",
            format!(
                "SELECT count() FROM trustiot.telemetry AS x INNER JOIN trustiot.telemetry AS y ON x.device_id = y.device_id WHERE y.tenant_id = '{b}'"
            ),
            answer("0"),
        ),
        attack(
            "JOIN: cross join",
            "SELECT count() FROM trustiot.telemetry AS x CROSS JOIN trustiot.telemetry AS y",
            answer((A_ROWS * A_ROWS).to_string()),
        ),
        attack(
            "JOIN: LEFT join to tenant B",
            format!(
                "SELECT countIf(y.tenant_id = '{b}') FROM trustiot.telemetry AS x LEFT JOIN trustiot.telemetry AS y ON x.point = y.point"
            ),
            answer("0"),
        ),
        // Subqueries.
        attack(
            "subquery: IN",
            format!(
                "SELECT count() FROM trustiot.telemetry WHERE tenant_id IN (SELECT tenant_id FROM trustiot.telemetry WHERE tenant_id = '{b}')"
            ),
            answer("0"),
        ),
        attack(
            "subquery: scalar",
            "SELECT (SELECT count() FROM trustiot.telemetry)",
            answer(&a_count),
        ),
        attack(
            "subquery: FROM",
            "SELECT count() FROM (SELECT * FROM trustiot.telemetry)",
            answer(&a_count),
        ),
        attack(
            "subquery: CTE",
            "WITH t AS (SELECT * FROM trustiot.telemetry) SELECT count() FROM t",
            answer(&a_count),
        ),
        // UNION.
        attack(
            "UNION ALL with tenant B filter",
            format!(
                "SELECT count() FROM (SELECT tenant_id FROM trustiot.telemetry UNION ALL SELECT tenant_id FROM trustiot.telemetry WHERE tenant_id = '{b}')"
            ),
            answer(&a_count),
        ),
        attack(
            "UNION DISTINCT of tenants",
            "SELECT tenant_id FROM trustiot.telemetry UNION DISTINCT SELECT tenant_id FROM trustiot.telemetry",
            answer(a.to_string()),
        ),
        // FINAL.
        attack(
            "FINAL",
            "SELECT count() FROM trustiot.telemetry FINAL",
            Expect::Denied,
        ),
        // Table functions.
        attack(
            "merge(): the telemetry table",
            "SELECT count() FROM merge('trustiot', '^telemetry$')",
            answer(&a_count),
        ),
        attack(
            "merge(): every table in the database",
            "SELECT count() FROM merge('trustiot', '.*')",
            answer(&a_count),
        ),
        attack(
            "view()",
            "SELECT count() FROM view(SELECT * FROM trustiot.telemetry)",
            answer(&a_count),
        ),
        attack(
            "view() filtered on tenant B",
            format!(
                "SELECT count() FROM view(SELECT * FROM trustiot.telemetry WHERE tenant_id = '{b}')"
            ),
            answer("0"),
        ),
        attack(
            "mergeTreeIndex()",
            "SELECT * FROM mergeTreeIndex('trustiot', 'telemetry', with_marks = true)",
            Expect::Denied,
        ),
        attack(
            "remote() to this server",
            "SELECT count() FROM remote('127.0.0.1', trustiot.telemetry)",
            Expect::Denied,
        ),
        attack(
            "cluster()",
            "SELECT count() FROM cluster('default', trustiot.telemetry)",
            Expect::Denied,
        ),
        attack(
            "loop()",
            "SELECT count() FROM loop(trustiot.telemetry) LIMIT 1",
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
            format!("SELECT count() FROM trustiot.telemetry SETTINGS SQL_tenant_id = '{b}'"),
            Expect::Denied,
        ),
        Attack {
            name: "URL parameter SQL_tenant_id = B",
            sql: "SELECT count() FROM trustiot.telemetry".to_owned(),
            params: vec![("SQL_tenant_id", b.to_string())],
            expect: Expect::Denied,
        },
        attack(
            "query SETTINGS additional_table_filters",
            "SELECT count() FROM trustiot.telemetry SETTINGS additional_table_filters = {'trustiot.telemetry': '1'}",
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
                "EXECUTE AS {} SELECT count() FROM trustiot.telemetry",
                world.ingest.user
            ),
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
                "INSERT INTO trustiot.telemetry (tenant_id, point, value_type) VALUES ('{b}', 'x', 'float')"
            ),
            Expect::Denied,
        ),
        attack(
            "tenant lightweight DELETE",
            format!("DELETE FROM trustiot.telemetry WHERE tenant_id = '{b}'"),
            Expect::Denied,
        ),
        attack(
            "tenant ALTER TABLE DELETE",
            format!("ALTER TABLE trustiot.telemetry DELETE WHERE tenant_id = '{b}'"),
            Expect::Denied,
        ),
        attack(
            "CREATE VIEW",
            "CREATE VIEW trustiot.leak AS SELECT * FROM trustiot.telemetry",
            Expect::Denied,
        ),
        attack(
            "CREATE ROW POLICY",
            "CREATE ROW POLICY leak ON trustiot.telemetry USING 1 TO CURRENT_USER",
            Expect::Denied,
        ),
        attack(
            "GRANT to self",
            "GRANT SELECT ON *.* TO CURRENT_USER",
            Expect::Denied,
        ),
    ]
}

async fn tenant_attacks_read_nothing_of_another_tenant(world: &World) {
    let mut failures = Vec::new();
    #[allow(clippy::print_stdout, reason = "the attack list is the test evidence")]
    {
        println!("attack | outcome");
        for attack in attacks(world) {
            let params: Vec<(&str, &str)> = attack
                .params
                .iter()
                .map(|(name, value)| (*name, value.as_str()))
                .collect();
            let result = world.ch.query_with(&world.a, &attack.sql, &params).await;
            let outcome = match &result {
                Ok(body) => format!("answer {:?}", body.trim()),
                Err(error) => format!("refused {}", error.code()),
            };
            println!("{} | {outcome}", attack.name);
            let text = match &result {
                Ok(body) => body.as_str(),
                Err(error) => error.body.as_str(),
            };
            let as_expected = match (&attack.expect, &result) {
                (Expect::Answer(expected), Ok(body)) => body.trim() == expected,
                (Expect::Denied, Err(_)) => true,
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
        .query_with(&world.a, "SELECT count() FROM trustiot.telemetry", &params)
        .await
        .expect_err("a tenant without its role must not read the table");
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
    let count = "SELECT count() FROM trustiot.telemetry";
    assert_eq!(
        world
            .ch
            .query(&world.a, count)
            .await
            .unwrap_or_else(failed)
            .trim(),
        A_ROWS.to_string()
    );
    assert_eq!(
        world
            .ch
            .query(&world.b, count)
            .await
            .unwrap_or_else(failed)
            .trim(),
        B_ROWS.to_string()
    );
    // The service policy (USING 1) lets the ingest account read every tenant.
    let both = format!("SELECT count() FROM trustiot.telemetry WHERE tenant_id IN ('{a}', '{b}')");
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
            .query(&world.unscoped, count)
            .await
            .unwrap_or_else(failed)
            .trim(),
        "0"
    );
}

async fn account_without_tenant_setting_gets_an_error(world: &World) {
    for sql in [
        "SELECT count() FROM trustiot.telemetry",
        "SELECT * FROM trustiot.telemetry LIMIT 1",
        "SELECT count() FROM merge('trustiot', '^telemetry$')",
    ] {
        let error: QueryError = world
            .ch
            .query(&world.no_tenant, sql)
            .await
            .expect_err("a tenant account without SQL_tenant_id must not get rows");
        assert_eq!(error.code(), "UNKNOWN_SETTING", "{sql}: {}", error.body);
        assert!(error.body.contains("SQL_tenant_id"), "{}", error.body);
    }
}

/// The quota is keyed by user name: one tenant using up its quota does not block another.
/// The query reads a table: ClickHouse 26.8 does not count constant queries such as `SELECT 1`
/// against a quota.
async fn quota_is_per_user(world: &World) {
    const COUNT_ROWS: &str = "SELECT count() FROM trustiot.telemetry";
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
        if let Err(error) = world.ch.query(&world.quota_1, COUNT_ROWS).await {
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
            .query(&world.quota_2, COUNT_ROWS)
            .await
            .unwrap_or_else(failed)
            .trim(),
        "0"
    );
    assert!(world.ch.query(&world.quota_1, COUNT_ROWS).await.is_err());
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
    // Only MergeTree-family tables in the TrustIoT databases.
    let others = ch
        .admin_ok(
            "SELECT count() FROM system.tables WHERE database IN ('trustiot', 'trustiot_logs') \
             AND NOT endsWith(engine, 'MergeTree')",
        )
        .await;
    assert_eq!(others.trim(), "0");
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
             ORDER BY 1, 2",
        )
        .await;
    #[allow(clippy::print_stdout, reason = "test evidence")]
    {
        println!("access entities:\n{}", rows.trim());
    }
    let lines: Vec<&str> = rows.lines().collect();
    // 2 roles, 5 row policies, 1 settings profile, 1 quota.
    assert_eq!(lines.len(), 9, "{rows}");
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
    table_settings_are_in_place(&ch).await;
    access_entities_come_from_sql(&ch).await;

    let world = World::new(ch).await;
    world.insert_rows().await;
    // Create the system log tables (query_log, part_log, text_log), so the attacks on them meet
    // the access check rather than a missing table.
    world.ch.admin_ok("SYSTEM FLUSH LOGS").await;
    each_account_sees_its_own_rows(&world).await;
    tenant_attacks_read_nothing_of_another_tenant(&world).await;
    set_role_none_loses_access(&world).await;
    account_without_tenant_setting_gets_an_error(&world).await;
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
