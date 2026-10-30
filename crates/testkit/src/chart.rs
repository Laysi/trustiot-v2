//! What the deployment chart (deploy/chart) assumes about a service, so a service's tests can
//! hold its code to the same numbers.
//!
//! [`postgres_connection_budget`] reads the chart's Postgres connection budget
//! (`trustiot.postgres.connectionBudgets` in deploy/chart/templates/_postgres.tpl,
//! Laysi/trustiot-v2#575): the chart adds up what every replica of every service can open and
//! refuses values that do not fit the bundled Postgres' `max_connections`. A service whose pools
//! change without the budget changing with them would make that check wrong, so each service that
//! uses Postgres has a test comparing its pools with its budget, and [`pool_size_calls`] lists the
//! pools its code opens, so a new pool fails that test until someone budgets it.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

/// The chart helper that holds the budgets, as JSON.
const BUDGETS_HELPER: &str = "trustiot.postgres.connectionBudgets";

/// The chart's Postgres connection budget of one service: what one replica opens at most.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PostgresConnectionBudget {
    /// Smallest `services.<name>.database.maxConnections` the chart accepts, which it passes as
    /// `TRUSTIOT__DATABASE__MAX_CONNECTIONS` (the service's main pool).
    pub min_max_connections: u32,
    /// Largest `services.<name>.database.maxConnections` the chart accepts.
    pub max_max_connections: u32,
    /// Every other pool one replica opens, at its largest, whatever role it logs in as.
    pub other_pools: u32,
}

impl PostgresConnectionBudget {
    /// What one replica opens at most with a main pool of `max_connections`.
    #[must_use]
    pub const fn per_replica(self, max_connections: u32) -> u32 {
        max_connections + self.other_pools
    }
}

/// The repository's root, from this crate's directory.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The chart's connection budget of `service` (a key under the chart's `services`).
///
/// # Panics
///
/// When the chart's template cannot be read, the helper is not where it was, its body is not the
/// JSON this expects, or it has no budget for `service`: each is a test failure.
#[must_use]
pub fn postgres_connection_budget(service: &str) -> PostgresConnectionBudget {
    let path = repo_root().join("deploy/chart/templates/_postgres.tpl");
    let template = fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    let body = template
        .split(&format!(r#"{{{{- define "{BUDGETS_HELPER}" -}}}}"#))
        .nth(1)
        .and_then(|rest| rest.split("{{- end -}}").next())
        .unwrap_or_else(|| panic!("no helper {BUDGETS_HELPER} in {}", path.display()));
    let budgets: Value = serde_json::from_str(body)
        .unwrap_or_else(|error| panic!("{BUDGETS_HELPER} is not JSON: {error}\n{body}"));
    let budget = budgets.get(service).unwrap_or_else(|| {
        panic!("{BUDGETS_HELPER} has no budget for {service}; add one (deploy/chart/README.md)")
    });
    let number = |pointer: &str| {
        budget
            .pointer(pointer)
            .and_then(Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
            .unwrap_or_else(|| panic!("{BUDGETS_HELPER}: {service}{pointer} is not a count"))
    };
    PostgresConnectionBudget {
        min_max_connections: number("/maxConnections/min"),
        max_max_connections: number("/maxConnections/max"),
        other_pools: number("/otherPools"),
    }
}

/// Every `.max_connections(<argument>)` in the Rust files under `src` (a crate's `src`
/// directory, for example `concat!(env!("CARGO_MANIFEST_DIR"), "/src")`), as
/// `(<path relative to src, with />, <argument>)`, sorted. Each is one pool the crate opens (or a
/// helper that opens them), so a test that compares this list with the pools it budgets fails
/// when a pool is added.
///
/// # Panics
///
/// When a directory or file under `src` cannot be read.
#[must_use]
pub fn pool_size_calls(src: &Path) -> Vec<(String, String)> {
    let mut calls = Vec::new();
    let mut directories = vec![src.to_path_buf()];
    while let Some(directory) = directories.pop() {
        let entries = fs::read_dir(&directory)
            .unwrap_or_else(|error| panic!("read {}: {error}", directory.display()));
        for entry in entries {
            let path = entry
                .unwrap_or_else(|error| panic!("read {}: {error}", directory.display()))
                .path();
            if path.is_dir() {
                directories.push(path);
                continue;
            }
            if path.extension().is_none_or(|extension| extension != "rs") {
                continue;
            }
            let text = fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
            let relative = path
                .strip_prefix(src)
                .unwrap_or(&path)
                .components()
                .map(|part| part.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");
            for after in text.split(".max_connections(").skip(1) {
                let argument: String = after.chars().take_while(|c| *c != ')').collect();
                calls.push((relative.clone(), argument.trim().to_owned()));
            }
        }
    }
    calls.sort();
    calls
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_every_budget_of_the_chart() {
        for service in [
            "assets",
            "assistant",
            "certs",
            "gateway-ops",
            "identity",
            "ingest",
            "telemetry-query",
        ] {
            let budget = postgres_connection_budget(service);
            assert!(
                budget.min_max_connections <= budget.max_max_connections,
                "{service}: {budget:?}"
            );
            assert_eq!(
                budget.per_replica(10),
                10 + budget.other_pools,
                "{service}: {budget:?}"
            );
        }
    }

    #[test]
    #[should_panic(expected = "has no budget for knowledge")]
    fn a_service_without_a_budget_fails() {
        let _ = postgres_connection_budget("knowledge");
    }

    #[test]
    fn lists_the_pool_size_calls_of_a_crate() {
        let src = std::env::temp_dir().join(format!("testkit-chart-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(src.join("store")).expect("create the sample crate");
        fs::write(
            src.join("lib.rs"),
            "let a = PgPoolOptions::new()\n    .max_connections(config.database.max_connections)\n    \
             .min_connections(0);\nlet b = PgPoolOptions::new().max_connections( READER );\n",
        )
        .expect("write lib.rs");
        fs::write(
            src.join("store/pool.rs"),
            ".max_connections(POOL_CONNECTIONS)",
        )
        .expect("write store/pool.rs");
        fs::write(src.join("notes.md"), ".max_connections(ignored)").expect("write notes.md");
        let calls = pool_size_calls(&src);
        fs::remove_dir_all(&src).expect("remove the sample crate");
        assert_eq!(
            calls,
            [
                ("lib.rs", "READER"),
                ("lib.rs", "config.database.max_connections"),
                ("store/pool.rs", "POOL_CONNECTIONS"),
            ]
            .map(|(file, argument)| (file.to_owned(), argument.to_owned()))
        );
    }
}
