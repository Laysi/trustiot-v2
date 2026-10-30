//! Static checks of the ClickHouse migration files, which need no server, and the upgrade of
//! `0012_gateway_stream_ids.sql` against real ClickHouse servers.

mod common;

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use common::{Account, ClickHouse, Migration, migrations, migrations_dir, split_statements};

fn words(statement: &str) -> Vec<String> {
    statement
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .filter(|word| !word.is_empty())
        .map(str::to_ascii_uppercase)
        .collect()
}

#[test]
fn files_are_numbered_in_apply_order() {
    let names: Vec<String> = migrations().into_iter().map(|m| m.name).collect();
    assert!(
        !names.is_empty(),
        "no files in {}",
        migrations_dir().display()
    );
    for (index, name) in names.iter().enumerate() {
        let expected_prefix = format!("{:04}_", index + 1);
        let stem = name
            .strip_prefix(&expected_prefix)
            .and_then(|rest| rest.strip_suffix(".sql"));
        assert!(
            stem.is_some_and(|stem| !stem.is_empty()
                && stem
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')),
            "{name}: migration files are named NNNN_lower_snake.sql, numbered from 0001 without gaps"
        );
    }
}

#[test]
fn every_file_has_statements() {
    for migration in migrations() {
        assert!(
            !migration.statements.is_empty(),
            "{} has no statements",
            migration.name
        );
    }
}

/// The statement with every string literal replaced by `''`, so the words of a comment or a
/// constraint pattern are never read as SQL keywords.
fn without_literals(statement: &str) -> String {
    let mut out = String::with_capacity(statement.len());
    let mut chars = statement.chars();
    while let Some(c) = chars.next() {
        if c != '\'' {
            out.push(c);
            continue;
        }
        out.push_str("''");
        while let Some(inner) = chars.next() {
            match inner {
                '\\' => {
                    chars.next();
                }
                '\'' => {
                    // '' inside a literal is an escaped quote, not its end.
                    if chars.clone().next() == Some('\'') {
                        chars.next();
                    } else {
                        break;
                    }
                }
                _ => {}
            }
        }
    }
    out
}

/// The statement's tokens outside string literals: names (dotted names stay one token), keywords
/// and operators, split at white space, parentheses and commas, with `=` a token of its own.
fn tokens(statement: &str) -> Vec<String> {
    without_literals(statement)
        .replace(['(', ')', ','], " ")
        .replace('=', " = ")
        .split_whitespace()
        .map(str::to_owned)
        .collect()
}

fn upper(tokens: &[String]) -> Vec<String> {
    tokens
        .iter()
        .map(|token| token.to_ascii_uppercase())
        .collect()
}

fn starts_with(tokens: &[String], prefix: &[&str]) -> bool {
    tokens.len() >= prefix.len() && tokens.iter().zip(prefix).all(|(token, word)| token == word)
}

/// Words that end, rename or detach a relation or an access entity. Refused in every statement,
/// outside string literals.
const NEVER: [&str; 5] = ["DROP", "TRUNCATE", "DETACH", "EXCHANGE", "RENAME"];

/// The `CREATE ... IF NOT EXISTS` forms the files use.
const CREATE_FORMS: [&[&str]; 9] = [
    &["CREATE", "DATABASE", "IF", "NOT", "EXISTS"],
    &["CREATE", "TABLE", "IF", "NOT", "EXISTS"],
    &["CREATE", "VIEW", "IF", "NOT", "EXISTS"],
    &["CREATE", "MATERIALIZED", "VIEW", "IF", "NOT", "EXISTS"],
    &["CREATE", "ROLE", "IF", "NOT", "EXISTS"],
    &["CREATE", "USER", "IF", "NOT", "EXISTS"],
    &["CREATE", "QUOTA", "IF", "NOT", "EXISTS"],
    &["CREATE", "SETTINGS", "PROFILE", "IF", "NOT", "EXISTS"],
    &["CREATE", "ROW", "POLICY", "IF", "NOT", "EXISTS"],
];

/// Access statements: they change who may do what, never the data. `ALTER ROW POLICY` changes
/// which rows a policy shows (0011_queryable_period.sql adds the queryable period to the tenant
/// policies); renaming one is refused with the other `NEVER` words.
const ACCESS_FORMS: [&[&str]; 5] = [
    &["GRANT"],
    &["REVOKE"],
    &["ALTER", "USER"],
    &["ALTER", "ROW", "POLICY"],
    &["SET", "DEFAULT", "ROLE"],
];

/// The one command an `ALTER TABLE <table>` statement may carry. Each is safe to repeat and keeps
/// every row.
const ALTER_TABLE_COMMANDS: [&[&str]; 6] = [
    &["ADD", "COLUMN", "IF", "NOT", "EXISTS"],
    &["ADD", "CONSTRAINT", "IF", "NOT", "EXISTS"],
    &["MODIFY", "COLUMN"],
    &["COMMENT", "COLUMN"],
    &["MODIFY", "COMMENT"],
    &["MODIFY", "TTL"],
];

/// Words that would start another `ALTER TABLE` command (a second command after a comma) or give
/// a column a TTL or remove one of its properties. Refused after the allowed command.
const ALTER_TABLE_AFTER_COMMAND: [&str; 17] = [
    "ADD",
    "MODIFY",
    "DELETE",
    "UPDATE",
    "CLEAR",
    "MOVE",
    "REPLACE",
    "PARTITION",
    "FREEZE",
    "UNFREEZE",
    "ATTACH",
    "FETCH",
    "MATERIALIZE",
    "APPLY",
    "REMOVE",
    "TTL",
    "FINAL",
];

/// Column type changes that keep every stored value, each reviewed: (table, column, new type,
/// why nothing is lost). `MODIFY COLUMN` with a type is allowed only for these; without a type
/// (only a comment, codec or default) it never rewrites a value.
const LOSSLESS_TYPE_CHANGES: [(&str, &str, &str, &str); 2] = [
    (
        "trustiot.telemetry",
        "gateway_id",
        "String",
        "UUID to String writes each UUID as its text form (0012_gateway_stream_ids.sql, checked \
         by id_migration_gives_one_schema_on_fresh_and_upgraded_servers)",
    ),
    (
        "trustiot.telemetry",
        "stream_id",
        "String",
        "UUID to String writes each UUID as its text form (0012_gateway_stream_ids.sql, checked \
         by id_migration_gives_one_schema_on_fresh_and_upgraded_servers)",
    ),
];

/// Words that may follow the column name of a `MODIFY COLUMN` that changes no type.
const COLUMN_PROPERTIES: [&str; 3] = ["COMMENT", "CODEC", "DEFAULT"];

/// A `MODIFY COLUMN` of `table` either changes no type or makes a change listed in
/// `LOSSLESS_TYPE_CHANGES`. `rest` is the statement's tokens after `MODIFY COLUMN`.
fn check_column_change(file: &str, statement: &str, table: &str, rest: &[String]) {
    let column = rest.first().map_or("", String::as_str);
    let Some(next) = rest.get(1) else {
        return;
    };
    if COLUMN_PROPERTIES.contains(&next.to_ascii_uppercase().as_str()) {
        return;
    }
    assert!(
        LOSSLESS_TYPE_CHANGES
            .iter()
            .any(|(t, c, ty, _)| *t == table && *c == column && *ty == next.as_str()),
        "{file}: MODIFY COLUMN changes the type of {table}.{column} to {next}, which is not in \
         LOSSLESS_TYPE_CHANGES. A type change can rewrite or cut stored values; add it there only \
         with the reason no value is lost, and a test that reads an old value back:\n{statement}"
    );
}

/// A view as the first file that created it wrote it: the tokens between `CREATE VIEW IF NOT
/// EXISTS` and `AS SELECT`, which hold the name, `DEFINER = <user>` and `SQL SECURITY DEFINER`.
fn view_header(tokens: &[String], skip: usize) -> Option<Vec<String>> {
    let upper = upper(tokens);
    let end = upper
        .windows(2)
        .position(|pair| pair[0] == "AS" && pair[1] == "SELECT")?;
    (end > skip).then(|| tokens[skip..end].to_vec())
}

/// Accepts the statement forms the migration files may use and refuses everything else, naming
/// the file and the statement. `views` maps each plain view created with `CREATE VIEW IF NOT
/// EXISTS` in an earlier file to its header (`view_header`).
fn check_statement_form(file: &str, statement: &str, views: &BTreeMap<String, Vec<String>>) {
    let tokens = tokens(statement);
    let words = upper(&tokens);
    for never in NEVER {
        assert!(
            !words.iter().any(|word| word == never),
            "{file}: {never} is not allowed in migrations (it destroys data, or drops or renames \
             what grants point at):\n{statement}"
        );
    }

    // The one replacement: a plain view an earlier file created with CREATE VIEW IF NOT EXISTS,
    // replaced with the same name, DEFINER and SQL SECURITY, so only its columns can change. A
    // plain view holds no data and ClickHouse keeps grants and row policies by its name; 26.8 has
    // no other way to change a plain view's columns (MODIFY QUERY is refused for storage View,
    // see 0012_gateway_stream_ids.sql).
    if starts_with(&words, &["CREATE", "OR", "REPLACE", "VIEW"]) {
        let header = view_header(&tokens, 4).unwrap_or_else(|| {
            panic!("{file}: CREATE OR REPLACE VIEW without AS SELECT:\n{statement}")
        });
        let original = header.first().and_then(|name| views.get(name));
        assert_eq!(
            Some(&header),
            original,
            "{file}: CREATE OR REPLACE VIEW may only replace a plain view that an earlier file \
             created with CREATE VIEW IF NOT EXISTS, with the same name, DEFINER and SQL SECURITY \
             word for word before AS SELECT:\n{statement}"
        );
        return;
    }
    if CREATE_FORMS.iter().any(|form| starts_with(&words, form)) {
        return;
    }
    if ACCESS_FORMS.iter().any(|form| starts_with(&words, form)) {
        return;
    }
    if starts_with(&words, &["ALTER", "TABLE"]) && words.len() > 3 {
        let command = &words[3..];
        if let Some(form) = ALTER_TABLE_COMMANDS
            .iter()
            .find(|form| starts_with(command, form))
        {
            let rest = &command[form.len()..];
            let extra = rest
                .iter()
                .find(|word| ALTER_TABLE_AFTER_COMMAND.contains(&word.as_str()));
            assert!(
                extra.is_none(),
                "{file}: ALTER TABLE may carry only one command, {}, without {} after it:\n\
                 {statement}",
                form.join(" "),
                extra.map_or("", String::as_str)
            );
            if *form == ["MODIFY", "COLUMN"] {
                check_column_change(file, statement, &tokens[2], &tokens[3 + form.len()..]);
            }
            return;
        }
    }
    panic!(
        "{file}: statement form not allowed in migrations. Allowed: CREATE DATABASE, TABLE, VIEW, \
         MATERIALIZED VIEW, ROLE, USER, QUOTA, SETTINGS PROFILE or ROW POLICY with IF NOT EXISTS; \
         ALTER TABLE with one of ADD COLUMN IF NOT EXISTS, ADD CONSTRAINT IF NOT EXISTS, MODIFY \
         COLUMN, COMMENT COLUMN, MODIFY COMMENT, MODIFY TTL; GRANT, REVOKE, ALTER USER, ALTER ROW \
         POLICY, SET DEFAULT ROLE; and CREATE OR REPLACE VIEW of a plain view an earlier file \
         created (services/ingest/README.md, file format):\n{statement}"
    );
}

/// The migration Job may apply a file again (for example after a failed deploy), so every
/// statement must be safe to repeat, and none may destroy data: only the statement forms that
/// `check_statement_form` lists are allowed.
#[test]
fn statements_are_idempotent_and_never_destroy_data() {
    let mut views = BTreeMap::new();
    for migration in migrations() {
        for statement in &migration.statements {
            check_statement_form(&migration.name, statement, &views);
        }
        // Views count as created only once their file is done: a replacement must come from a
        // later file than the CREATE VIEW IF NOT EXISTS it replaces.
        for statement in &migration.statements {
            let tokens = tokens(statement);
            if starts_with(&upper(&tokens), &["CREATE", "VIEW", "IF", "NOT", "EXISTS"]) {
                let header = view_header(&tokens, 5).unwrap_or_else(|| {
                    panic!("{}: a view without AS SELECT:\n{statement}", migration.name)
                });
                if let Some(name) = header.first().cloned() {
                    views.entry(name).or_insert(header);
                }
            }
        }
    }
}

/// Only MergeTree-family engines, so a move to ClickHouse Cloud (SharedMergeTree) stays possible
/// (decision 14).
#[test]
fn tables_use_only_mergetree_engines() {
    let mut tables = 0;
    for migration in migrations() {
        for statement in &migration.statements {
            let words = words(statement);
            if !(words.first().map(String::as_str) == Some("CREATE")
                && words.get(1).map(String::as_str) == Some("TABLE"))
            {
                continue;
            }
            tables += 1;
            let engine = words
                .iter()
                .position(|word| word == "ENGINE")
                .and_then(|at| words.get(at + 1))
                .unwrap_or_else(|| panic!("{}: CREATE TABLE without ENGINE", migration.name));
            assert!(
                engine.ends_with("MERGETREE"),
                "{}: engine {engine} is not in the MergeTree family",
                migration.name
            );
        }
    }
    assert!(
        tables >= 3,
        "expected the telemetry, batch_commits and stream_commits tables"
    );
}

/// Every view runs as an explicit definer (SQL SECURITY DEFINER), so the accounts that read it
/// need no grant on the tables behind it (Laysi/trustiot-v2#260). The server default for a
/// plain view is SQL SECURITY INVOKER, which would need such a grant.
#[test]
fn views_run_as_their_definer() {
    let mut views = 0;
    for migration in migrations() {
        for statement in &migration.statements {
            let text = words(statement).join(" ");
            if !(text.starts_with("CREATE VIEW ")
                || text.starts_with("CREATE MATERIALIZED VIEW ")
                || text.starts_with("CREATE OR REPLACE VIEW "))
            {
                continue;
            }
            views += 1;
            // DEFINER twice: once for `DEFINER = <user>`, once for `SQL SECURITY DEFINER`.
            assert!(
                text.matches(" DEFINER ").count() >= 2 && text.contains(" SQL SECURITY DEFINER "),
                "{}: a view needs DEFINER = <user> SQL SECURITY DEFINER:\n{statement}",
                migration.name
            );
        }
    }
    assert!(views >= 1, "expected the tenant view");
}

#[test]
fn splitter_rejects_ambiguous_files() {
    assert_eq!(
        split_statements("-- a; comment;\nSELECT 1;\n\nSELECT\n    2;\n").unwrap(),
        vec!["SELECT 1".to_owned(), "SELECT\n    2".to_owned()]
    );
    assert!(split_statements("SELECT 1; SELECT 2;\n").is_err());
    assert!(split_statements("SELECT 1\n").is_err());
    assert!(split_statements("SELECT 1;\n;\n").is_err());
}

/// The file that changes the gateway and stream IDs to text and adds the completion table keyed
/// by gateway plus stream (Laysi/trustiot-v2#639).
const ID_MIGRATION: &str = "0012_gateway_stream_ids.sql";

/// The files after the ID migration, which the upgrade test applies after it on both servers.
/// Each must give the same schema on both and change nothing when applied again; a new file is
/// added here once that holds for it.
const LATER_MIGRATIONS: [&str; 1] = ["0013_tenant_id_comment.sql"];

/// What the ID migration defines, as text: the trustiot relations with their DDL and columns, and
/// the access entities of the product (names that start with trustiot_, without the bootstrap
/// admin). Test accounts are left out, so a server with test data compares equal to one without.
const SCHEMA: [&str; 5] = [
    "SELECT name, engine, create_table_query, comment FROM system.tables
     WHERE database = 'trustiot' ORDER BY name FORMAT TSV",
    "SELECT table, position, name, type, comment FROM system.columns
     WHERE database = 'trustiot' ORDER BY table, position FORMAT TSV",
    "SELECT coalesce(user_name, role_name) AS grantee, toString(access_type), database, table,
            column, is_partial_revoke, grant_option
     FROM system.grants WHERE startsWith(grantee, 'trustiot_') AND grantee != currentUser()
     ORDER BY ALL FORMAT TSV",
    "SELECT coalesce(user_name, role_name) AS grantee, granted_role_name, with_admin_option
     FROM system.role_grants WHERE startsWith(grantee, 'trustiot_') AND grantee != currentUser()
     ORDER BY ALL FORMAT TSV",
    "SELECT short_name, database, table, select_filter, is_restrictive, apply_to_all,
            apply_to_list, apply_to_except
     FROM system.row_policies ORDER BY ALL FORMAT TSV",
];

async fn schema(ch: &ClickHouse) -> Vec<String> {
    let mut out = Vec::new();
    for query in SCHEMA {
        out.push(ch.admin_ok(query).await);
    }
    out
}

/// Waits until ClickHouse has finished every mutation of `trustiot.telemetry`, such as the one
/// that rewrites existing rows after `MODIFY COLUMN`.
async fn wait_for_telemetry_mutations(ch: &ClickHouse) {
    let started = Instant::now();
    loop {
        let pending = ch
            .admin_ok(
                "SELECT count() FROM system.mutations \
                 WHERE database = 'trustiot' AND table = 'telemetry' AND NOT is_done",
            )
            .await;
        if pending.trim() == "0" {
            return;
        }
        assert!(
            started.elapsed() < Duration::from_secs(300),
            "the mutations of trustiot.telemetry did not finish within 300 s: {}",
            ch.admin_ok(
                "SELECT mutation_id, command, latest_fail_reason FROM system.mutations \
                 WHERE database = 'trustiot' AND table = 'telemetry' AND NOT is_done FORMAT TSV"
            )
            .await
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

fn split_at_id_migration() -> (Vec<Migration>, Migration, Vec<Migration>) {
    let mut all = migrations();
    let at = all
        .iter()
        .position(|migration| migration.name == ID_MIGRATION)
        .unwrap_or_else(|| panic!("{ID_MIGRATION} is missing"));
    let mut after = all.split_off(at);
    let id_migration = after.remove(0);
    // The upgrade case is a server at 0011: it gets the ID migration and every later file, and
    // each later file is listed, so a new one is looked at before it joins the comparison.
    let later: Vec<&str> = after.iter().map(|m| m.name.as_str()).collect();
    assert_eq!(
        later, LATER_MIGRATIONS,
        "files after {ID_MIGRATION}: check that each gives one schema on both servers and \
         changes nothing applied again, then list it in LATER_MIGRATIONS"
    );
    (all, id_migration, after)
}

/// Laysi/trustiot-v2#639: the ID migration gives the same schema on a fresh server and on a server
/// that already had 0001 to 0011 and data in the old shape (readings with UUID gateway and stream
/// IDs and an old completion record), and applying it a second time changes nothing on either.
/// The reading written before the change stays readable, with its UUIDs as text, also through the
/// tenant view, and the view it rebuilds keeps the queryable period of 0011_queryable_period.sql:
/// the row policies are bound to the view's name, so an older reading stays hidden.
///
/// Always two containers of the test's own, one after the other: the comparison needs servers on
/// which nothing was applied before.
#[tokio::test(flavor = "current_thread")]
async fn id_migration_gives_one_schema_on_fresh_and_upgraded_servers() {
    let (before, id_migration, later) = split_at_id_migration();

    // A fresh server: every file, then the ID migration once more.
    let Some(fresh) = ClickHouse::start_throwaway().await else {
        return;
    };
    fresh.apply_migrations().await;
    let fresh_schema = schema(&fresh).await;
    fresh.apply(&id_migration).await;
    for migration in &later {
        fresh.apply(migration).await;
    }
    assert_eq!(
        schema(&fresh).await,
        fresh_schema,
        "applying {ID_MIGRATION} and the later files again changed a fresh server"
    );
    drop(fresh);

    // A server at 0011 with data written in the old shape.
    let Some(upgraded) = ClickHouse::start_throwaway().await else {
        return;
    };
    for migration in &before {
        upgraded.apply(migration).await;
    }
    let password = uuid::Uuid::new_v4().simple().to_string();
    // CREATE USER ... DEFAULT ROLE grants the role, as the migration Job creates accounts.
    let ingest = Account {
        user: "upgrade_ingest".to_owned(),
        password: password.clone(),
    };
    upgraded
        .admin_ok(&format!(
            "CREATE USER {} IDENTIFIED WITH sha256_password BY '{password}' \
             DEFAULT ROLE trustiot_ingest",
            ingest.user
        ))
        .await;
    let tenant = uuid::Uuid::new_v4();
    let tenant_account = Account {
        user: "upgrade_tenant".to_owned(),
        password: password.clone(),
    };
    // The windows the telemetry-query service gives every account (0011_queryable_period.sql):
    // raw readings of the last 7 days.
    upgraded
        .admin_ok(&format!(
            "CREATE USER {} IDENTIFIED WITH sha256_password BY '{password}' \
             SETTINGS PROFILE 'trustiot_tenant_reader', SQL_tenant_id = '{tenant}' CONST, \
             SQL_queryable_raw_days = 7 CONST, SQL_queryable_1m_months = 3 CONST, \
             SQL_queryable_1h_months = 3 CONST",
            tenant_account.user
        ))
        .await;
    upgraded
        .admin_ok("GRANT trustiot_tenant_reader TO upgrade_tenant")
        .await;
    let (gateway, stream) = (uuid::Uuid::new_v4(), uuid::Uuid::new_v4());
    for sql in [
        format!(
            "INSERT INTO trustiot.telemetry (tenant_id, gateway_id, stream_id, batch_seq, \
             device_id, point, ts, received_at, value_type, value_int) VALUES \
             ('{tenant}', '{gateway}', '{stream}', 1, generateUUIDv4(), 'legacy', \
             now64(3, 'UTC'), now64(3, 'UTC'), 'int', 42)"
        ),
        // Read 30 days ago, received now: kept by the TTL, outside the 7-day window.
        format!(
            "INSERT INTO trustiot.telemetry (tenant_id, gateway_id, stream_id, batch_seq, \
             device_id, point, ts, received_at, value_type, value_int) VALUES \
             ('{tenant}', '{gateway}', '{stream}', 2, generateUUIDv4(), 'legacy_old', \
             now64(3, 'UTC') - toIntervalDay(30), now64(3, 'UTC'), 'int', 7)"
        ),
        format!(
            "INSERT INTO trustiot.batch_commits (tenant_id, gateway_id, stream_id, seq, \
             row_count, committed_at) VALUES ('{tenant}', '{gateway}', '{stream}', 1, 1, \
             now64(3, 'UTC'))"
        ),
    ] {
        upgraded
            .query(&ingest, &sql)
            .await
            .unwrap_or_else(|error| panic!("{sql}: {}", error.body));
    }

    // Before the ID migration, the view of 0005 with the window of 0011 shows only the reading
    // inside the window.
    let before_upgrade = upgraded
        .query(
            &tenant_account,
            "SELECT point, value_int FROM trustiot.tenant_telemetry ORDER BY point FORMAT TSV",
        )
        .await
        .unwrap_or_else(|error| panic!("read through the tenant view: {}", error.body));
    assert_eq!(
        before_upgrade, "legacy\t42\n",
        "the window before {ID_MIGRATION}"
    );

    for migration in std::iter::once(&id_migration).chain(&later) {
        upgraded.apply(migration).await;
    }
    let upgraded_schema = schema(&upgraded).await;
    for migration in std::iter::once(&id_migration).chain(&later) {
        upgraded.apply(migration).await;
    }
    assert_eq!(
        schema(&upgraded).await,
        upgraded_schema,
        "applying {ID_MIGRATION} and the later files again changed an upgraded server"
    );
    assert_eq!(
        upgraded_schema, fresh_schema,
        "{ID_MIGRATION} gives a different schema on a server that had 0001 to 0011"
    );
    #[allow(clippy::print_stdout, reason = "test evidence")]
    {
        println!(
            "schema after {ID_MIGRATION}, the same on both servers:\n{}",
            upgraded_schema.join("\n")
        );
    }

    // The reading written with UUID IDs: the rewrite of the existing part finishes, and the IDs
    // read back as their text form, from the table and through the tenant view.
    wait_for_telemetry_mutations(&upgraded).await;
    let row = upgraded
        .query(
            &ingest,
            "SELECT gateway_id, stream_id, toTypeName(gateway_id), toTypeName(stream_id), \
             value_int FROM trustiot.telemetry WHERE point = 'legacy' FORMAT TSV",
        )
        .await
        .unwrap_or_else(|error| panic!("read the legacy reading: {}", error.body));
    assert_eq!(row, format!("{gateway}\t{stream}\tString\tString\t42\n"));
    let old = upgraded
        .query(
            &ingest,
            "SELECT gateway_id, value_int FROM trustiot.telemetry WHERE point = 'legacy_old' \
             FORMAT TSV",
        )
        .await
        .unwrap_or_else(|error| panic!("read the older reading: {}", error.body));
    assert_eq!(old, format!("{gateway}\t7\n"), "the older reading is kept");
    // Through the rebuilt view: the reading inside the window only, with its text gateway ID.
    let through_view = upgraded
        .query(
            &tenant_account,
            "SELECT gateway_id, toTypeName(gateway_id), value_int \
             FROM trustiot.tenant_telemetry FORMAT TSV",
        )
        .await
        .unwrap_or_else(|error| panic!("read through the tenant view: {}", error.body));
    assert_eq!(
        through_view,
        format!("{gateway}\tString\t42\n"),
        "the rebuilt view keeps the window of 0011_queryable_period.sql"
    );
    // The old completion record stays where it was, out of the ingest role's reach.
    let error = upgraded
        .query(&ingest, "SELECT count() FROM trustiot.batch_commits")
        .await
        .expect_err("the ingest role has no privilege on the retired table");
    assert_eq!(error.code(), "ACCESS_DENIED", "{}", error.body);
    #[allow(clippy::print_stdout, reason = "test evidence")]
    {
        println!("legacy reading after the upgrade: {}", row.trim());
        println!("through the tenant view: {}", through_view.trim());
    }
}
