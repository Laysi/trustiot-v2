//! The chart's Postgres connection budget of this service (deploy/chart/templates/_postgres.tpl,
//! `trustiot.postgres.connectionBudgets`, Laysi/trustiot-v2#575) matches the pools its code opens.
//! The chart refuses values under which all services could open more connections than Postgres
//! accepts, so a pool that grows or a new one must change the budget in the same change.

use std::path::Path;

use telemetry_query::EVENT_READER_CONNECTIONS;
use telemetry_query::config::{MAX_DATABASE_CONNECTIONS, MIN_DATABASE_CONNECTIONS};
use testkit::chart::{pool_size_calls, postgres_connection_budget};

#[test]
fn the_chart_budgets_every_pool_the_service_opens() {
    // The pools `telemetry_query::start` opens: the service role's, sized by
    // TRUSTIOT__DATABASE__MAX_CONNECTIONS (the chart's database.maxConnections), and the platform
    // role's event reader. A new pool adds a call here: budget it in the chart, then list it.
    let calls = pool_size_calls(&Path::new(env!("CARGO_MANIFEST_DIR")).join("src"));
    assert_eq!(
        calls,
        [
            ("lib.rs", "EVENT_READER_CONNECTIONS"),
            ("lib.rs", "config.database.max_connections"),
        ]
        .map(|(file, argument)| (file.to_owned(), argument.to_owned())),
        "the service's pools changed: update trustiot.postgres.connectionBudgets in \
         deploy/chart/templates/_postgres.tpl and this test"
    );

    let budget = postgres_connection_budget("telemetry-query");
    assert_eq!(
        budget.other_pools, EVENT_READER_CONNECTIONS,
        "the chart's otherPools for telemetry-query"
    );
    assert_eq!(
        (budget.min_max_connections, budget.max_max_connections),
        (MIN_DATABASE_CONNECTIONS, MAX_DATABASE_CONNECTIONS),
        "the chart's database.maxConnections range for telemetry-query is the one the service \
         accepts"
    );
}
