//! The Postgres store of the suite: tables with row-level security (decision 29), attacked
//! through service-kit's [`TenantDb`], the same path the service's handlers use.
//!
//! Declare each table with [`pg_table!`](crate::pg_table), each view with
//! [`pg_view!`](crate::pg_view), and everything else the service role can use with
//! [`PgStore::exempt`]. The macros build every probe's SQL at compile time from the name (a
//! string literal), so the SQL stays static as `tools/sql-check` requires.
//!
//! | Probe | Passes when |
//! | --- | --- |
//! | coverage | every table, view, materialized view, and foreign table the service role has any privilege on (outside system schemas and extensions) is declared or exempt; tenant tables have `tenant_id`; exempt ones do not |
//! | seed | the seeder (the migration role or a superuser, exempt from row-level security) runs the table's seed SQL for each of the rows the suite asks per tenant and reads every row back by its id or canary |
//! | session | the kit accepts the service role's sessions (its session check passes) |
//! | read | in a tenant transaction the attacker sees none of the victim's markers, and sees each of its own seeded rows (a view: at least one row); or is refused (`42501`) |
//! | read without a tenant | outside a tenant transaction no row is visible, or the read is refused |
//! | update own rows | `UPDATE ... SET tenant_id = tenant_id RETURNING` returns none of the victim's markers, or is refused |
//! | move into the victim | `UPDATE ... SET tenant_id = <victim>` changes no row: refused by `WITH CHECK` (`42501`) |
//! | insert into the victim | the seed SQL with the victim's tenant is refused (`42501`) |
//!
//! Every write probe runs in its own transaction and is rolled back.

use std::collections::BTreeMap;
use std::time::Duration;

use service_kit::tenant::TenantDb;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{Connection, PgConnection};
use uuid::Uuid;

use super::{Attack, Report, Seeded, SeededRow, Side, Store, World, shows};

/// SQLSTATE `insufficient_privilege`: a row-level security refusal or a missing grant.
const INSUFFICIENT_PRIVILEGE: &str = "42501";

/// How long the service role's pool waits for a session; the kit refuses unsafe sessions until
/// this runs out.
const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(5);

/// The relations the service role can reach: tables, partitioned tables, views, materialized
/// views, and foreign tables with any privilege granted to it (directly, through a role it
/// belongs to, or through PUBLIC), outside the system schemas and extensions. `$1` is the role.
const REACHABLE_RELATIONS: &str = "
SELECT n.nspname || '.' || c.relname AS name,
       c.relkind::text AS kind,
       EXISTS (
           SELECT 1 FROM pg_attribute AS a
           WHERE a.attrelid = c.oid AND a.attname = 'tenant_id' AND a.attnum > 0
             AND NOT a.attisdropped
       ) AS has_tenant_id
FROM pg_class AS c
JOIN pg_namespace AS n ON n.oid = c.relnamespace
WHERE c.relkind IN ('r', 'p', 'v', 'm', 'f')
  AND n.nspname NOT IN ('pg_catalog', 'information_schema')
  AND n.nspname NOT LIKE 'pg\\_toast%'
  AND n.nspname NOT LIKE 'pg\\_temp\\_%'
  AND NOT EXISTS (
      SELECT 1 FROM pg_depend AS d
      WHERE d.classid = 'pg_class'::regclass AND d.objid = c.oid AND d.deptype = 'e'
  )
  AND has_schema_privilege($1, n.oid, 'USAGE')
  AND (
      has_table_privilege($1, c.oid, 'SELECT, INSERT, UPDATE, DELETE, TRUNCATE, REFERENCES, TRIGGER')
      OR has_any_column_privilege($1, c.oid, 'SELECT, INSERT, UPDATE, REFERENCES')
  )
ORDER BY 1";

/// Whether a declaration is a table or a view.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RelationKind {
    /// A table or partitioned table with a `tenant_id` column.
    Table,
    /// A view (`security_invoker`) over tenant tables.
    View,
}

/// The static SQL of one declaration. Made by [`pg_table!`](crate::pg_table) and
/// [`pg_view!`](crate::pg_view); not meant to be written by hand.
#[doc(hidden)]
#[derive(Clone, Copy, Debug)]
pub struct TableSql {
    /// The name as declared, `table` or `schema.table`.
    pub name: &'static str,
    /// Table or view.
    pub kind: RelationKind,
    /// Every visible row as text.
    pub read: &'static str,
    /// Touches every visible row without changing it, returning each as text.
    pub update_own: Option<&'static str>,
    /// Moves every visible row into the tenant `$1`.
    pub move_to: Option<&'static str>,
    /// Inserts one row: `$1` tenant id (uuid), `$2` row id (uuid), `$3` canary (text), and `$4`
    /// the owner (uuid) when [`TableSql::owned`].
    pub seed: Option<&'static str>,
    /// The table holds at most one row per tenant (its key is `tenant_id`): one row is seeded
    /// per tenant, and that row's id is the tenant id.
    pub one_per_tenant: bool,
    /// The rows belong to a user, and the service serves a user only their own: the seed gets
    /// the run's user ([`World::user`]) as `$4`, in both tenants.
    pub owned: bool,
}

/// A table or view in the suite. Make one with [`pg_table!`](crate::pg_table) or
/// [`pg_view!`](crate::pg_view).
#[derive(Clone, Copy, Debug)]
pub struct PgTable {
    sql: TableSql,
}

impl PgTable {
    #[doc(hidden)]
    #[must_use]
    pub fn from_sql(sql: TableSql) -> Self {
        Self { sql }
    }

    /// The name as declared.
    #[must_use]
    pub fn name(&self) -> &'static str {
        self.sql.name
    }
}

/// Declares a tenant table: `pg_table!("devices", seed = "INSERT INTO devices (tenant_id, id,
/// name) VALUES ($1, $2, $3)")`.
///
/// The name is a string literal, `table` (in `public`) or `schema.table`, lower case. The seed
/// is static SQL that inserts one row as the seeder: `$1` is the tenant id (uuid), `$2` a new row
/// id (uuid), `$3` a canary (text). Store `$2` or `$3` (ideally both) where a read of the row
/// shows them. A table whose rows need a parent row inserts both in one statement:
/// `WITH parent AS (INSERT INTO devices (tenant_id, id, name) VALUES ($1, gen_random_uuid(), $3)
/// RETURNING tenant_id, id) INSERT INTO points (tenant_id, id, device_id, name) SELECT
/// tenant_id, $2, id, $3 FROM parent`.
///
/// A table with one row per tenant (`tenant_id` is its key, such as a tenant's settings) adds
/// `one_per_tenant`: `pg_table!("assets.tenants", seed = "...", one_per_tenant)`. The suite then
/// seeds one row per tenant whatever the endpoints ask for, and that row's id (`$2`) is the
/// tenant id, so a read that shows the row's `tenant_id` shows the row. Postgres needs every
/// parameter to have a type, so a seed that does not store `$2` or `$3` still names them, for
/// example `INSERT INTO assets.tenants (tenant_id) SELECT $1 WHERE $2::uuid IS NOT NULL AND
/// $3::text IS NOT NULL`.
///
/// A table whose rows belong to a user, served only to that user (such as the assistant's
/// conversations), adds `owned`: `pg_table!("assistant.conversations", seed = "...", owned)`.
/// The seed then gets a fourth parameter, `$4`, the owner (uuid): the run's user, the same in
/// both tenants and the user of every probe's token
/// ([`World::user`](crate::isolation::World::user)), so the endpoints' positive controls see the
/// rows, and only the tenant keeps the two tenants' rows apart.
#[macro_export]
macro_rules! pg_table {
    ($table:literal, seed = $seed:expr $(,)?) => {
        $crate::pg_table!(@table $table, $seed, false, false)
    };
    ($table:literal, seed = $seed:expr, one_per_tenant $(,)?) => {
        $crate::pg_table!(@table $table, $seed, true, false)
    };
    ($table:literal, seed = $seed:expr, owned $(,)?) => {
        $crate::pg_table!(@table $table, $seed, false, true)
    };
    (@table $table:literal, $seed:expr, $one_per_tenant:literal, $owned:literal) => {
        $crate::isolation::postgres::PgTable::from_sql($crate::isolation::postgres::TableSql {
            name: $table,
            kind: $crate::isolation::postgres::RelationKind::Table,
            read: concat!("SELECT testkit_row::text FROM ", $table, " AS testkit_row"),
            update_own: Some(concat!(
                "UPDATE ",
                $table,
                " AS testkit_row SET tenant_id = tenant_id RETURNING testkit_row::text"
            )),
            move_to: Some(concat!("UPDATE ", $table, " SET tenant_id = $1")),
            seed: Some($seed),
            one_per_tenant: $one_per_tenant,
            owned: $owned,
        })
    };
}

/// Declares a view over tenant tables: `pg_view!("device_summaries")`. Views are only read:
/// the attacker must see at least one row and none of the victim's markers.
#[macro_export]
macro_rules! pg_view {
    ($view:literal $(,)?) => {
        $crate::isolation::postgres::PgTable::from_sql($crate::isolation::postgres::TableSql {
            name: $view,
            kind: $crate::isolation::postgres::RelationKind::View,
            read: concat!("SELECT testkit_row::text FROM ", $view, " AS testkit_row"),
            update_own: None,
            move_to: None,
            seed: None,
            one_per_tenant: false,
            owned: false,
        })
    };
}

/// The service's Postgres database: how to reach it as the seeder and as the service role, and
/// what the suite declares about its relations.
#[derive(Debug)]
pub struct PgStore {
    seeder: PgConnectOptions,
    service: PgConnectOptions,
    tables: Vec<PgTable>,
    exempt: Vec<(&'static str, &'static str)>,
}

impl PgStore {
    /// `seeder` connects as a role exempt from row-level security (the migration role that owns
    /// the tables, or a superuser); `service` as the service role (`verify-full`, as the service
    /// connects). The service role's name is taken from `service`.
    #[must_use]
    pub fn new(seeder: PgConnectOptions, service: PgConnectOptions) -> Self {
        Self {
            seeder,
            service,
            tables: Vec::new(),
            exempt: Vec::new(),
        }
    }

    /// Adds a table or view to attack.
    #[must_use]
    pub fn table(mut self, table: PgTable) -> Self {
        self.tables.push(table);
        self
    }

    /// Leaves a relation the service role can use out of the attacks, for `reason`: it holds no
    /// tenant data. A relation with a `tenant_id` column cannot be exempt.
    #[must_use]
    pub fn exempt(mut self, relation: &'static str, reason: &'static str) -> Self {
        self.exempt.push((relation, reason));
        self
    }

    async fn seeder(&self, report: &mut Report) -> Option<PgConnection> {
        match PgConnection::connect_with(&self.seeder).await {
            Ok(connection) => Some(connection),
            Err(error) => {
                report.fail("postgres", "connect as the seeder", error.to_string());
                None
            }
        }
    }

    async fn attack_table(
        &self,
        db: &TenantDb,
        table: &PgTable,
        attack: &Attack<'_>,
        report: &mut Report,
    ) {
        let sql = table.sql;
        let read = sql.read;
        let name = sql.name;
        let attacker = attack.attacker;
        let victim = attack.victim();
        let seeded = attack.seeded;
        let user = attack.user();
        let victim_tenant = Uuid::from(seeded.world().tenant(victim));

        // Read every visible row as the attacker.
        let probe = format!("read as {}", attacker.label());
        match db.begin(&user).await {
            Err(error) => report.fail(name, probe, error.to_string()),
            Ok(mut tx) => match sqlx::query_scalar::<_, String>(read)
                .fetch_all(&mut *tx)
                .await
            {
                Ok(rows) => {
                    let text = rows.join("\n");
                    let leaked = seeded.leaks(&text, victim, &[]);
                    let own = seeded.rows(name, attacker);
                    let missing = own.iter().filter(|row| !shows(&text, row)).count();
                    if !leaked.is_empty() {
                        report.fail(
                            name,
                            probe,
                            format!(
                                "{} rows visible, including {}'s: {leaked:?}",
                                rows.len(),
                                victim.label()
                            ),
                        );
                    } else if sql.kind == RelationKind::View && rows.is_empty() {
                        report.fail(
                            name,
                            probe,
                            "positive control: the view shows no row, so the probe proves \
                             nothing; seed the tables it reads so it shows the attacker's rows",
                        );
                    } else if missing > 0 {
                        report.fail(
                            name,
                            probe,
                            format!(
                                "positive control: {missing} of {} own rows not visible, so the \
                                 probe proves nothing",
                                own.len()
                            ),
                        );
                    } else {
                        report.pass(
                            name,
                            probe,
                            format!(
                                "{} rows visible, all own, none of {}",
                                rows.len(),
                                victim.label()
                            ),
                        );
                    }
                }
                Err(error) => refused_or_failed(report, name, probe, &error),
            },
        }

        // Outside a tenant transaction (once per table).
        if attacker == Side::A {
            let probe = "read without a tenant";
            match sqlx::query_scalar::<_, String>(read)
                .fetch_all(db.pool())
                .await
            {
                Ok(rows) if rows.is_empty() => report.pass(name, probe, "no row visible"),
                Ok(rows) => report.fail(
                    name,
                    probe,
                    format!("{} rows visible outside a tenant transaction", rows.len()),
                ),
                Err(error) => refused_or_failed(report, name, probe, &error),
            }
        }

        if let Some(update_own) = sql.update_own {
            let probe = format!("update own rows as {}", attacker.label());
            match db.begin(&user).await {
                Err(error) => report.fail(name, probe, error.to_string()),
                Ok(mut tx) => match sqlx::query_scalar::<_, String>(update_own)
                    .fetch_all(&mut *tx)
                    .await
                {
                    Ok(rows) => {
                        let leaked = seeded.leaks(&rows.join("\n"), victim, &[]);
                        if leaked.is_empty() {
                            report.pass(
                                name,
                                probe,
                                format!("{} rows reached, none of {}", rows.len(), victim.label()),
                            );
                        } else {
                            report.fail(
                                name,
                                probe,
                                format!("reached {}'s rows: {leaked:?}", victim.label()),
                            );
                        }
                    }
                    Err(error) => refused_or_failed(report, name, probe, &error),
                },
            }
        }

        if let Some(move_to) = sql.move_to {
            let probe = format!("move own rows into {}", victim.label());
            match db.begin(&user).await {
                Err(error) => report.fail(name, probe, error.to_string()),
                Ok(mut tx) => match sqlx::query(move_to)
                    .bind(victim_tenant)
                    .execute(&mut *tx)
                    .await
                {
                    Ok(done) if done.rows_affected() == 0 => {
                        report.pass(name, probe, "no row changed");
                    }
                    Ok(done) => report.fail(
                        name,
                        probe,
                        format!(
                            "{} rows moved into {}: WITH CHECK is missing",
                            done.rows_affected(),
                            victim.label()
                        ),
                    ),
                    Err(error) => expect_refusal(report, name, probe, &error),
                },
            }
        }

        if let Some(seed) = sql.seed {
            let probe = format!("insert into {}", victim.label());
            let row = SeededRow::new(name, attacker);
            match db.begin(&user).await {
                Err(error) => report.fail(name, probe, error.to_string()),
                Ok(mut tx) => match seed_query(&sql, seed, victim_tenant, &row, seeded.world())
                    .execute(&mut *tx)
                    .await
                {
                    Ok(_) => report.fail(
                        name,
                        probe,
                        format!(
                            "inserted a row into {}: WITH CHECK is missing",
                            victim.label()
                        ),
                    ),
                    Err(error) => expect_refusal(report, name, probe, &error),
                },
            }
        }
    }
}

/// `seed` with its parameters: the tenant, the row's id and canary, and the run's user when the
/// table is [`TableSql::owned`] (Postgres refuses a parameter the statement does not have).
fn seed_query<'q>(
    sql: &TableSql,
    seed: &'static str,
    tenant: Uuid,
    row: &'q SeededRow,
    world: World,
) -> sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments> {
    let query = sqlx::query(seed)
        .bind(tenant)
        .bind(row.id)
        .bind(&row.canary);
    if sql.owned {
        query.bind(Uuid::from(world.user()))
    } else {
        query
    }
}

/// The SQLSTATE of a database error.
fn sqlstate(error: &sqlx::Error) -> Option<String> {
    error
        .as_database_error()
        .and_then(|error| error.code())
        .map(|code| code.into_owned())
}

/// A refusal (`42501`) passes; any other error fails.
fn refused_or_failed(
    report: &mut Report,
    name: &str,
    probe: impl Into<String>,
    error: &sqlx::Error,
) {
    if sqlstate(error).as_deref() == Some(INSUFFICIENT_PRIVILEGE) {
        report.pass(name, probe, "refused (42501)");
    } else {
        report.fail(name, probe, format!("failed: {error}"));
    }
}

/// A write into the other tenant must be refused by row-level security (`42501`); any other
/// error leaves open whether it would have been, so it fails.
fn expect_refusal(report: &mut Report, name: &str, probe: impl Into<String>, error: &sqlx::Error) {
    match sqlstate(error) {
        Some(code) if code == INSUFFICIENT_PRIVILEGE => {
            report.pass(name, probe, "refused (42501)");
        }
        code => report.fail(
            name,
            probe,
            format!(
                "expected a row-level security refusal (42501), got {}: {error}",
                code.as_deref().unwrap_or("no SQLSTATE")
            ),
        ),
    }
}

/// `schema.name` for a declared name; an unqualified name is in `public`.
fn qualified(name: &str) -> String {
    if name.contains('.') {
        name.to_owned()
    } else {
        format!("public.{name}")
    }
}

/// Whether a declared name is `name` or `schema.name` of lower-case unquoted identifiers.
fn valid_name(name: &str) -> bool {
    let identifier = |part: &str| {
        let mut chars = part.chars();
        chars
            .next()
            .is_some_and(|first| first.is_ascii_lowercase() || first == '_')
            && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '$')
    };
    let parts: Vec<&str> = name.split('.').collect();
    (1..=2).contains(&parts.len()) && parts.into_iter().all(identifier)
}

/// What the suite declares about a relation.
#[derive(Clone, Copy, Debug)]
enum Declared {
    Table(RelationKind),
    Exempt(&'static str),
}

/// A relation the service role can reach.
#[derive(Clone, Debug)]
struct Reachable {
    kind: String,
    has_tenant_id: bool,
}

fn kind_name(kind: &str) -> &'static str {
    match kind {
        "r" => "table",
        "p" => "partitioned table",
        "v" => "view",
        "m" => "materialized view",
        "f" => "foreign table",
        _ => "relation",
    }
}

/// Compares the reachable relations with the declarations.
fn check_declarations(
    reachable: &BTreeMap<String, Reachable>,
    tables: &[PgTable],
    exempt: &[(&'static str, &'static str)],
    report: &mut Report,
) {
    let mut declared: BTreeMap<String, Declared> = BTreeMap::new();
    let entries = tables
        .iter()
        .map(|table| (table.sql.name, Declared::Table(table.sql.kind)))
        .chain(
            exempt
                .iter()
                .map(|(name, reason)| (*name, Declared::Exempt(reason))),
        );
    for (name, declaration) in entries {
        if !valid_name(name) {
            report.fail(
                name,
                "coverage",
                "not `table` or `schema.table` of lower-case unquoted identifiers",
            );
        } else if declared.insert(qualified(name), declaration).is_some() {
            report.fail(name, "coverage", "declared more than once");
        }
    }
    for (name, relation) in reachable {
        let kind = kind_name(&relation.kind);
        let Some(declaration) = declared.get(name) else {
            report.fail(
                name,
                "coverage",
                format!(
                    "the service role can use this {kind} but the suite does not declare it: add \
                     pg_table!/pg_view!, or exempt() it with the reason it holds no tenant data"
                ),
            );
            continue;
        };
        match *declaration {
            Declared::Table(RelationKind::Table)
                if !matches!(relation.kind.as_str(), "r" | "p") =>
            {
                report.fail(
                    name,
                    "coverage",
                    format!("declared with pg_table! but it is a {kind}"),
                );
            }
            Declared::Table(RelationKind::Table) if !relation.has_tenant_id => report.fail(
                name,
                "coverage",
                "declared as a tenant table but has no tenant_id column (sql/tenant-table.sql)",
            ),
            Declared::Table(RelationKind::View) if relation.kind != "v" => {
                report.fail(
                    name,
                    "coverage",
                    format!("declared with pg_view! but it is a {kind}"),
                );
            }
            Declared::Table(_) => report.pass(name, "coverage", format!("declared {kind}")),
            Declared::Exempt(_) if relation.has_tenant_id => report.fail(
                name,
                "coverage",
                "exempt, but its tenant_id column says it holds tenant data: declare it",
            ),
            Declared::Exempt(reason) => {
                report.pass(name, "coverage", format!("exempt {kind}: {reason}"));
            }
        }
    }
    for name in declared.keys() {
        if !reachable.contains_key(name) {
            report.fail(
                name,
                "coverage",
                "declared, but the service role cannot use it (missing, misspelled, or not \
                 granted)",
            );
        }
    }
}

impl Store for PgStore {
    async fn check_coverage(&self, report: &mut Report) {
        let role = self.service.get_username().to_owned();
        let Some(mut seeder) = self.seeder(report).await else {
            return;
        };
        let rows = sqlx::query_as::<_, (String, String, bool)>(REACHABLE_RELATIONS)
            .bind(&role)
            .fetch_all(&mut seeder)
            .await;
        let _ = seeder.close().await;
        match rows {
            Ok(rows) => {
                let reachable = rows
                    .into_iter()
                    .map(|(name, kind, has_tenant_id)| {
                        (
                            name,
                            Reachable {
                                kind,
                                has_tenant_id,
                            },
                        )
                    })
                    .collect();
                check_declarations(&reachable, &self.tables, &self.exempt, report);
            }
            Err(error) => report.fail("postgres", "coverage", error.to_string()),
        }
    }

    async fn seed(
        &self,
        world: World,
        rows_per_tenant: usize,
        report: &mut Report,
    ) -> Vec<SeededRow> {
        let Some(mut seeder) = self.seeder(report).await else {
            return Vec::new();
        };
        let mut seeded = Vec::new();
        for table in &self.tables {
            let sql = table.sql;
            let read = sql.read;
            let Some(seed) = sql.seed else { continue };
            let mut rows = Vec::new();
            let count = if sql.one_per_tenant {
                1
            } else {
                rows_per_tenant
            };
            for side in [Side::A, Side::B] {
                for _ in 0..count {
                    let mut row = SeededRow::new(sql.name, side);
                    if sql.one_per_tenant {
                        row.id = Uuid::from(world.tenant(side));
                    }
                    match seed_query(&sql, seed, Uuid::from(world.tenant(side)), &row, world)
                        .execute(&mut seeder)
                        .await
                    {
                        Ok(_) => rows.push(row),
                        Err(error) => report.fail(
                            sql.name,
                            "seed",
                            format!("seeding a row of {} failed: {error}", side.label()),
                        ),
                    }
                }
            }
            match sqlx::query_scalar::<_, String>(read)
                .fetch_all(&mut seeder)
                .await
            {
                Ok(stored) => {
                    let text = stored.join("\n");
                    let unseen = rows.iter().filter(|row| !shows(&text, row)).count();
                    if unseen > 0 {
                        report.fail(
                            sql.name,
                            "seed",
                            format!(
                                "{unseen} seeded rows do not show their id ($2) or canary ($3); \
                                 the seed must store one of them"
                            ),
                        );
                    } else {
                        let what = if sql.one_per_tenant {
                            "1 row per tenant (one_per_tenant)".to_owned()
                        } else {
                            format!("{count} rows per tenant")
                        };
                        report.pass(sql.name, "seed", what);
                    }
                }
                Err(error) => {
                    report.fail(sql.name, "seed", format!("reading back failed: {error}"))
                }
            }
            seeded.extend(rows);
        }
        let _ = seeder.close().await;
        seeded
    }

    async fn attack(&self, attack: &Attack<'_>, report: &mut Report) {
        let pool = PgPoolOptions::new()
            .max_connections(2)
            .acquire_timeout(ACQUIRE_TIMEOUT);
        let db = match TenantDb::connect_lazy(self.service.clone(), pool) {
            Ok(db) => db,
            Err(error) => {
                report.fail("postgres", "service role pool", error.to_string());
                return;
            }
        };
        let probe = format!("session as {}", attack.attacker.label());
        match db.ping().await {
            Ok(()) => report.pass(
                "postgres",
                probe,
                "the kit accepts the service role's session",
            ),
            Err(error) => {
                report.fail("postgres", probe, error.to_string());
                db.pool().close().await;
                return;
            }
        }
        for table in &self.tables {
            self.attack_table(&db, table, attack, report).await;
        }
        db.pool().close().await;
    }

    async fn snapshot(&self, seeded: &Seeded, side: Side, report: &mut Report) -> Vec<String> {
        let Some(mut seeder) = self.seeder(report).await else {
            return Vec::new();
        };
        let mut snapshot = Vec::new();
        for table in self
            .tables
            .iter()
            .filter(|table| table.sql.kind == RelationKind::Table)
        {
            let read = table.sql.read;
            match sqlx::query_scalar::<_, String>(read)
                .fetch_all(&mut seeder)
                .await
            {
                Ok(rows) => snapshot.extend(
                    rows.into_iter()
                        .filter(|row| !seeded.leaks(row, side, &[]).is_empty())
                        .map(|row| format!("{}: {row}", table.sql.name)),
                ),
                Err(error) => report.fail(table.sql.name, "snapshot", error.to_string()),
            }
        }
        let _ = seeder.close().await;
        snapshot.sort();
        snapshot
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reachable(entries: &[(&str, &str, bool)]) -> BTreeMap<String, Reachable> {
        entries
            .iter()
            .map(|(name, kind, has_tenant_id)| {
                (
                    (*name).to_owned(),
                    Reachable {
                        kind: (*kind).to_owned(),
                        has_tenant_id: *has_tenant_id,
                    },
                )
            })
            .collect()
    }

    const SEED: &str = "INSERT INTO items (tenant_id, id, name) VALUES ($1, $2, $3)";

    #[test]
    fn the_macros_build_static_sql_from_the_name() {
        let table = crate::pg_table!("app.items", seed = SEED);
        assert_eq!(table.name(), "app.items");
        assert_eq!(
            table.sql.read,
            "SELECT testkit_row::text FROM app.items AS testkit_row"
        );
        assert_eq!(
            table.sql.move_to,
            Some("UPDATE app.items SET tenant_id = $1")
        );
        assert_eq!(table.sql.seed, Some(SEED));
        assert!(!table.sql.one_per_tenant);
        let view = crate::pg_view!("item_names");
        assert_eq!(view.sql.kind, RelationKind::View);
        assert!(view.sql.update_own.is_none() && view.sql.seed.is_none());
        assert!(!table.sql.owned && !view.sql.owned);
        let single = crate::pg_table!("app.settings", seed = SEED, one_per_tenant);
        assert!(single.sql.one_per_tenant && !single.sql.owned);
        let owned = crate::pg_table!("app.notes", seed = SEED, owned);
        assert!(owned.sql.owned && !owned.sql.one_per_tenant);
        assert_eq!(owned.sql.seed, Some(SEED));
        assert_eq!(
            single.sql.read,
            "SELECT testkit_row::text FROM app.settings AS testkit_row"
        );
    }

    #[test]
    fn declared_relations_pass() {
        let mut report = Report::default();
        check_declarations(
            &reachable(&[
                ("public.items", "r", true),
                ("public.item_names", "v", false),
                ("public.units", "r", false),
            ]),
            &[
                crate::pg_table!("items", seed = SEED),
                crate::pg_view!("item_names"),
            ],
            &[("public.units", "a shared catalogue")],
            &mut report,
        );
        assert_eq!(report.failures(), Vec::<String>::new());
    }

    #[test]
    fn a_forgotten_relation_fails() {
        let mut report = Report::default();
        check_declarations(
            &reachable(&[("public.items", "r", true), ("public.notes", "r", true)]),
            &[crate::pg_table!("items", seed = SEED)],
            &[],
            &mut report,
        );
        let failures = report.failures();
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert!(failures[0].starts_with("public.notes | coverage | the service role can use"));
    }

    #[test]
    fn wrong_declarations_fail() {
        let mut report = Report::default();
        check_declarations(
            &reachable(&[
                ("public.items", "r", true),
                ("public.no_tenant", "r", false),
                ("public.summary", "m", true),
                ("public.listing", "v", false),
            ]),
            &[
                crate::pg_table!("no_tenant", seed = SEED),
                crate::pg_table!("listing", seed = SEED),
                crate::pg_table!("gone", seed = SEED),
                crate::pg_table!("Items", seed = SEED),
            ],
            &[
                ("items", "says it has no tenant data"),
                ("public.summary", "same"),
            ],
            &mut report,
        );
        let failures = report.failures().join("\n");
        for expected in [
            "public.no_tenant | coverage | declared as a tenant table but has no tenant_id",
            "public.listing | coverage | declared with pg_table! but it is a view",
            "public.gone | coverage | declared, but the service role cannot use it",
            "Items | coverage | not `table` or `schema.table`",
            "public.items | coverage | exempt, but its tenant_id column",
            "public.summary | coverage | exempt, but its tenant_id column",
        ] {
            assert!(
                failures.contains(expected),
                "missing {expected:?} in\n{failures}"
            );
        }
    }

    #[test]
    fn a_repeated_declaration_fails() {
        let mut report = Report::default();
        check_declarations(
            &reachable(&[("public.items", "r", true)]),
            &[crate::pg_table!("items", seed = SEED)],
            &[("public.items", "again")],
            &mut report,
        );
        assert!(
            report
                .failures()
                .iter()
                .any(|f| f.contains("declared more than once"))
        );
    }

    #[test]
    fn names_are_validated_and_qualified() {
        assert!(valid_name("items") && valid_name("app.items_2") && valid_name("_x"));
        assert!(!valid_name("Items") && !valid_name("a.b.c") && !valid_name("x; drop"));
        assert!(!valid_name("\"items\"") && !valid_name("") && !valid_name("1x"));
        assert_eq!(qualified("items"), "public.items");
        assert_eq!(qualified("app.items"), "app.items");
    }
}
