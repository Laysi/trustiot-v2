//! The chart's Postgres connection budget of this service (deploy/chart/templates/_postgres.tpl,
//! `trustiot.postgres.connectionBudgets`, Laysi/trustiot-v2#575) matches the pools its code opens.
//! The chart refuses values under which all services could open more connections than Postgres
//! accepts, so a pool that grows or a new one must change the budget in the same change.

use std::path::Path;

use identity::config::MAX_DATABASE_CONNECTIONS;
use identity::session::WARM_CONNECTIONS;
use identity::{delegation, event_log, login, tenants};
use testkit::chart::{pool_size_calls, postgres_connection_budget};

/// Every pool `identity::start` opens besides the ext_authz check's, at its largest: the tenant
/// API's, the login requests' and the login sends' (`LoginStore::connect_lazy`), the run
/// delegations' and the event log's as `identity_app`, and the event log's as
/// `identity_platform` (services/identity/README.md, "連線池").
fn other_pools() -> u32 {
    tenants::POOL_CONNECTIONS
        + login::POOL_CONNECTIONS
        + login::SEND_POOL_CONNECTIONS
        + delegation::POOL_CONNECTIONS
        + event_log::SERVICE_CONNECTIONS
        + event_log::platform_connections()
}

#[test]
fn the_chart_budgets_every_pool_the_service_opens() {
    // Where the code sizes a pool. A new pool adds a call here: add it to `other_pools` and to
    // the chart's budget, then list it.
    let calls = pool_size_calls(&Path::new(env!("CARGO_MANIFEST_DIR")).join("src"));
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
            // The tenant API (tenants::POOL_CONNECTIONS).
            ("tenants.rs", "POOL_CONNECTIONS"),
        ]
        .map(|(file, argument)| (file.to_owned(), argument.to_owned())),
        "the service's pools changed: update trustiot.postgres.connectionBudgets in \
         deploy/chart/templates/_postgres.tpl, `other_pools` and this list"
    );

    let budget = postgres_connection_budget("identity");
    assert_eq!(
        budget.other_pools,
        other_pools(),
        "the chart's otherPools for identity"
    );
    assert_eq!(
        (budget.min_max_connections, budget.max_max_connections),
        (WARM_CONNECTIONS, MAX_DATABASE_CONNECTIONS),
        "the chart's database.maxConnections range for identity is the one the service accepts"
    );
}
