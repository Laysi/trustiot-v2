//! The ingest binary starts with the platform's Postgres settings (the variables
//! `bun scripts/dev.ts env` and the chart set), a ClickHouse account and a broker account, and
//! readiness follows the three dependencies (Laysi/trustiot-v2#640, acceptance 1;
//! Laysi/trustiot-v2#641): 200 while Postgres, ClickHouse and the broker all answer, and not 200,
//! naming the failing check in the log, while one does not; once the broker is back, ready
//! again. The running binary then takes a batch from the broker and acknowledges it `committed`,
//! and the same batch again `duplicate` with the same row count (Laysi/trustiot-v2#641,
//! acceptance 1, through the real process; tests/telemetry.rs has the rest of the write path).
//!
//! Postgres is the product's schema, applied by the migration Job (`testkit::product`); ClickHouse
//! is the schema tests' container with the product's ClickHouse DDL, and the service logs in with
//! an account that holds `trustiot_ingest`, as Laysi/trustiot-v2#642 will deliver it; the broker
//! is Mosquitto with the role the migration Job gives `service:ingest` (tests/support/broker.rs).
//! Starts throwaway containers with podman or docker; skipped without either unless `CI` is set.
//! Run with `--nocapture` to see each instance's readiness answer.
#![allow(
    clippy::print_stdout,
    reason = "prints each readiness answer as the test's evidence"
)]

mod common;
mod support;

use common::{Account, ClickHouse};
use protocol::telemetry::AckOutcome;
use protocol::{GatewayId, StreamId};
use support::broker::{Broker, Gateway, batch, now_millis};
use support::{ClickHouseTarget, START_LIMIT, Service, bind, closed_port};
use testkit::product::product_postgres;

/// The account the service logs in with in these tests, and how it gets the service's role.
const ACCOUNT: &str = "trustiot_test_ingest";
const CREATE_ACCOUNT: &str = "CREATE USER IF NOT EXISTS trustiot_test_ingest
IDENTIFIED WITH sha256_password BY {password:String} HOST ANY";
const GRANT_ROLE: &str = "GRANT trustiot_ingest TO trustiot_test_ingest";

/// The rows of one batch.
const BATCH_ROWS: &str = "SELECT count() FROM trustiot.telemetry
WHERE gateway_id = {gateway:String} AND stream_id = {stream:String} AND batch_seq = {seq:UInt64}
FORMAT TabSeparated";

/// What the service logs once its subscription is granted.
const SUBSCRIBED: &str = "connected to the mqtt broker; telemetry subscribed";

#[tokio::test(flavor = "multi_thread")]
#[allow(
    clippy::too_many_lines,
    reason = "one scenario on one set of containers, step by step"
)]
async fn readiness_follows_postgres_clickhouse_and_the_broker() {
    let Some(product) = product_postgres(&["ingest_app", "ingest_platform"]).await else {
        return;
    };
    let Some(clickhouse) = ClickHouse::start().await else {
        return;
    };
    let Some(broker) = Broker::start() else {
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
    let mqtt = broker.ingest_config();

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
    let (all, no_clickhouse, refused, no_postgres) = tokio::join!(
        Service::start(&product, None, &reachable, &mqtt),
        Service::start(&product, None, &clickhouse_down, &mqtt),
        Service::start(&product, None, &wrong_password, &mqtt),
        Service::start(&product, Some(closed_port()), &reachable, &mqtt),
    );

    // 1. All three answer: ready.
    all.wait_ready().await;
    let (status, body) = all.readiness().await;
    assert_eq!(status, 200, "{body}");
    println!("all reachable: /readyz {status} {body}");

    // 2. ClickHouse unreachable, or refusing the account: not ready, and the log names the
    //    `clickhouse` check; Postgres and the broker are fine there. Each instance is connected
    //    to the broker before its readiness is first asked, so only the dependency it lacks
    //    fails.
    for service in [&no_clickhouse, &refused, &no_postgres] {
        service.wait_logged(SUBSCRIBED).await;
    }
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
            .all(
                |line| line["fields"]["check"] != "clickhouse" && line["fields"]["check"] != "mqtt"
            ),
        "the clickhouse or mqtt check fails with Postgres down\n{}",
        no_postgres.stdout.text()
    );

    // 4. The service that has all three is still ready: the failures were the others' own.
    let (status, body) = all.readiness().await;
    assert_eq!(status, 200, "{body}");
    // The others share the subscription's group; from here on only `all` takes batches.
    drop((no_clickhouse, refused, no_postgres));

    // 5. The broker stops: not ready, and the log names the `mqtt` check; back, ready again.
    broker.stop();
    let failed = all.wait_check_failed("mqtt").await;
    let (status, body) = all.readiness().await;
    assert_ne!(status, 200, "broker stopped: {body}");
    println!(
        "broker stopped: /readyz {status} {body}; logged {}",
        failed["fields"]
    );
    broker.resume();
    // The client waits up to its longest reconnect delay between attempts, and an attempt may
    // take its connect timeout; the rest is margin for a loaded host.
    all.wait_ready_within(
        mqtt_client::RECONNECT_DELAY_MAX + mqtt_client::CONNECT_TIMEOUT + START_LIMIT,
    )
    .await;
    let (status, body) = all.readiness().await;
    assert_eq!(status, 200, "broker back: {body}");
    println!("broker back: /readyz {status} {body}");
    let broker_password = std::fs::read_to_string(&mqtt.password_file).expect("read the file");
    assert!(
        !all.stdout.text().contains(broker_password.trim()),
        "the log contains the broker password"
    );

    // 6. A batch through the running binary: committed, and the same batch again duplicate with
    //    the same row count; the table holds it once.
    let gateway = GatewayId::new(format!("gw-{}", uuid::Uuid::new_v4().simple())).expect("an ID");
    let stream = StreamId::new("startup").expect("an ID");
    bind(&product, &gateway, uuid::Uuid::new_v4(), 1).await;
    let mut sender = Gateway::connect(&broker).await;
    // The admin reads no telemetry row (row policy `deny_by_default`); the service's account
    // reads every row.
    let ingest_account = Account {
        user: ACCOUNT.to_owned(),
        password: password.clone(),
    };
    let devices = [uuid::Uuid::new_v4(), uuid::Uuid::new_v4()];
    let payload = batch(&stream, 1, &devices, 5, now_millis())
        .encode()
        .expect("encode the batch");
    let records = 2 * 2 * 5;
    let seq = "1".to_owned();
    let params = [
        ("param_gateway", gateway.as_str()),
        ("param_stream", stream.as_str()),
        ("param_seq", seq.as_str()),
    ];
    for (attempt, expected) in [
        ("first", AckOutcome::Committed { records }),
        ("again", AckOutcome::Duplicate { records }),
    ] {
        sender.send(&gateway, payload.clone(), false).await;
        let ack = sender.ack(&gateway, &stream, 1).await;
        assert_eq!(ack.outcome, expected, "{attempt}");
        let rows = clickhouse
            .query_with(&ingest_account, BATCH_ROWS, &params)
            .await
            .unwrap_or_else(|error| panic!("count the batch's rows: {}", error.body));
        assert_eq!(rows.trim(), records.to_string(), "{attempt}");
        println!(
            "batch sent {attempt}: acknowledged {:?}, {} rows in trustiot.telemetry",
            ack.outcome,
            rows.trim()
        );
    }
}
