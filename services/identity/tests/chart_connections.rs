//! The chart's Postgres connection budget of this service (deploy/chart/templates/_postgres.tpl,
//! `trustiot.postgres.connectionBudgets`, Laysi/trustiot-v2#575) matches the pools its code opens.
//! The chart refuses values under which all services could open more connections than Postgres
//! accepts, so a pool that grows or a new one must change the budget in the same change.
//!
//! `identity::connections_per_replica` is the one sum of the service's pools: the README's
//! numbers (`src/lib.rs`'s unit test) and the chart's budget (here) are both held to it, and the
//! two lists below fail when the code opens a pool that the sum does not count.

use std::fs;
use std::path::{Path, PathBuf};

use identity::config::MAX_DATABASE_CONNECTIONS;
use identity::connections_per_replica;
use identity::session::WARM_CONNECTIONS;
use testkit::chart::{pool_size_calls, postgres_connection_budget};

/// The service's source directory.
fn src() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

/// Every call in `code` (outside `//` comments) that opens pools: a `<Type>::connect_lazy*`
/// constructor, or `start_event_log`, which opens the event log's two. In source order, as
/// `<Type>::<function>` or `start_event_log`; a line can hold more than one.
fn pool_openers(code: &str) -> Vec<String> {
    const CONSTRUCTOR: &str = "::connect_lazy";
    const EVENT_LOG: &str = "start_event_log(";
    let is_name = |c: char| c.is_alphanumeric() || c == '_';
    let mut openers = Vec::new();
    for line in code.lines() {
        let mut rest = line.trim_start();
        if rest.starts_with("//") {
            continue;
        }
        loop {
            // The first opener left on the line, whichever kind it is.
            let event_log = rest.find(EVENT_LOG);
            let constructor_first = rest
                .find(CONSTRUCTOR)
                .is_some_and(|at| event_log.is_none_or(|next| at < next));
            if constructor_first {
                let Some((before, after)) = rest.split_once(CONSTRUCTOR) else {
                    break;
                };
                let owner = before.rsplit(|c| !is_name(c)).next().unwrap_or_default();
                let suffix: String = after.chars().take_while(|c| is_name(*c)).collect();
                openers.push(format!("{owner}{CONSTRUCTOR}{suffix}"));
                rest = after;
            } else if let Some((before, after)) = rest.split_once(EVENT_LOG) {
                if before.split_whitespace().last() != Some("fn") {
                    openers.push("start_event_log".to_owned());
                }
                rest = after;
            } else {
                break;
            }
        }
    }
    openers
}

#[test]
fn the_chart_budgets_every_pool_the_service_opens() {
    // Where the code sizes a pool. A new pool adds a call here: add it to
    // `identity::connections_per_replica` and to the chart's budget, then list it.
    let calls = pool_size_calls(&src());
    assert_eq!(
        calls,
        [
            // Run delegations (delegation::POOL_CONNECTIONS).
            ("delegation.rs", "POOL_CONNECTIONS"),
            // The event log's two pools, `pool(event_log::SERVICE_CONNECTIONS)` and
            // `pool(event_log::platform_connections())` in `start_event_log`.
            ("lib.rs", "connections"),
            // `LoginStore::connect_lazy`, twice: login::POOL_CONNECTIONS and
            // login::SEND_POOL_CONNECTIONS.
            ("login.rs", "connections"),
            // The ext_authz check's, TRUSTIOT__DATABASE__MAX_CONNECTIONS.
            ("session.rs", "max_connections"),
            // The tenant API's (tenants::POOL_CONNECTIONS), which setting members' roles and the
            // signed-in user's own account share (`TenantStore::connect_lazy_shared`).
            ("tenants.rs", "POOL_CONNECTIONS"),
        ]
        .map(|(file, argument)| (file.to_owned(), argument.to_owned())),
        "the service's pools changed: update trustiot.postgres.connectionBudgets in \
         deploy/chart/templates/_postgres.tpl, `identity::connections_per_replica` and this list"
    );

    // How often `start` opens each pool. Calling an existing constructor once more opens another
    // pool without another `.max_connections(...)`, which the list above would not notice
    // (Laysi/trustiot-v2#575's test report): add it to `identity::connections_per_replica` and
    // to the chart's budget, then list it.
    let path = src().join("lib.rs");
    let lib = fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    assert_eq!(
        pool_openers(&lib),
        [
            // The ext_authz check's (`max_connections`).
            "PgSessionStore::connect_lazy",
            // tenants::POOL_CONNECTIONS, shared by the tenant, member and account APIs.
            "TenantStore::connect_lazy_shared",
            // event_log::SERVICE_CONNECTIONS and event_log::platform_connections(): the two calls
            // in its body, at the end of this list.
            "start_event_log",
            // delegation::POOL_CONNECTIONS.
            "PgDelegationStore::connect_lazy",
            // login::POOL_CONNECTIONS, then login::SEND_POOL_CONNECTIONS.
            "LoginStore::connect_lazy",
            "LoginStore::connect_lazy",
            // In `start_event_log`: the service role's pool and the platform role's.
            "TenantDb::connect_lazy_with_background",
            "PlatformDb::connect_lazy",
        ]
        .map(str::to_owned),
        "`start` opens other pools than `identity::connections_per_replica` counts: update it, \
         trustiot.postgres.connectionBudgets in deploy/chart/templates/_postgres.tpl and this list"
    );

    let budget = postgres_connection_budget("identity");
    assert_eq!(
        (budget.min_max_connections, budget.max_max_connections),
        (WARM_CONNECTIONS, MAX_DATABASE_CONNECTIONS),
        "the chart's database.maxConnections range for identity is the one the service accepts"
    );
    // The chart's sum is the service's for every main pool size the chart accepts.
    for max_connections in budget.min_max_connections..=budget.max_max_connections {
        assert_eq!(
            budget.per_replica(max_connections),
            connections_per_replica(max_connections).total(),
            "the chart's budget for identity (otherPools {}) and \
             identity::connections_per_replica, with database.maxConnections {max_connections}",
            budget.other_pools
        );
    }
}

#[test]
fn the_opener_list_reads_every_call_form() {
    // The forms `start` uses: a constructor that ends its line, one after `let ... =` with a
    // type, one as an argument; a comment and the definition of `start_event_log` are not calls.
    // Several openers on one line all count, in order, and a module path is not the owner.
    let sample = "    let store = PgSessionStore::connect_lazy(\n\
                  \x20       PgDelegationStore::connect_lazy(config.postgres().connect_options())?,\n\
                  \x20   let (db, work): (TenantDb, BackgroundTenantDb) = TenantDb::connect_lazy_with_background(\n\
                  \x20   // TenantStore::connect_lazy(options) would open another pool\n\
                  \x20   let log = start_event_log(&context)?;\n\
                  fn start_event_log(context: &Context<Config>) -> Result<(), BoxError> {\n\
                  \x20   let pools = (LoginStore::connect_lazy(a)?, login::LoginStore::connect_lazy(b)?);\n\
                  \x20   let both = (TenantStore::connect_lazy_shared(o), start_event_log(&c)?, start_event_log(&c)?);\n";
    assert_eq!(
        pool_openers(sample),
        [
            "PgSessionStore::connect_lazy",
            "PgDelegationStore::connect_lazy",
            "TenantDb::connect_lazy_with_background",
            "start_event_log",
            "LoginStore::connect_lazy",
            "LoginStore::connect_lazy",
            "TenantStore::connect_lazy_shared",
            "start_event_log",
            "start_event_log",
        ]
    );
}
