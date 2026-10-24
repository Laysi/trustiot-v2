//! The ingest binary starts with the platform's Postgres settings (the variables
//! `bun scripts/dev.ts env` and the chart set) and a ClickHouse account, and readiness follows
//! both dependencies (Laysi/trustiot-v2#640, acceptance 1): 200 while Postgres and ClickHouse
//! both answer, and not 200, naming the failing check in the log, while either one does not.
//!
//! Postgres is the product's schema, applied by the migration Job (`testkit::product`); ClickHouse
//! is the schema tests' container with the product's ClickHouse DDL, and the service logs in with
//! an account that holds `trustiot_ingest`, as Laysi/trustiot-v2#642 will deliver it. Starts
//! throwaway containers with podman or docker; skipped without either unless `CI` is set. Run
//! with `--nocapture` to see each instance's readiness answer.
#![allow(
    clippy::print_stdout,
    reason = "prints each readiness answer as the test's evidence"
)]

mod common;
mod support;

use common::ClickHouse;
use support::{ClickHouseTarget, Service, closed_port};
use testkit::product::product_postgres;

/// The account the service logs in with in these tests, and how it gets the service's role.
const ACCOUNT: &str = "trustiot_test_ingest";
const CREATE_ACCOUNT: &str = "CREATE USER IF NOT EXISTS trustiot_test_ingest
IDENTIFIED WITH sha256_password BY {password:String} HOST ANY";
const GRANT_ROLE: &str = "GRANT trustiot_ingest TO trustiot_test_ingest";

#[tokio::test(flavor = "multi_thread")]
async fn readiness_follows_postgres_and_clickhouse() {
    let Some(product) = product_postgres(&["ingest_app", "ingest_platform"]).await else {
        return;
    };
    let Some(clickhouse) = ClickHouse::start().await else {
        return;
    };
    clickhouse.apply_migrations().await;
    // 64 hex digits, made at run time.
    let password = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    clickhouse
        .query_with(
            &clickhouse.admin,
            CREATE_ACCOUNT,
            &[("param_password", &password)],
        )
        .await
        .unwrap_or_else(|error| panic!("create the service's account: {}", error.body));
    clickhouse.admin_ok(GRANT_ROLE).await;

    let reachable = ClickHouseTarget {
        url: clickhouse.url.clone(),
        user: ACCOUNT.to_owned(),
        password: password.clone(),
    };
    let clickhouse_down = ClickHouseTarget {
        url: format!("http://127.0.0.1:{}", closed_port()),
        user: ACCOUNT.to_owned(),
        password: password.clone(),
    };
    let wrong_password = ClickHouseTarget {
        url: clickhouse.url.clone(),
        user: ACCOUNT.to_owned(),
        password: format!("{password}-wrong"),
    };
    let (both, no_clickhouse, refused, no_postgres) = tokio::join!(
        Service::start(&product, None, &reachable),
        Service::start(&product, None, &clickhouse_down),
        Service::start(&product, None, &wrong_password),
        Service::start(&product, Some(closed_port()), &reachable),
    );

    // 1. Both answer: ready.
    both.wait_ready().await;
    let (status, body) = both.readiness().await;
    assert_eq!(status, 200, "{body}");
    println!("both reachable: /readyz {status} {body}");

    // 2. ClickHouse unreachable, or refusing the account: not ready, and the log names the
    //    `clickhouse` check; Postgres is fine there.
    for (name, service) in [
        ("ClickHouse unreachable", &no_clickhouse),
        ("ClickHouse refuses the password", &refused),
    ] {
        let failed = service.wait_check_failed("clickhouse").await;
        let (status, body) = service.readiness().await;
        assert_ne!(status, 200, "{name}: {body}");
        println!(
            "{name}: /readyz {status} {body}; logged {}",
            failed["fields"]
        );
        assert!(
            service
                .stdout
                .messages("readiness check failed")
                .iter()
                .all(|line| line["fields"]["check"] == "clickhouse"),
            "{name}: only the clickhouse check fails\n{}",
            service.stdout.text()
        );
        assert!(
            !service.stdout.text().contains(&password),
            "{name}: the log contains the ClickHouse password"
        );
    }

    // 3. Postgres unreachable (ClickHouse is fine): not ready, and the log names the `postgres`
    //    check.
    let failed = no_postgres.wait_check_failed("postgres").await;
    let (status, body) = no_postgres.readiness().await;
    assert_ne!(status, 200, "{body}");
    println!(
        "Postgres unreachable: /readyz {status} {body}; logged {}",
        failed["fields"]
    );
    assert!(
        no_postgres
            .stdout
            .messages("readiness check failed")
            .iter()
            .all(|line| line["fields"]["check"] != "clickhouse"),
        "the clickhouse check fails with Postgres down\n{}",
        no_postgres.stdout.text()
    );

    // 4. The service that has both is still ready: the failures were the others' own.
    let (status, body) = both.readiness().await;
    assert_eq!(status, 200, "{body}");
}
