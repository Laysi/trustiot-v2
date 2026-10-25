//! Loads the synthetic fixture into a throwaway ClickHouse with the product's ClickHouse schema
//! (the migration Job's `PRODUCT`), once through the library and once through the
//! `lab-dataset` command for a second tenant, and checks that what ClickHouse holds is the
//! dataset: the row counts of the index, every row with its shifted times, one insert per batch
//! in arrival order, and a completion record for every batch except the uncommitted one.
//!
//! Always a throwaway container, never `TEST_CLICKHOUSE_URL`: the test stops merges and creates
//! an account that reads every row, server-wide state a shared server must not keep.

#![allow(
    clippy::print_stdout,
    clippy::unwrap_used,
    reason = "test helpers unwrap and print like the tests themselves (clippy.toml allows both in \
              tests); the printed summary is what `-- --nocapture` shows as evidence"
)]

/// The ClickHouse container and its HTTP client, shared with the ingest service's schema tests.
#[path = "../../../services/ingest/tests/common/mod.rs"]
mod common;

use std::collections::{BTreeMap, BTreeSet};

use chrono::{TimeDelta, Utc};
use lab_dataset::{Dataset, Loaded, Reading, Target, TimeShift, Value, cli, synthetic_index};
use uuid::Uuid;

use self::common::{Account, ClickHouse};

/// A reader of every row of both tables, for the comparisons. Test-only: the product has no
/// such account, and it exists only in the test's own container.
const READER: &str = "lab_dataset_test_reader";
const CREATE_READER: &str = "CREATE USER lab_dataset_test_reader IDENTIFIED WITH \
     sha256_password BY {password:String} HOST ANY";
const GRANT_TELEMETRY: &str = "GRANT SELECT ON trustiot.telemetry TO lab_dataset_test_reader";
const GRANT_COMMITS: &str = "GRANT SELECT ON trustiot.batch_commits TO lab_dataset_test_reader";
const POLICY_TELEMETRY: &str = "CREATE ROW POLICY lab_dataset_test_all_rows ON \
     trustiot.telemetry AS PERMISSIVE FOR SELECT USING 1 TO lab_dataset_test_reader";
const POLICY_COMMITS: &str = "CREATE ROW POLICY lab_dataset_test_all_rows ON \
     trustiot.batch_commits AS PERMISSIVE FOR SELECT USING 1 TO lab_dataset_test_reader";

/// Keeps every INSERT in its own part, so `_part` shows the insert order.
const STOP_MERGES: &str = "SYSTEM STOP MERGES trustiot.telemetry";

const ROWS: &str = "SELECT toString(stream_id), batch_seq, toString(device_id), point,
  toUnixTimestamp64Milli(ts), toUnixTimestamp64Milli(received_at), value_type, value_float,
  value_int, value_bool, value_string, toString(gateway_id)
FROM trustiot.telemetry WHERE tenant_id = {tenant:UUID} FORMAT TSV";

const SERIES: &str = "SELECT toString(device_id), point, count()
FROM trustiot.telemetry WHERE tenant_id = {tenant:UUID} GROUP BY device_id, point FORMAT TSV";

/// Each batch's insert: the parts holding its rows, and the first part's block number.
const INSERT_ORDER: &str = "SELECT toString(stream_id), batch_seq,
  min(toUInt64(splitByChar('_', _part)[2])) AS block, uniqExact(_part)
FROM trustiot.telemetry WHERE tenant_id = {tenant:UUID}
GROUP BY stream_id, batch_seq ORDER BY block FORMAT TSV";

const COMMITS: &str = "SELECT toString(stream_id), seq, row_count,
  toUnixTimestamp64Milli(committed_at), toString(gateway_id)
FROM trustiot.batch_commits FINAL WHERE tenant_id = {tenant:UUID} ORDER BY stream_id, seq FORMAT TSV";

fn random_hex() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}

async fn query(
    server: &ClickHouse,
    account: &Account,
    sql: &str,
    tenant: Uuid,
) -> Vec<Vec<String>> {
    let tenant = tenant.to_string();
    let body = server
        .query_with(account, sql, &[("param_tenant", &tenant)])
        .await
        .unwrap_or_else(|error| panic!("{sql}\n{}", error.body));
    body.lines()
        .map(|line| line.split('\t').map(str::to_owned).collect())
        .collect()
}

/// A row as the ROWS query prints it.
fn expected_row(loaded: &Loaded, reading: &Reading) -> Vec<String> {
    let shift = loaded.shift();
    let (float, int, flag, text) = match &reading.value {
        Value::Float(float) => (*float, 0, false, String::new()),
        Value::Int(int) => (0.0, *int, false, String::new()),
        Value::Bool(flag) => (0.0, 0, *flag, String::new()),
        Value::String(text) => (0.0, 0, false, text.clone()),
    };
    vec![
        loaded.stream(&reading.stream).unwrap().to_string(),
        reading.batch_seq.to_string(),
        loaded.device(&reading.device).unwrap().to_string(),
        reading.point.clone(),
        shift
            .apply(reading.ts)
            .unwrap()
            .timestamp_millis()
            .to_string(),
        shift
            .apply(reading.received_at)
            .unwrap()
            .timestamp_millis()
            .to_string(),
        reading.value.value_type().as_str().to_owned(),
        float.to_bits().to_string(),
        int.to_string(),
        flag.to_string(),
        text,
        loaded.gateway.to_string(),
    ]
}

/// The ROWS query's output with the float column as its bit pattern, so floats compare exactly.
fn actual_rows(rows: Vec<Vec<String>>) -> Vec<Vec<String>> {
    rows.into_iter()
        .map(|mut row| {
            let float: f64 = row[7]
                .parse()
                .unwrap_or_else(|_| panic!("float column: {row:?}"));
            row[7] = float.to_bits().to_string();
            row
        })
        .collect()
}

/// Checks everything the tenant holds against the dataset.
async fn check_tenant(server: &ClickHouse, reader: &Account, dataset: &Dataset, loaded: &Loaded) {
    let readings = dataset.readings().unwrap();
    let index = dataset.index();
    let tenant = loaded.tenant;

    // Row counts: the total and each point's, as the index states them.
    let rows: u64 = index.segments.iter().map(|segment| segment.rows).sum();
    assert_eq!(loaded.rows, rows);
    let mut stated: BTreeMap<(String, String), u64> = BTreeMap::new();
    for series in index.segments.iter().flat_map(|segment| &segment.series) {
        let device = loaded.device(&series.device).unwrap().to_string();
        *stated.entry((device, series.point.clone())).or_default() += series.rows;
    }
    let counted: BTreeMap<(String, String), u64> = query(server, reader, SERIES, tenant)
        .await
        .into_iter()
        .map(|row| ((row[0].clone(), row[1].clone()), row[2].parse().unwrap()))
        .collect();
    assert_eq!(counted, stated, "rows per point");
    assert_eq!(counted.values().sum::<u64>(), rows, "rows in ClickHouse");
    let names: BTreeMap<String, &str> = loaded
        .devices
        .iter()
        .map(|(key, id)| (id.to_string(), key.as_str()))
        .collect();
    println!(
        "tenant {tenant}: {} rows in ClickHouse, index states {rows}",
        counted.values().sum::<u64>()
    );
    for ((device, point), count) in &counted {
        println!(
            "  {}/{point}: {count} rows, index states {}",
            names[device],
            stated[&(device.clone(), point.clone())]
        );
    }

    // Every row, with its IDs, shifted times and value.
    let mut expected: Vec<Vec<String>> = readings.iter().map(|r| expected_row(loaded, r)).collect();
    let mut actual = actual_rows(query(server, reader, ROWS, tenant).await);
    expected.sort();
    actual.sort();
    assert_eq!(actual, expected, "the rows of tenant {tenant}");

    // One INSERT (one part) per batch, in the order the batches arrived.
    let arrival: Vec<(String, String)> = readings
        .chunk_by(|a, b| a.stream == b.stream && a.batch_seq == b.batch_seq)
        .map(|batch| {
            (
                loaded.stream(&batch[0].stream).unwrap().to_string(),
                batch[0].batch_seq.to_string(),
            )
        })
        .collect();
    let order = query(server, reader, INSERT_ORDER, tenant).await;
    assert!(
        order.iter().all(|row| row[3] == "1"),
        "a batch in several parts: {order:?}"
    );
    let inserted: Vec<(String, String)> = order
        .into_iter()
        .map(|row| (row[0].clone(), row[1].clone()))
        .collect();
    assert_eq!(inserted, arrival, "insert order");
    assert_eq!(u64::try_from(arrival.len()).unwrap(), loaded.batches);

    // A completion record for every batch but the uncommitted ones, committed when received.
    let uncommitted: BTreeSet<(String, u64)> = index
        .segments
        .iter()
        .flat_map(|segment| &segment.uncommitted)
        .map(|batch| (batch.stream.clone(), batch.batch_seq))
        .collect();
    let mut expected_commits: Vec<Vec<String>> = readings
        .chunk_by(|a, b| a.stream == b.stream && a.batch_seq == b.batch_seq)
        .filter(|batch| !uncommitted.contains(&(batch[0].stream.clone(), batch[0].batch_seq)))
        .map(|batch| {
            vec![
                loaded.stream(&batch[0].stream).unwrap().to_string(),
                batch[0].batch_seq.to_string(),
                batch.len().to_string(),
                loaded
                    .shift()
                    .apply(batch[0].received_at)
                    .unwrap()
                    .timestamp_millis()
                    .to_string(),
                loaded.gateway.to_string(),
            ]
        })
        .collect();
    let mut commits = query(server, reader, COMMITS, tenant).await;
    expected_commits.sort();
    commits.sort();
    assert_eq!(commits, expected_commits, "completion records");
    assert_eq!(
        u64::try_from(commits.len()).unwrap(),
        loaded.committed_batches
    );
    assert_eq!(loaded.committed_batches + 1, loaded.batches);
    println!(
        "  {} batches inserted in arrival order, one part each; {} completion records \
         ({} uncommitted); every row equal to the dataset shifted by {} ms",
        arrival.len(),
        commits.len(),
        uncommitted.len(),
        loaded.shift_ms
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_fixture_loads_as_the_ingest_service_writes_it() {
    let Some(server) = ClickHouse::start_throwaway().await else {
        return;
    };
    for migration in migrate::registry::PRODUCT.clickhouse {
        let statements = migrate::clickhouse::split_statements(migration.sql)
            .unwrap_or_else(|error| panic!("{}: {error}", migration.name));
        for statement in statements {
            server
                .admin(&statement)
                .await
                .unwrap_or_else(|error| panic!("{}: {statement}\n{}", migration.name, error.body));
        }
    }
    let reader = Account {
        user: READER.to_owned(),
        password: random_hex(),
    };
    let params = [("param_password", reader.password.as_str())];
    for statement in [
        CREATE_READER,
        GRANT_TELEMETRY,
        GRANT_COMMITS,
        POLICY_TELEMETRY,
        POLICY_COMMITS,
    ] {
        server
            .query_with(&server.admin, statement, &params)
            .await
            .unwrap_or_else(|error| panic!("{statement}\n{}", error.body));
    }

    server.admin_ok(STOP_MERGES).await;

    let dataset = Dataset::open(synthetic_index()).unwrap();
    let writer = lab_dataset::ClickHouse::new(
        &server.url,
        &server.admin.user,
        &server.admin.password,
        None,
    )
    .unwrap();

    // Tenant A through the library, ending within the last day before now.
    let now = Utc::now();
    let shift = dataset
        .shift_ending_before(now, TimeDelta::days(1))
        .unwrap();
    let target = Target::new(Uuid::new_v4(), Uuid::new_v4());
    let a = dataset.load(&writer, &target, shift).await.unwrap();
    assert_eq!((a.rows, a.batches, a.committed_batches), (316, 16, 15));
    let end = shift.apply(dataset.end()).unwrap();
    assert!(
        end <= now && now - end < TimeDelta::days(1),
        "{end} vs {now}"
    );
    check_tenant(&server, &reader, &dataset, &a).await;

    // Tenant B through the command, with an exact shift and one device ID given.
    let tenant_b = Uuid::new_v4();
    let gateway_b = Uuid::new_v4();
    let press = Uuid::new_v4();
    let shift_b = TimeShift::from_millis(a.shift_ms - 3_600_000 - 1);
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_lab-dataset"))
        .arg("load")
        .arg(synthetic_index())
        .args(["--tenant", &tenant_b.to_string()])
        .args(["--gateway", &gateway_b.to_string()])
        .args(["--device", &format!("press-1={press}")])
        .args(["--shift-ms", &shift_b.millis().to_string()])
        .env(cli::URL_ENV, &server.url)
        .env(cli::USER_ENV, &server.admin.user)
        .env(cli::PASSWORD_ENV, &server.admin.password)
        .env_remove(cli::CA_FILE_ENV)
        .output()
        .await
        .expect("run lab-dataset");
    assert!(
        output.status.success(),
        "lab-dataset load failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let printed: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let b = Loaded {
        dataset: printed["dataset"].as_str().unwrap().to_owned(),
        version: u32::try_from(printed["version"].as_u64().unwrap()).unwrap(),
        tenant: printed["tenant"].as_str().unwrap().parse().unwrap(),
        gateway: printed["gateway"].as_str().unwrap().parse().unwrap(),
        shift_ms: printed["shift_ms"].as_i64().unwrap(),
        devices: serde_json::from_value(printed["devices"].clone()).unwrap(),
        streams: serde_json::from_value(printed["streams"].clone()).unwrap(),
        rows: printed["rows"].as_u64().unwrap(),
        batches: printed["batches"].as_u64().unwrap(),
        committed_batches: printed["committed_batches"].as_u64().unwrap(),
    };
    assert_eq!(
        (b.tenant, b.gateway, b.shift()),
        (tenant_b, gateway_b, shift_b)
    );
    assert_eq!(b.device("press-1"), Some(press));
    check_tenant(&server, &reader, &dataset, &b).await;

    // Each tenant has its own devices and streams, and A is unchanged by B's load.
    let ids = |loaded: &Loaded| -> BTreeSet<Uuid> {
        loaded
            .devices
            .values()
            .chain(loaded.streams.values())
            .copied()
            .collect()
    };
    assert!(ids(&a).is_disjoint(&ids(&b)));
    check_tenant(&server, &reader, &dataset, &a).await;
}
