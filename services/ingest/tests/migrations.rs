//! Static checks of the ClickHouse migration files, which need no server, and the upgrade of
//! `0010_gateway_stream_ids.sql` against real ClickHouse servers.

mod common;

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

/// `CREATE OR REPLACE VIEW` of a plain view: the one replacement the files may use. A plain view
/// holds no data, ClickHouse keeps grants and row policies by the relation's name, and 26.8 has
/// no other way to change a plain view's columns (`MODIFY QUERY` is refused for storage View,
/// see 0010_gateway_stream_ids.sql). A materialized view is not covered: it holds data.
fn replaces_a_plain_view(text: &str) -> bool {
    text.starts_with("CREATE OR REPLACE VIEW ")
}

/// The migration Job may apply a file again (for example after a failed deploy), so every
/// statement must be safe to repeat, and none may destroy data.
#[test]
fn statements_are_idempotent_and_never_destroy_data() {
    for migration in migrations() {
        for statement in &migration.statements {
            let words = words(statement);
            let text = words.join(" ");
            if replaces_a_plain_view(&text) {
                // Idempotent by itself: repeating it gives the same view. No IF NOT EXISTS, and
                // the rest of the statement may still not hold a destructive word.
                let rest = text.replacen("CREATE OR REPLACE VIEW ", "CREATE VIEW ", 1);
                for forbidden in ["OR REPLACE", "DROP", "TRUNCATE", "DETACH", "EXCHANGE"] {
                    assert!(
                        !format!(" {rest} ").contains(&format!(" {forbidden} ")),
                        "{}: {forbidden} is not allowed in migrations:\n{statement}",
                        migration.name
                    );
                }
                continue;
            }
            if words.first().map(String::as_str) == Some("CREATE") {
                assert!(
                    text.contains(" IF NOT EXISTS "),
                    "{}: CREATE without IF NOT EXISTS:\n{statement}",
                    migration.name
                );
            }
            for forbidden in ["OR REPLACE", "DROP", "TRUNCATE", "DETACH", "EXCHANGE"] {
                assert!(
                    !format!(" {text} ").contains(&format!(" {forbidden} ")),
                    "{}: {forbidden} is not allowed in migrations (it replaces or destroys data, \
                     or drops the grants that point at an access entity):\n{statement}",
                    migration.name
                );
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
                || replaces_a_plain_view(&text))
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
const ID_MIGRATION: &str = "0010_gateway_stream_ids.sql";

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

fn split_at_id_migration() -> (Vec<Migration>, Migration) {
    let mut all = migrations();
    let at = all
        .iter()
        .position(|migration| migration.name == ID_MIGRATION)
        .unwrap_or_else(|| panic!("{ID_MIGRATION} is missing"));
    let mut after = all.split_off(at);
    let id_migration = after.remove(0);
    // The upgrade case is a server at 0009: the ID migration is the newest file it gets. A later
    // file joins this test only if it must be compared the same way.
    let later: Vec<&str> = after.iter().map(|m| m.name.as_str()).collect();
    assert!(
        later.is_empty(),
        "files after {ID_MIGRATION}: {later:?}; extend this test to apply them in both cases"
    );
    (all, id_migration)
}

/// Laysi/trustiot-v2#639: the ID migration gives the same schema on a fresh server and on a server
/// that already had 0001 to 0009 and data in the old shape (a reading with UUID gateway and stream
/// IDs and an old completion record), and applying it a second time changes nothing on either.
/// The reading written before the change stays readable, with its UUIDs as text, also through the
/// tenant view.
///
/// Always two containers of the test's own, one after the other: the comparison needs servers on
/// which nothing was applied before.
#[tokio::test(flavor = "current_thread")]
async fn id_migration_gives_one_schema_on_fresh_and_upgraded_servers() {
    let (before, id_migration) = split_at_id_migration();

    // A fresh server: every file, then the ID migration once more.
    let Some(fresh) = ClickHouse::start_fresh().await else {
        return;
    };
    fresh.apply_migrations().await;
    let fresh_schema = schema(&fresh).await;
    fresh.apply(&id_migration).await;
    assert_eq!(
        schema(&fresh).await,
        fresh_schema,
        "applying {ID_MIGRATION} again changed a fresh server"
    );
    drop(fresh);

    // A server at 0009 with data written in the old shape.
    let Some(upgraded) = ClickHouse::start_fresh().await else {
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
    upgraded
        .admin_ok(&format!(
            "CREATE USER {} IDENTIFIED WITH sha256_password BY '{password}' \
             SETTINGS PROFILE 'trustiot_tenant_reader', SQL_tenant_id = '{tenant}' CONST",
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

    upgraded.apply(&id_migration).await;
    let upgraded_schema = schema(&upgraded).await;
    upgraded.apply(&id_migration).await;
    assert_eq!(
        schema(&upgraded).await,
        upgraded_schema,
        "applying {ID_MIGRATION} again changed an upgraded server"
    );
    assert_eq!(
        upgraded_schema, fresh_schema,
        "{ID_MIGRATION} gives a different schema on a server that had 0001 to 0009"
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
    let through_view = upgraded
        .query(
            &tenant_account,
            "SELECT gateway_id, toTypeName(gateway_id), value_int \
             FROM trustiot.tenant_telemetry FORMAT TSV",
        )
        .await
        .unwrap_or_else(|error| panic!("read through the tenant view: {}", error.body));
    assert_eq!(through_view, format!("{gateway}\tString\t42\n"));
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
