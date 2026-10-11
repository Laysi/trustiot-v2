//! Static checks of the ClickHouse migration files; no server needed.

mod common;

use common::{migrations, migrations_dir, split_statements};

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

/// The migration Job may apply a file again (for example after a failed deploy), so every
/// statement must be safe to repeat, and none may destroy data.
#[test]
fn statements_are_idempotent_and_never_destroy_data() {
    for migration in migrations() {
        for statement in &migration.statements {
            let words = words(statement);
            let text = words.join(" ");
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
        tables >= 2,
        "expected the telemetry and batch_commits tables"
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
            if !(text.starts_with("CREATE VIEW ") || text.starts_with("CREATE MATERIALIZED VIEW "))
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
