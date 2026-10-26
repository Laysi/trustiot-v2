//! The history API of device charts (Laysi/trustiot-v2#119, acceptance 3 and 4): a device's
//! series, downsampled over the requested range in the requested time zone, equals what a
//! direct query of the telemetry table computes, with buckets that start at the time zone's
//! local hours and midnights, from the aggregates' whole cells (Laysi/trustiot-v2#143: a range
//! that is not on cell boundaries reads the cells that start in it); raw readings and latest values equal the stored ones; a range that
//! starts before the queryable period is cut to it and one wholly before it is refused; and the
//! API's refusals (no token, no tenant, malformed and invalid parameters, too many points, the
//! tenant's query quota).
//!
//! The expected results come from the telemetry table itself, read by a test-only account that
//! sees every row, with other ClickHouse functions than the service's (`toStartOfHour`,
//! `toStartOfDay`, `toStartOfFifteenMinutes` instead of `toStartOfInterval`).
//!
//! Needs podman or docker (a TLS Postgres and a ClickHouse); skipped without one unless `CI` is
//! set. One test: the binary shares one product database.

mod support;

use chrono::{DateTime, TimeDelta, Utc};
use contract::{ServiceName, TenantId, TenantRole};
use serde_json::Value;
use support::{ClickHouseTarget, Readings, Service, Stack, wait_account_ready};
use testkit::identity::TestIssuer;

/// The reference: per bucket, the bucket's start in Unix milliseconds, the count, min, max and
/// mean of `value_float`, and the latest `value_float`.
macro_rules! reference_float {
    ($bucket:literal) => {
        concat!(
            "SELECT toUnixTimestamp(",
            $bucket,
            ") * 1000 AS bucket, count(), min(value_float), max(value_float), avg(value_float),
  argMax(value_float, ts)
FROM trustiot.telemetry
WHERE tenant_id = {tenant:UUID} AND device_id = {device:UUID} AND point = {point:String}
  AND ts >= fromUnixTimestamp64Milli({from:Int64}) AND ts < fromUnixTimestamp64Milli({to:Int64})
GROUP BY bucket ORDER BY bucket FORMAT JSONCompactEachRow"
        )
    };
}

const REF_HOUR: &str = reference_float!("toStartOfHour(ts, {tz:String})");
const REF_3_HOURS: &str = reference_float!(
    "toStartOfDay(ts, {tz:String}) + toIntervalHour(intDiv(toHour(ts, {tz:String}), 3) * 3)"
);
const REF_DAY: &str = reference_float!("toStartOfDay(ts, {tz:String})");
const REF_15_MINUTES: &str = reference_float!("toStartOfFifteenMinutes(ts)");
const REF_5_MINUTES: &str = reference_float!("toStartOfFiveMinutes(ts)");

/// The same for an integer data point, per local hour.
const REF_HOUR_INT: &str =
    "SELECT toUnixTimestamp(toStartOfHour(ts, {tz:String})) * 1000 AS bucket,
  count(), min(value_int), max(value_int), avg(value_int), argMax(value_int, ts)
FROM trustiot.telemetry
WHERE tenant_id = {tenant:UUID} AND device_id = {device:UUID} AND point = {point:String}
  AND ts >= fromUnixTimestamp64Milli({from:Int64}) AND ts < fromUnixTimestamp64Milli({to:Int64})
GROUP BY bucket ORDER BY bucket FORMAT JSONCompactEachRow";

/// Raw readings of a float data point.
const REF_RAW: &str = "SELECT toUnixTimestamp64Milli(ts), value_float
FROM trustiot.telemetry
WHERE tenant_id = {tenant:UUID} AND device_id = {device:UUID} AND point = {point:String}
  AND ts >= fromUnixTimestamp64Milli({from:Int64}) AND ts < fromUnixTimestamp64Milli({to:Int64})
ORDER BY ts FORMAT JSONCompactEachRow";

/// The latest reading per data point since `from`.
const REF_LATEST: &str = "SELECT point, toUnixTimestamp64Milli(max(ts)),
  toString(argMax(value_type, ts)), argMax(value_float, ts), argMax(value_int, ts),
  argMax(value_bool, ts), argMax(value_string, ts)
FROM trustiot.telemetry
WHERE tenant_id = {tenant:UUID} AND device_id = {device:UUID}
  AND ts >= fromUnixTimestamp64Milli({from:Int64})
GROUP BY point ORDER BY point FORMAT JSONCompactEachRow";

/// How many readings of a data point lie in a range.
const REF_COUNT: &str = "SELECT count() FROM trustiot.telemetry
WHERE tenant_id = {tenant:UUID} AND device_id = {device:UUID} AND point = {point:String}
  AND ts >= fromUnixTimestamp64Milli({from:Int64}) AND ts < fromUnixTimestamp64Milli({to:Int64})
FORMAT TSV";

const MINUTE_MS: i64 = 60_000;
const HOUR_MS: i64 = 60 * MINUTE_MS;
const DAY_MS: i64 = 24 * HOUR_MS;

/// The service's clamp starts this much after the database's window (`period::PERIOD_MARGIN`).
const MARGIN_MS: i64 = 5 * MINUTE_MS;

/// `ms` rounded up to a multiple of `unit`.
fn ceil(ms: i64, unit: i64) -> i64 {
    ms + (unit - ms.rem_euclid(unit)) % unit
}

fn rfc3339(ms: i64) -> String {
    DateTime::<Utc>::from_timestamp_millis(ms)
        .expect("a time")
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn ms(value: &Value) -> i64 {
    value
        .as_str()
        .and_then(|text| text.parse::<DateTime<Utc>>().ok())
        .unwrap_or_else(|| panic!("not a time: {value}"))
        .timestamp_millis()
}

fn lines(body: &str) -> Vec<Value> {
    body.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("a JSON line"))
        .collect()
}

fn close(actual: &Value, expected: &Value, what: &str) {
    let (actual, expected) = (
        actual
            .as_f64()
            .unwrap_or_else(|| panic!("{what}: {actual}")),
        expected
            .as_f64()
            .unwrap_or_else(|| panic!("{what}: {expected}")),
    );
    assert!(
        (actual - expected).abs() <= 1e-9 * expected.abs().max(1.0),
        "{what}: {actual} != {expected}"
    );
}

struct Fixture {
    stack: Stack,
    service: Service,
    issuer: TestIssuer,
    tenant: TenantId,
    token: String,
    device: uuid::Uuid,
}

impl Fixture {
    fn params<'a>(
        &'a self,
        point: &'a str,
        tenant: &'a str,
        device: &'a str,
    ) -> [(&'a str, &'a str); 3] {
        [("tenant", tenant), ("device", device), ("point", point)]
    }

    async fn series(&self, token: &str, query: &str) -> Value {
        self.service
            .get_ok(
                &format!("/api/telemetry/v1/devices/{}/series?{query}", self.device),
                token,
            )
            .await
    }

    async fn status(&self, token: Option<&str>, path_and_query: &str) -> (u16, Value) {
        let (status, body) = self.service.get(path_and_query, token).await;
        let json = serde_json::from_str(&body).unwrap_or(Value::Null);
        (status, json)
    }
}

/// A refused request: the token (none: without the header), the path and query, the status, the
/// error code, and the message key (none: not checked).
type RefusalCase<'a> = (Option<&'a str>, String, u16, &'a str, Option<&'a str>);

/// Compares an aggregated float series over the requested range with the reference over `cells`,
/// bucket by bucket: `cells` is the requested range with both ends rounded up to the start of
/// the aggregate's next cell, as the series sums the cells that start in the requested range.
#[allow(
    clippy::print_stderr,
    reason = "prints each compared series for the test report"
)]
async fn assert_float_series(
    fixture: &Fixture,
    reference: &str,
    tz: &str,
    interval: Option<&str>,
    (from, to): (i64, i64),
    cells: (i64, i64),
    alignment: Option<(i64, i64)>,
) {
    let query = match interval {
        Some(interval) => format!(
            "point=temp&from={}&to={}&tz={tz}&interval={interval}",
            rfc3339(from),
            rfc3339(to)
        ),
        None => format!(
            "point=temp&from={}&to={}&tz={tz}",
            rfc3339(from),
            rfc3339(to)
        ),
    };
    let series = fixture.series(&fixture.token, &query).await;
    assert_eq!(series["truncated"], false, "{series}");
    assert_eq!(ms(&series["from"]), from);
    assert_eq!(ms(&series["to"]), to);
    assert_eq!(series["tz"], tz);
    let (tenant, device) = (fixture.tenant.to_string(), fixture.device.to_string());
    let (from_text, to_text) = (cells.0.to_string(), cells.1.to_string());
    let mut params = fixture.params("temp", &tenant, &device).to_vec();
    params.extend([("tz", tz), ("from", &from_text), ("to", &to_text)]);
    let expected = lines(&fixture.stack.reference(reference, &params).await);
    let points = series["points"].as_array().expect("points");
    let what = format!("{tz} {interval:?} {}..{}", rfc3339(from), rfc3339(to));
    assert!(!expected.is_empty(), "{what}: the reference has buckets");
    assert_eq!(points.len(), expected.len(), "{what}: buckets\n{series}");
    for (point, row) in points.iter().zip(&expected) {
        let start = ms(&point["time"]);
        assert_eq!(Value::from(start), row[0], "{what}: bucket start");
        assert_eq!(point["count"], row[1], "{what}: count at {start}");
        close(&point["min"], &row[2], "min");
        close(&point["max"], &row[3], "max");
        close(&point["avg"], &row[4], "avg");
        assert_eq!(point["last"]["type"], "float");
        close(&point["last"]["value"], &row[5], "last");
        if let Some((offset_ms, length_ms)) = alignment {
            assert_eq!(
                (start + offset_ms).rem_euclid(length_ms),
                0,
                "{what}: bucket {start} does not start on a local boundary"
            );
        }
    }
    let readings: u64 = points.iter().filter_map(|p| p["count"].as_u64()).sum();
    eprintln!(
        "series {what}: interval {}, {} buckets, {readings} readings, equal to the reference \
         over [{}, {})",
        series["interval"],
        points.len(),
        rfc3339(cells.0),
        rfc3339(cells.1)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn history_matches_clickhouse_within_the_queryable_period() {
    let Some(stack) = Stack::start().await else {
        return;
    };
    let issuer = TestIssuer::new();
    let service = Service::start(
        &stack.product,
        &ClickHouseTarget {
            url: &stack.clickhouse.url,
            password: &stack.service_password,
        },
        &issuer.jwks(),
    )
    .await;
    let tenant = stack.create_tenant(&issuer, "Plant A").await;
    let other = stack.create_tenant(&issuer, "Plant B").await;
    let token = issuer.tenant_token(ServiceName::TelemetryQuery, tenant, TenantRole::Viewer);
    let other_token = issuer.tenant_token(ServiceName::TelemetryQuery, other, TenantRole::Viewer);
    wait_account_ready(&service, &token, tenant).await;
    wait_account_ready(&service, &other_token, other).await;
    let device = uuid::Uuid::new_v4();
    let fixture = Fixture {
        stack,
        service,
        issuer,
        tenant,
        token,
        device,
    };
    let stack = &fixture.stack;

    // Readings of the last 50 hours, of each type, at uneven steps; older ones before the raw
    // period and before the aggregated period; a dense point for the raw limit; and the same
    // device ID in tenant B, which tenant A must never see.
    let now = Utc::now().timestamp_millis();
    let recent = now - 50 * HOUR_MS;
    let insert = |point, kind, start_ms, step_ms, count, tenant| Readings {
        tenant,
        device,
        point,
        kind,
        start_ms,
        step_ms,
        count,
        marker: "reading",
    };
    for readings in [
        insert("temp", "float", recent, 433_000, 415, tenant),
        insert("count", "int", recent, 600_000, 300, tenant),
        insert("on", "bool", recent, 900_000, 200, tenant),
        insert("label", "string", recent, 1_800_000, 100, tenant),
        insert("temp", "float", now - 9 * DAY_MS, 600_000, 12, tenant),
        insert("temp", "float", now - 120 * DAY_MS, 600_000, 5, tenant),
        insert("dense", "float", now - 3 * HOUR_MS, 1_000, 10_001, tenant),
        insert("temp", "float", recent, 300_000, 77, other),
    ] {
        stack.insert(&readings).await;
    }

    // 1. Acceptance 4: downsampled series equal the reference, in several time zones and
    //    intervals, with buckets on local boundaries. The range lies on UTC hours, so on the
    //    cells of both aggregates; Asia/Kolkata's hours are not UTC hours, so its hour-based
    //    intervals read the one-minute cells.
    let hour = now - now.rem_euclid(HOUR_MS);
    let range = (hour - 49 * HOUR_MS, hour - HOUR_MS);
    let taipei = 8 * HOUR_MS;
    let kolkata = 5 * HOUR_MS + 30 * 60_000;
    for (reference, tz, interval, alignment) in [
        (REF_HOUR, "Asia/Taipei", Some("1h"), Some((taipei, HOUR_MS))),
        (
            REF_3_HOURS,
            "Asia/Kolkata",
            Some("3h"),
            Some((kolkata, 3 * HOUR_MS)),
        ),
        (REF_DAY, "Asia/Taipei", Some("1d"), Some((taipei, DAY_MS))),
        (REF_DAY, "UTC", Some("1d"), Some((0, DAY_MS))),
        (
            REF_15_MINUTES,
            "Asia/Kolkata",
            Some("15m"),
            Some((kolkata, 15 * 60_000)),
        ),
        // No interval: the finest with at most 1500 buckets over 48 hours is 5 minutes.
        (REF_5_MINUTES, "Asia/Taipei", None, None),
    ] {
        assert_float_series(&fixture, reference, tz, interval, range, range, alignment).await;
    }
    // Off the cell boundaries: the cells that start in the range, so both ends move up to the
    // next minute (one-minute cells: minute intervals, and Asia/Kolkata's hours) or the next UTC
    // hour (one-hour cells).
    let odd = (range.0 + 61_234, range.1 - 1_500);
    let minutes = (ceil(odd.0, MINUTE_MS), ceil(odd.1, MINUTE_MS));
    let hours = (ceil(odd.0, HOUR_MS), ceil(odd.1, HOUR_MS));
    assert_eq!(hours, (range.0 + HOUR_MS, range.1));
    for (reference, tz, interval, cells, alignment) in [
        (REF_HOUR, "UTC", "1h", hours, (0, HOUR_MS)),
        (REF_HOUR, "Asia/Taipei", "1h", hours, (taipei, HOUR_MS)),
        (REF_HOUR, "Asia/Kolkata", "1h", minutes, (kolkata, HOUR_MS)),
        (REF_15_MINUTES, "UTC", "15m", minutes, (0, 15 * 60_000)),
    ] {
        assert_float_series(
            &fixture,
            reference,
            tz,
            Some(interval),
            odd,
            cells,
            Some(alignment),
        )
        .await;
    }
    let auto = fixture
        .series(
            &fixture.token,
            &format!(
                "point=temp&from={}&to={}&tz=Asia/Taipei",
                rfc3339(range.0),
                rfc3339(range.1)
            ),
        )
        .await;
    assert_eq!(auto["interval"], "5m");

    // An integer point, and the types of bool and text points.
    let (tenant_text, device_text) = (tenant.to_string(), device.to_string());
    let (from_text, to_text) = (range.0.to_string(), range.1.to_string());
    let series = fixture
        .series(
            &fixture.token,
            &format!(
                "point=count&from={}&to={}&tz=Asia/Taipei&interval=1h",
                rfc3339(range.0),
                rfc3339(range.1)
            ),
        )
        .await;
    let mut params = fixture.params("count", &tenant_text, &device_text).to_vec();
    params.extend([
        ("tz", "Asia/Taipei"),
        ("from", &from_text),
        ("to", &to_text),
    ]);
    let expected = lines(&stack.reference(REF_HOUR_INT, &params).await);
    let points = series["points"].as_array().expect("points");
    assert_eq!(points.len(), expected.len(), "{series}");
    for (point, row) in points.iter().zip(&expected) {
        assert_eq!(Value::from(ms(&point["time"])), row[0]);
        assert_eq!(point["count"], row[1]);
        close(&point["min"], &row[2], "int min");
        close(&point["max"], &row[3], "int max");
        close(&point["avg"], &row[4], "int avg");
        assert_eq!(point["last"]["type"], "int");
        assert_eq!(point["last"]["value"], row[5]);
    }
    for (point, kind) in [("on", "bool"), ("label", "string")] {
        let series = fixture
            .series(
                &fixture.token,
                &format!(
                    "point={point}&from={}&to={}&tz=UTC&interval=1d",
                    rfc3339(range.0),
                    rfc3339(range.1)
                ),
            )
            .await;
        let total: u64 = series["points"]
            .as_array()
            .expect("points")
            .iter()
            .map(|bucket| {
                assert_eq!(bucket["last"]["type"], kind, "{series}");
                bucket["count"].as_u64().expect("a count")
            })
            .sum();
        let mut params = fixture.params(point, &tenant_text, &device_text).to_vec();
        params.extend([("from", from_text.as_str()), ("to", to_text.as_str())]);
        let expected = stack.reference(REF_COUNT, &params).await;
        assert_eq!(format!("{total}\n"), expected, "{point}");
        if kind == "string" {
            assert!(series["points"][0]["min"].is_null(), "{series}");
        }
    }

    // Raw readings equal the stored ones.
    let raw_range = (now - 3 * HOUR_MS, now - HOUR_MS);
    let series = fixture
        .series(
            &fixture.token,
            &format!(
                "point=temp&from={}&to={}&tz=UTC&interval=raw",
                rfc3339(raw_range.0),
                rfc3339(raw_range.1)
            ),
        )
        .await;
    let (raw_from, raw_to) = (raw_range.0.to_string(), raw_range.1.to_string());
    let mut params = fixture.params("temp", &tenant_text, &device_text).to_vec();
    params.extend([("from", raw_from.as_str()), ("to", raw_to.as_str())]);
    let expected = lines(&stack.reference(REF_RAW, &params).await);
    let points = series["points"].as_array().expect("points");
    assert_eq!(points.len(), expected.len(), "{series}");
    for (point, row) in points.iter().zip(&expected) {
        assert_eq!(Value::from(ms(&point["time"])), row[0]);
        assert_eq!(point["count"], 1);
        close(&point["last"]["value"], &row[1], "raw value");
        close(&point["min"], &row[1], "raw min");
    }

    // Latest values equal the stored ones.
    let latest = fixture
        .service
        .get_ok(
            &format!("/api/telemetry/v1/devices/{device}/latest"),
            &fixture.token,
        )
        .await;
    let since = ms(&latest["earliest"]).to_string();
    let expected = lines(
        &stack
            .reference(
                REF_LATEST,
                &[
                    ("tenant", &tenant_text),
                    ("device", &device_text),
                    ("from", &since),
                ],
            )
            .await,
    );
    let points = latest["points"].as_array().expect("points");
    assert_eq!(points.len(), expected.len(), "{latest}");
    for (point, row) in points.iter().zip(&expected) {
        assert_eq!(point["point"], row[0]);
        assert_eq!(Value::from(ms(&point["time"])), row[1]);
        assert_eq!(point["value"]["type"], row[2]);
        let column = match row[2].as_str() {
            Some("float") => 3,
            Some("int") => 4,
            Some("bool") => 5,
            _ => 6,
        };
        assert_eq!(point["value"]["value"], row[column], "{point}");
    }

    // Tenant B, the same device ID: only B's readings, and A's never.
    let full = format!(
        "point=temp&from={}&to={}&tz=UTC&interval=1d",
        rfc3339(recent - HOUR_MS),
        rfc3339(now + HOUR_MS)
    );
    let count = |series: &Value| -> u64 {
        series["points"]
            .as_array()
            .expect("points")
            .iter()
            .map(|bucket| bucket["count"].as_u64().expect("a count"))
            .sum()
    };
    let other_series = fixture.series(&other_token, &full).await;
    assert_eq!(count(&other_series), 77, "{other_series}");
    let own_series = fixture.series(&fixture.token, &full).await;
    assert_eq!(count(&own_series), 415, "{own_series}");

    // 2. Acceptance 3: the queryable period.
    let before = Utc::now();
    let series = fixture
        .series(
            &fixture.token,
            &format!(
                "point=temp&from={}&to={}&tz=UTC&interval=raw",
                rfc3339(now - 10 * DAY_MS),
                rfc3339(now - 6 * DAY_MS)
            ),
        )
        .await;
    let after = Utc::now();
    assert_eq!(series["truncated"], true, "{series}");
    let earliest = ms(&series["earliest"]);
    assert_eq!(ms(&series["from"]), earliest);
    // Seven days back plus the margin, rounded up to a whole millisecond.
    assert!(
        (before - TimeDelta::days(7)).timestamp_millis() + MARGIN_MS <= earliest
            && earliest <= (after - TimeDelta::days(7)).timestamp_millis() + MARGIN_MS + 1,
        "raw period starts seven days and five minutes back: {series}"
    );
    assert_eq!(series["points"], serde_json::json!([]), "{series}");
    eprintln!(
        "period raw 10..6 days back: truncated to {}, {} points",
        series["earliest"],
        count(&series)
    );
    let (nine_from, nine_to) = (
        (now - 10 * DAY_MS).to_string(),
        (now - 8 * DAY_MS).to_string(),
    );
    let mut params = fixture.params("temp", &tenant_text, &device_text).to_vec();
    params.extend([("from", nine_from.as_str()), ("to", nine_to.as_str())]);
    assert_eq!(
        stack.reference(REF_COUNT, &params).await,
        "12\n",
        "the readings nine days back are stored, but not served raw"
    );
    let (status, body) = fixture
        .status(
            Some(fixture.token.as_str()),
            &format!(
                "/api/telemetry/v1/devices/{device}/series?point=temp&from={}&to={}&tz=UTC&interval=raw",
                rfc3339(now - 10 * DAY_MS),
                rfc3339(now - 8 * DAY_MS)
            ),
        )
        .await;
    assert_eq!(status, 403, "{body}");
    assert_eq!(body["code"], "quota_exceeded");
    assert_eq!(body["message_key"], "telemetry.outside_queryable_period");
    assert!(body["details"]["earliest"].is_string(), "{body}");
    // The nine-day-old readings are within the aggregated period.
    let series = fixture
        .series(
            &fixture.token,
            &format!(
                "point=temp&from={}&to={}&tz=UTC&interval=1d",
                rfc3339(now - 10 * DAY_MS),
                rfc3339(now - 8 * DAY_MS)
            ),
        )
        .await;
    assert_eq!(count(&series), 12, "{series}");
    // Aggregated: three months back; the 120-day-old readings are stored but never served.
    let series = fixture
        .series(
            &fixture.token,
            &format!(
                "point=temp&from={}&to={}&tz=UTC&interval=1d",
                rfc3339(now - 200 * DAY_MS),
                rfc3339(now + HOUR_MS)
            ),
        )
        .await;
    assert_eq!(series["truncated"], true, "{series}");
    assert_eq!(count(&series), 415 + 12, "{series}");
    let earliest = ms(&series["earliest"]);
    assert!(
        earliest > now - 93 * DAY_MS && earliest < now - 88 * DAY_MS,
        "three calendar months back: {series}"
    );
    assert_eq!(
        earliest.rem_euclid(HOUR_MS),
        0,
        "a day interval reads the one-hour cells, whose clamp starts on a whole UTC hour: {series}"
    );
    eprintln!(
        "period 1d 200 days back..now: truncated to {}, {} readings (120-day-old readings left out)",
        series["earliest"],
        count(&series)
    );
    let (old_from, old_to) = (
        (now - 121 * DAY_MS).to_string(),
        (now - 119 * DAY_MS).to_string(),
    );
    let mut params = fixture.params("temp", &tenant_text, &device_text).to_vec();
    params.extend([("from", old_from.as_str()), ("to", old_to.as_str())]);
    assert_eq!(stack.reference(REF_COUNT, &params).await, "5\n");
    let (status, body) = fixture
        .status(
            Some(fixture.token.as_str()),
            &format!(
                "/api/telemetry/v1/devices/{device}/series?point=temp&from={}&to={}&tz=UTC",
                rfc3339(now - 200 * DAY_MS),
                rfc3339(now - 100 * DAY_MS)
            ),
        )
        .await;
    assert_eq!(status, 403, "{body}");
    assert_eq!(body["message_key"], "telemetry.outside_queryable_period");
    // Without an interval, a truncated three-month range gets 3-hour buckets.
    let series = fixture
        .series(
            &fixture.token,
            &format!(
                "point=temp&from={}&to={}&tz=UTC",
                rfc3339(now - 200 * DAY_MS),
                rfc3339(now)
            ),
        )
        .await;
    assert_eq!(series["interval"], "3h", "{series}");
    assert_eq!(series["truncated"], true);

    // 3. Refusals.
    let base = format!("/api/telemetry/v1/devices/{device}/series");
    let good = format!(
        "from={}&to={}",
        rfc3339(now - 2 * HOUR_MS),
        rfc3339(now - HOUR_MS)
    );
    let no_tenant = fixture
        .issuer
        .token(&TestIssuer::claims(ServiceName::TelemetryQuery, None));
    let unknown = fixture.issuer.tenant_token(
        ServiceName::TelemetryQuery,
        TenantId::generate(),
        TenantRole::Viewer,
    );
    let for_identity =
        fixture
            .issuer
            .tenant_token(ServiceName::Identity, tenant, TenantRole::Viewer);
    let cases: Vec<RefusalCase<'_>> = vec![
        (
            None,
            format!("{base}?point=temp&{good}&tz=UTC"),
            401,
            "unauthenticated",
            None,
        ),
        (
            Some(for_identity.as_str()),
            format!("{base}?point=temp&{good}&tz=UTC"),
            401,
            "unauthenticated",
            None,
        ),
        (
            Some(no_tenant.as_str()),
            format!("{base}?point=temp&{good}&tz=UTC"),
            403,
            "permission_denied",
            None,
        ),
        (
            Some(unknown.as_str()),
            format!("{base}?point=temp&{good}&tz=UTC"),
            404,
            "not_found",
            Some("telemetry.account_not_ready"),
        ),
        (
            Some(fixture.token.as_str()),
            format!("/api/telemetry/v1/devices/not-a-device/series?point=temp&{good}&tz=UTC"),
            400,
            "invalid_request",
            Some("telemetry.device_id_invalid"),
        ),
        (
            Some(fixture.token.as_str()),
            format!("{base}?point=temp&{good}"),
            400,
            "invalid_request",
            Some("telemetry.query_invalid"),
        ),
        (
            Some(fixture.token.as_str()),
            format!("{base}?point=temp&{good}&tz=UTC&interval=2h"),
            400,
            "invalid_request",
            Some("telemetry.query_invalid"),
        ),
        (
            Some(fixture.token.as_str()),
            format!("{base}?point=temp&from=yesterday&to=today&tz=UTC"),
            400,
            "invalid_request",
            Some("telemetry.query_invalid"),
        ),
        (
            Some(fixture.token.as_str()),
            format!("{base}?point=temp&{good}&tz=..%2Fetc"),
            422,
            "validation_failed",
            Some("telemetry.time_zone_invalid"),
        ),
        (
            Some(fixture.token.as_str()),
            format!("{base}?point=temp&{good}&tz=Nowhere%2FZone"),
            422,
            "validation_failed",
            Some("telemetry.time_zone_invalid"),
        ),
        (
            Some(fixture.token.as_str()),
            format!("{base}?point=temp&{good}&tz=Nowhere%2FZone&interval=raw"),
            422,
            "validation_failed",
            Some("telemetry.time_zone_invalid"),
        ),
        (
            Some(fixture.token.as_str()),
            format!("{base}?point=&{good}&tz=UTC"),
            422,
            "validation_failed",
            Some("telemetry.point_invalid"),
        ),
        (
            Some(fixture.token.as_str()),
            format!(
                "{base}?point=temp&from={}&to={}&tz=UTC",
                rfc3339(now - HOUR_MS),
                rfc3339(now - 2 * HOUR_MS)
            ),
            422,
            "validation_failed",
            Some("telemetry.range_invalid"),
        ),
        // chrono reads years up to +262142, and the cells' DateTime ends in 2106: a range may
        // end at most 367 days after now (`semantic::range::MAX_AHEAD`).
        (
            Some(fixture.token.as_str()),
            format!(
                "{base}?point=temp&from={}&to=%2B262142-12-31T23:59:00Z&tz=UTC",
                rfc3339(now - HOUR_MS)
            ),
            422,
            "validation_failed",
            Some("telemetry.range_invalid"),
        ),
        (
            Some(fixture.token.as_str()),
            format!(
                "{base}?point=temp&from={}&to=2106-02-08T00:00:00Z&tz=UTC&interval=1h",
                rfc3339(now - HOUR_MS)
            ),
            422,
            "validation_failed",
            Some("telemetry.range_invalid"),
        ),
        (
            Some(fixture.token.as_str()),
            format!(
                "{base}?point=temp&from={}&to={}&tz=UTC&interval=1m",
                rfc3339(now - 30 * DAY_MS),
                rfc3339(now)
            ),
            422,
            "validation_failed",
            Some("telemetry.too_many_points"),
        ),
        (
            Some(fixture.token.as_str()),
            format!(
                "{base}?point=dense&from={}&to={}&tz=UTC&interval=raw",
                rfc3339(now - 4 * HOUR_MS),
                rfc3339(now)
            ),
            422,
            "validation_failed",
            Some("telemetry.too_many_points"),
        ),
        (
            Some(fixture.token.as_str()),
            "/api/telemetry/v1/devices/not-a-device/latest".to_owned(),
            400,
            "invalid_request",
            Some("telemetry.device_id_invalid"),
        ),
        (
            Some(no_tenant.as_str()),
            format!("/api/telemetry/v1/devices/{device}/latest"),
            403,
            "permission_denied",
            None,
        ),
    ];
    for (token, path, status, code, key) in cases {
        let (actual, body) = fixture.status(token, &path).await;
        assert_eq!(actual, status, "{path}: {body}");
        assert_eq!(body["code"], code, "{path}: {body}");
        if let Some(key) = key {
            assert_eq!(body["message_key"], key, "{path}: {body}");
        }
        assert!(
            !body.to_string().contains("DB::Exception"),
            "{path}: ClickHouse's error text reached the client: {body}"
        );
        eprintln!(
            "refusal {actual} {code} {}: {}",
            body["message_key"],
            path.replace(&device_text, "<device>")
        );
    }

    // 4. The tenant's query quota (120 queries a minute, 0004_access.sql): once used up, the API
    //    answers 429 until the next interval. Tenant B, so A's checks above are not affected; a
    //    minute boundary in between resets the count once, hence up to four times the limit.
    let latest = format!("/api/telemetry/v1/devices/{device}/latest");
    let mut limited = None;
    for call in 0..480 {
        let (status, body) = fixture.status(Some(other_token.as_str()), &latest).await;
        if status == 429 {
            limited = Some((call, body));
            break;
        }
        assert_eq!(status, 200, "call {call}: {body}");
    }
    let (call, body) = limited.expect("the quota ran out within 480 queries");
    assert!(
        call >= 100,
        "the quota allows about 120 queries a minute, ran out after {call}"
    );
    assert_eq!(body["code"], "rate_limited", "{body}");
    eprintln!("quota: call {call} answered 429 rate_limited");
}
