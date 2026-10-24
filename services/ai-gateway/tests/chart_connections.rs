//! The chart's Postgres connection budget of this service (deploy/chart/templates/_postgres.tpl,
//! `trustiot.postgres.connectionBudgets`, Laysi/trustiot-v2#575) matches the pools its code opens.
//! The chart refuses values under which all services could open more connections than Postgres
//! accepts, so a pool that grows or a new one must change the budget in the same change.
//!
//! Other services read the budget with `crates/testkit` (`chart::postgres_connection_budget`,
//! `chart::pool_size_calls`); testkit depends on `contract`, which must not enter this crate's
//! tree, not even as a dev-dependency (tests/no_contract.rs), so this test reads the same JSON
//! itself, the same way.

use std::fs;
use std::path::{Path, PathBuf};

use ai_gateway::config::{MAX_DATABASE_CONNECTIONS, MIN_DATABASE_CONNECTIONS};
use serde_json::Value;

/// The chart helper that holds the budgets, as JSON.
const BUDGETS_HELPER: &str = "trustiot.postgres.connectionBudgets";

fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// This service's budget: `(min, max)` of `database.maxConnections` and `otherPools`.
fn budget() -> ((u64, u64), u64) {
    let path = crate_dir().join("../../deploy/chart/templates/_postgres.tpl");
    let template = fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    let body = template
        .split(&format!(r#"{{{{- define "{BUDGETS_HELPER}" -}}}}"#))
        .nth(1)
        .and_then(|rest| rest.split("{{- end -}}").next())
        .unwrap_or_else(|| panic!("no helper {BUDGETS_HELPER} in {}", path.display()));
    let budgets: Value = serde_json::from_str(body)
        .unwrap_or_else(|error| panic!("{BUDGETS_HELPER} is not JSON: {error}\n{body}"));
    let budget = budgets
        .get("ai-gateway")
        .unwrap_or_else(|| panic!("{BUDGETS_HELPER} has no budget for ai-gateway"));
    let number = |pointer: &str| {
        budget
            .pointer(pointer)
            .and_then(Value::as_u64)
            .unwrap_or_else(|| panic!("{BUDGETS_HELPER}: ai-gateway{pointer} is not a count"))
    };
    (
        (number("/maxConnections/min"), number("/maxConnections/max")),
        number("/otherPools"),
    )
}

/// Every `.max_connections(<argument>)` under `src`, as `(<file relative to src>, <argument>)`,
/// sorted: each is a pool the crate opens.
fn pool_size_calls(src: &Path) -> Vec<(String, String)> {
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

#[test]
fn the_chart_budgets_every_pool_the_service_opens() {
    // The settings pool `Store::connect_lazy` opens, sized by AI_GATEWAY__DATABASE__MAX_CONNECTIONS
    // (the chart's database.maxConnections), is the only one. A new pool adds a call here: budget
    // it in the chart, then list it.
    assert_eq!(
        pool_size_calls(&crate_dir().join("src")),
        [("store.rs", "config.database.max_connections")]
            .map(|(file, argument)| (file.to_owned(), argument.to_owned())),
        "the service's pools changed: update trustiot.postgres.connectionBudgets in \
         deploy/chart/templates/_postgres.tpl and this test"
    );

    let (range, other_pools) = budget();
    assert_eq!(other_pools, 0, "the chart's otherPools for ai-gateway");
    assert_eq!(
        range,
        (
            u64::from(MIN_DATABASE_CONNECTIONS),
            u64::from(MAX_DATABASE_CONNECTIONS)
        ),
        "the chart's database.maxConnections range for ai-gateway is the one the service accepts"
    );
}
