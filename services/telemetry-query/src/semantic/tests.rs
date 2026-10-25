//! Unit tests of the semantic API's checks, of the cumulative check's rule, and of how answers
//! are assembled from ClickHouse's rows. The statements themselves, with the step, gap and
//! suspicious-step rules they carry, run against ClickHouse in tests/semantic.rs.

use serde_json::json;

use super::*;

fn at(text: &str) -> DateTime<Utc> {
    text.parse().expect("an RFC 3339 time")
}

const DEVICE: &str = "0192f6e0-7a3c-7d4e-8f00-0123456789ab";

fn key(point: &str) -> SeriesKey {
    SeriesKey {
        device_id: Uuid::parse_str(DEVICE).expect("a UUID"),
        point: point.to_owned(),
    }
}

fn query(points: &[&str], interval: Interval) -> SeriesQuery {
    SeriesQuery {
        series: points.iter().map(|point| key(point)).collect(),
        tz: "Asia/Taipei".to_owned(),
        period: Some("last_7_days".to_owned()),
        from: None,
        to: None,
        interval,
    }
}

const NOW: &str = "2026-10-26T10:17:42Z";

fn status(result: Result<Checked, Box<Response>>) -> u16 {
    match result {
        Ok(_) => panic!("accepted"),
        Err(response) => response.status().as_u16(),
    }
}

#[test]
fn the_cumulative_check_counts_drops_against_rises() {
    // At most two drops in the window: a reset and a wrap, however short the window.
    assert_eq!(verdict(0, 0, 40), Verdict::Cumulative);
    assert_eq!(verdict(2, 3, 40), Verdict::Cumulative);
    // Three or more drops: at most 2 % of the rises.
    assert_eq!(verdict(3, 150, 400), Verdict::Cumulative);
    assert_eq!(verdict(3, 149, 400), Verdict::NotCumulative);
    assert_eq!(verdict(4, 200, 400), Verdict::Cumulative);
    assert_eq!(verdict(5, 200, 400), Verdict::NotCumulative);
    // Flat comparisons do not dilute the drops: a gauge that idles at 0 with a few bursts a
    // day rises about as often as it falls.
    assert_eq!(verdict(4, 4, 1_440), Verdict::NotCumulative);
    // The fixture's gauges: 26 drops against 21 rises in 47 comparisons, and the hour table's
    // 6 drops in 7.
    assert_eq!(verdict(26, 21, 47), Verdict::NotCumulative);
    assert_eq!(verdict(6, 1, 7), Verdict::NotCumulative);
    // A counter needs ten comparisons to be confirmed.
    assert_eq!(verdict(0, 9, 9), Verdict::TooFewReadings);
    assert_eq!(verdict(1, 8, 9), Verdict::TooFewReadings);
    assert_eq!(verdict(0, 10, 10), Verdict::Cumulative);
    // Too few comparisons do not save a series that falls too often.
    assert_eq!(verdict(4, 4, 9), Verdict::NotCumulative);
}

#[test]
fn point_keys_are_letters_digits_dots_underscores_and_hyphens() {
    for good in ["energy_kwh", "a", "Phase-1.voltage", &"p".repeat(128)] {
        assert!(is_point_key(good), "{good}");
    }
    for bad in [
        "",
        "_energy",
        ".x",
        "energy kwh",
        "x') OR 1=1 --",
        "x'",
        "x\\",
        "能量",
        &"p".repeat(129),
    ] {
        assert!(!is_point_key(bad), "{bad}");
    }
}

#[test]
fn the_series_parameter_is_an_escaped_array_of_tuples() {
    let series = [key("energy_kwh"), key("it's\\")];
    assert_eq!(
        series_parameter(&series),
        format!("[('{DEVICE}', 'energy_kwh'), ('{DEVICE}', 'it\\'s\\\\')]")
    );
}

#[test]
fn requests_are_checked_before_anything_is_queried() {
    let now = at(NOW);
    assert!(check(&query(&["energy_kwh"], Interval::Day1), Kind::Usage, now).is_ok());
    // The series: 1 to 20, distinct, with valid keys.
    assert_eq!(
        status(check(&query(&[], Interval::Total), Kind::Usage, now)),
        422
    );
    let many: Vec<String> = (0..21).map(|n| format!("p{n}")).collect();
    let many: Vec<&str> = many.iter().map(String::as_str).collect();
    assert_eq!(
        status(check(&query(&many, Interval::Total), Kind::Usage, now)),
        422
    );
    assert!(check(&query(&many[..20], Interval::Total), Kind::Usage, now).is_ok());
    assert_eq!(
        status(check(
            &query(&["a", "a"], Interval::Total),
            Kind::Usage,
            now
        )),
        422
    );
    assert_eq!(
        status(check(
            &query(&["x' OR 1=1"], Interval::Total),
            Kind::Usage,
            now
        )),
        422
    );
    // The time zone: a name of the database compiled in.
    for tz in ["Asia/Taipei') --", "asia/taipei", "", "Mars/Olympus"] {
        let mut bad = query(&["a"], Interval::Total);
        bad.tz = tz.to_owned();
        assert_eq!(status(check(&bad, Kind::Usage, now)), 422, "{tz}");
    }
    // The period and the range.
    let mut bad = query(&["a"], Interval::Total);
    bad.period = Some("last_7_days'".to_owned());
    assert_eq!(status(check(&bad, Kind::Usage, now)), 422);
    bad.period = None;
    bad.from = Some(at("2026-10-02T00:00:00Z"));
    bad.to = Some(at("2026-10-01T00:00:00Z"));
    assert_eq!(status(check(&bad, Kind::Usage, now)), 422);
    // Wholly before the queryable period: 403 with its start.
    bad.from = Some(at("2026-01-01T00:00:00Z"));
    bad.to = Some(at("2026-02-01T00:00:00Z"));
    let refused = check(&bad, Kind::Usage, now).err().expect("refused");
    assert_eq!(refused.status().as_u16(), 403);
}

#[test]
fn a_body_with_another_field_is_not_a_request() {
    let body = json!({
        "series": [{"device_id": DEVICE, "point": "energy_kwh"}],
        "tz": "Asia/Taipei",
        "period": "today",
    });
    let parsed: SeriesQuery = serde_json::from_value(body.clone()).expect("a request");
    assert_eq!(parsed.interval, Interval::Total);
    for (field, value) in [
        ("tenant_id", json!("0192f6e0-7a3c-7d4e-8f00-000000000001")),
        ("user_id", json!("x")),
        ("sql", json!("SELECT 1")),
    ] {
        let mut extra = body.clone();
        extra[field] = value;
        assert!(
            serde_json::from_value::<SeriesQuery>(extra).is_err(),
            "{field}"
        );
    }
    let mut extra = body;
    extra["series"][0]["tenant_id"] = json!("x");
    assert!(serde_json::from_value::<SeriesQuery>(extra).is_err());
}

#[test]
fn times_are_written_with_the_offset_of_the_time_zone() {
    let taipei = range::time_zone("Asia/Taipei").expect("a time zone");
    assert_eq!(
        local(at("2026-01-05T22:00:00Z"), &taipei).to_rfc3339(),
        "2026-01-06T06:00:00+08:00"
    );
    let new_york = range::time_zone("America/New_York").expect("a time zone");
    assert_eq!(
        local(at("2026-07-01T12:00:00Z"), &new_york).to_rfc3339(),
        "2026-07-01T08:00:00-04:00"
    );
    assert_eq!(
        local(at("2026-12-01T12:00:00Z"), &new_york).to_rfc3339(),
        "2026-12-01T07:00:00-05:00"
    );
}

/// A checked usage request over 2026-10-20 in UTC, hourly.
fn checked(interval: Interval) -> (SeriesQuery, Checked) {
    let mut query = query(&["energy_kwh"], interval);
    query.tz = "UTC".to_owned();
    query.period = None;
    query.from = Some(at("2026-10-20T00:00:00Z"));
    query.to = Some(at("2026-10-21T00:00:00Z"));
    let checked = check(&query, Kind::Usage, at(NOW)).expect("a valid request");
    (query, checked)
}

fn seconds(text: &str) -> i64 {
    at(text).timestamp()
}

fn millis(text: &str) -> i64 {
    at(text).timestamp_millis()
}

/// A usage row of `energy_kwh` for the bucket at `bucket`, with nothing unusual.
fn cell(bucket: &str, usage: f64) -> UsageCells {
    UsageCells {
        device: DEVICE.to_owned(),
        point: "energy_kwh".to_owned(),
        bucket: seconds(bucket),
        usage: Some(usage),
        readings: 12,
        drops: 0,
        skipped: 0,
        non_numeric: 0,
        usable: 12,
        gaps: 0,
        longest_gap_ms: 0,
        longest_gap_from: 0,
        suspicious: 0,
        inside_suspicious: Some(0.0),
        inside_suspicious_at: 0,
        across_suspicious: Some(0.0),
        across_suspicious_at: 0,
        first_at: millis(bucket),
        first: Some(100.0),
        last_at: millis(bucket) + 3_300_000,
        last: Some(100.0 + usage),
        check_drops: 1,
        check_rises: 60,
        check_comparisons: 70,
        window_first: seconds("2026-10-19T00:00:00Z"),
    }
}

#[test]
fn usage_rows_add_up_and_carry_the_series_figures() {
    let (query, checked) = checked(Interval::Hour1);
    let mut cells = vec![
        cell("2026-10-20T00:00:00Z", 5.0),
        cell("2026-10-20T01:00:00Z", 7.5),
        cell("2026-10-20T03:00:00Z", 2.5),
    ];
    cells[1].drops = 1;
    cells[2].gaps = 1;
    cells[2].longest_gap_from = millis("2026-10-20T01:55:00Z");
    cells[2].longest_gap_ms = 3_900_000;
    cells[2].suspicious = 1;
    cells[2].across_suspicious = Some(2.0);
    cells[2].across_suspicious_at = millis("2026-10-20T03:00:00Z");
    let answer = usage(&query, &checked, cells).expect("an answer");
    assert_eq!(answer.range.resolution, Resolution::Hour);
    assert_eq!(answer.range.from.to_rfc3339(), "2026-10-20T00:00:00+00:00");
    let [series] = answer.series.as_slice() else {
        panic!("one series");
    };
    assert_eq!(series.status, UsageStatus::Ok);
    assert_eq!(series.total, Some(15.0));
    assert_eq!(series.drops, 1);
    assert_eq!(series.readings, 36);
    assert!(series.baseline, "the window starts a day before the range");
    let check = series.check.as_ref().expect("a check");
    assert_eq!(check.verdict, Verdict::Cumulative);
    assert_eq!(check.from.to_rfc3339(), "2026-10-19T00:00:00+00:00");
    assert_eq!(check.to.to_rfc3339(), "2026-10-22T00:00:00+00:00");
    assert_eq!(series.gaps, 1);
    let gap = series.longest_gap.as_ref().expect("a gap");
    assert_eq!(gap.from.to_rfc3339(), "2026-10-20T01:55:00+00:00");
    assert_eq!(gap.to.to_rfc3339(), "2026-10-20T03:00:00+00:00");
    assert_eq!(series.suspicious_steps, 1);
    assert_eq!(
        series.largest_suspicious_step,
        Some(TimedValue {
            time: local(at("2026-10-20T03:00:00Z"), &checked.tz),
            value: 2.0
        })
    );
    assert_eq!(series.rows.len(), 3);
    assert!(series.rows[2].after_gap && !series.rows[1].after_gap);
    assert_eq!(series.highest.as_ref().map(|h| h.value), Some(7.5));
    assert_eq!(series.lowest.as_ref().map(|l| l.value), Some(2.5));
    assert_eq!(
        series.first_reading.as_ref().map(|f| f.time.to_rfc3339()),
        Some("2026-10-20T00:00:00+00:00".to_owned())
    );
    assert_eq!(series.last_reading.as_ref().map(|l| l.value), Some(102.5));
}

#[test]
fn a_series_that_is_not_a_counter_or_not_numeric_or_empty_gets_no_usage() {
    let (query, checked) = checked(Interval::Total);
    // No rows: no data.
    let answer = usage(&query, &checked, Vec::new()).expect("an answer");
    assert_eq!(answer.series[0].status, UsageStatus::NoData);
    assert_eq!(answer.series[0].total, None);
    assert!(answer.series[0].check.is_none());
    // A gauge.
    let mut gauge = cell("2026-10-20T00:00:00Z", 812.0);
    (
        gauge.check_drops,
        gauge.check_rises,
        gauge.check_comparisons,
    ) = (26, 21, 47);
    let answer = usage(&query, &checked, vec![gauge]).expect("an answer");
    let series = &answer.series[0];
    assert_eq!(series.status, UsageStatus::NotCumulative);
    assert_eq!(series.total, None);
    assert!(series.rows.is_empty());
    assert_eq!(series.check.as_ref().map(|c| c.drops), Some(26));
    // On/off or text.
    let mut state = cell("2026-10-20T00:00:00Z", 0.0);
    state.non_numeric = 12;
    state.usable = 0;
    let answer = usage(&query, &checked, vec![state]).expect("an answer");
    assert_eq!(answer.series[0].status, UsageStatus::NotNumeric);
    // Too few readings: computed, flagged in the check.
    let mut young = cell("2026-10-20T00:00:00Z", 3.0);
    (
        young.check_drops,
        young.check_rises,
        young.check_comparisons,
    ) = (0, 4, 4);
    young.window_first = seconds("2026-10-20T00:00:00Z");
    let answer = usage(&query, &checked, vec![young]).expect("an answer");
    let series = &answer.series[0];
    assert_eq!(series.status, UsageStatus::Ok);
    assert_eq!(series.total, Some(3.0));
    assert_eq!(
        series.check.as_ref().map(|c| c.verdict),
        Some(Verdict::TooFewReadings)
    );
    assert!(!series.baseline, "nothing before the range");
    assert!(series.highest.is_none(), "no extremes for `total`");
}

#[test]
fn series_are_answered_in_the_requested_order_and_others_are_ignored() {
    let (mut query, checked) = checked(Interval::Total);
    query.series = vec![key("water_m3"), key("energy_kwh")];
    let mut stranger = cell("2026-10-20T00:00:00Z", 99.0);
    stranger.point = "not_asked".to_owned();
    let answer = usage(
        &query,
        &checked,
        vec![cell("2026-10-20T00:00:00Z", 4.0), stranger],
    )
    .expect("an answer");
    let points: Vec<(&str, UsageStatus)> = answer
        .series
        .iter()
        .map(|series| (series.point.as_str(), series.status))
        .collect();
    assert_eq!(
        points,
        [
            ("water_m3", UsageStatus::NoData),
            ("energy_kwh", UsageStatus::Ok)
        ]
    );
}

fn typed(at: &str, value: f64) -> (i64, String, Option<f64>, i64, bool, String) {
    (
        millis(at),
        "float".to_owned(),
        Some(value),
        0,
        false,
        String::new(),
    )
}

#[test]
fn statistics_combine_the_buckets() {
    let (query, checked) = checked(Interval::Hour1);
    let row = |bucket: &str, count: u64, sum: f64, min: f64, max: f64, last: f64| {
        StatisticsCells(
            DEVICE.to_owned(),
            "energy_kwh".to_owned(),
            seconds(bucket),
            count,
            count,
            Some(sum),
            Some(min),
            Some(seconds(bucket)),
            Some(max),
            Some(seconds(bucket)),
            typed(bucket, min),
            typed(bucket, last),
        )
    };
    let answer = statistics(
        &query,
        &checked,
        vec![
            row("2026-10-20T00:00:00Z", 2, 10.0, 4.0, 6.0, 6.0),
            row("2026-10-20T01:00:00Z", 2, 30.0, 14.0, 16.0, 14.0),
        ],
    )
    .expect("an answer");
    let series = &answer.series[0];
    assert_eq!(series.status, DataStatus::Ok);
    assert_eq!((series.count, series.numeric_count), (4, 4));
    assert_eq!(series.avg, Some(10.0));
    assert_eq!(series.min, Some(4.0));
    assert_eq!(series.max, Some(16.0));
    assert_eq!(
        series.max_time.map(|time| time.to_rfc3339()),
        Some("2026-10-20T01:00:00+00:00".to_owned())
    );
    assert_eq!(
        series.first.as_ref().map(|f| f.value.clone()),
        Some(Reading::Float(Some(4.0)))
    );
    assert_eq!(
        series.last.as_ref().map(|l| l.value.clone()),
        Some(Reading::Float(Some(14.0)))
    );
    assert_eq!(series.rows[1].avg, Some(15.0));
    assert_eq!(series.highest.as_ref().map(|h| h.value), Some(15.0));
    assert_eq!(series.lowest.as_ref().map(|l| l.value), Some(5.0));
    let empty = statistics(&query, &checked, Vec::new()).expect("an answer");
    assert_eq!(empty.series[0].status, DataStatus::NoData);
    assert_eq!(empty.series[0].avg, None);
}

#[test]
fn statistics_of_text_have_counts_and_readings_only() {
    // ClickHouse writes no minimum, maximum or their times for a series of text: null.
    let (query, checked) = checked(Interval::Total);
    let text = |at: &str| {
        (
            millis(at),
            "string".to_owned(),
            Some(0.0),
            0,
            false,
            "auto".to_owned(),
        )
    };
    let row = StatisticsCells(
        DEVICE.to_owned(),
        "energy_kwh".to_owned(),
        seconds("2026-10-20T00:00:00Z"),
        3,
        0,
        Some(0.0),
        None,
        None,
        None,
        None,
        text("2026-10-20T00:00:00Z"),
        text("2026-10-20T00:10:00Z"),
    );
    let answer = statistics(&query, &checked, vec![row]).expect("an answer");
    let series = &answer.series[0];
    assert_eq!(series.status, DataStatus::Ok);
    assert_eq!((series.count, series.numeric_count), (3, 0));
    assert_eq!((series.min, series.max, series.avg), (None, None, None));
    assert_eq!((series.min_time, series.max_time), (None, None));
    assert_eq!(series.rows[0].avg, None);
    assert_eq!(
        series.last.as_ref().map(|l| l.value.clone()),
        Some(Reading::String("auto".to_owned()))
    );
}

#[test]
fn a_bucket_of_text_leaves_the_average_of_the_numbers() {
    // A data point that changed type: numbers in one bucket, text (no sum) in the next.
    let (query, checked) = checked(Interval::Hour1);
    let text = |at: &str| {
        (
            millis(at),
            "string".to_owned(),
            None,
            0,
            false,
            "off".to_owned(),
        )
    };
    let numbers = StatisticsCells(
        DEVICE.to_owned(),
        "energy_kwh".to_owned(),
        seconds("2026-10-20T00:00:00Z"),
        2,
        2,
        Some(10.0),
        Some(4.0),
        Some(seconds("2026-10-20T00:00:00Z")),
        Some(6.0),
        Some(seconds("2026-10-20T00:00:00Z")),
        typed("2026-10-20T00:00:00Z", 4.0),
        typed("2026-10-20T00:10:00Z", 6.0),
    );
    let words = StatisticsCells(
        DEVICE.to_owned(),
        "energy_kwh".to_owned(),
        seconds("2026-10-20T01:00:00Z"),
        1,
        0,
        None,
        None,
        None,
        None,
        None,
        text("2026-10-20T01:00:00Z"),
        text("2026-10-20T01:00:00Z"),
    );
    let answer = statistics(&query, &checked, vec![numbers, words]).expect("an answer");
    let series = &answer.series[0];
    assert_eq!((series.count, series.numeric_count), (3, 2));
    assert_eq!(series.avg, Some(5.0));
    assert_eq!((series.rows[0].avg, series.rows[1].avg), (Some(5.0), None));
}

#[test]
fn every_statement_unit_pair_is_planned() {
    for resolution in [Resolution::Minute, Resolution::Hour] {
        for unit in [
            Unit::Minutes(5),
            Unit::Hour,
            Unit::Day,
            Unit::Week,
            Unit::Month,
            Unit::Total,
        ] {
            for kind in [Kind::Statistics, Kind::Usage] {
                let (statement, step) = statement(kind, resolution, unit);
                let usage = statement.contains("lagInFrame");
                assert_eq!(usage, kind == Kind::Usage);
                if matches!(unit, Unit::Minutes(_)) {
                    assert!(statement.contains("toIntervalMinute"));
                    assert_eq!(step, 5);
                    assert!(statement.contains("tenant_telemetry_1m\n"));
                }
            }
        }
    }
}

#[test]
fn the_spec_keeps_both_interval_sets_apart() {
    // utoipa keys schemas by name: a second `Interval` would replace the history API's.
    let spec = serde_json::to_value(crate::openapi()).expect("the spec as JSON");
    let schemas = &spec["components"]["schemas"];
    let values = |name: &str| -> Vec<String> {
        schemas[name]["enum"]
            .as_array()
            .unwrap_or_else(|| panic!("no enum schema {name}"))
            .iter()
            .map(|value| value.as_str().expect("a name").to_owned())
            .collect()
    };
    assert!(values("Interval").contains(&"raw".to_owned()));
    assert!(values("BucketInterval").contains(&"total".to_owned()));
    assert_eq!(
        spec["paths"][STATISTICS_PATH]["post"]["requestBody"]["content"]["application/json"]["schema"]
            ["$ref"],
        "#/components/schemas/SeriesQuery"
    );
}

#[tokio::test]
async fn a_usage_over_too_many_minute_cells_is_too_heavy() {
    // Twenty series for 60 local days of a half-hour time zone read the one-minute table:
    // 20 x 60 x 1440 cells, over the cap. The same statistics are 1200 rows, within theirs.
    let points: Vec<String> = (0..20).map(|n| format!("p{n}")).collect();
    let points: Vec<&str> = points.iter().map(String::as_str).collect();
    let mut heavy = query(&points, Interval::Day1);
    heavy.tz = "Asia/Kolkata".to_owned();
    heavy.period = Some("last_60_days".to_owned());
    assert!(check(&heavy, Kind::Statistics, at(NOW)).is_ok());
    let refused = check(&heavy, Kind::Usage, at(NOW)).err().expect("refused");
    assert_eq!(refused.status().as_u16(), 422);
    let body = http_body_util::BodyExt::collect(refused.into_body())
        .await
        .expect("the body")
        .to_bytes();
    let body: serde_json::Value = serde_json::from_slice(&body).expect("JSON");
    assert_eq!(body["message_key"], "telemetry.query_too_heavy");
    assert_eq!(body["details"]["parameter"], "series");
    // Ten series fit.
    heavy.series.truncate(10);
    assert!(check(&heavy, Kind::Usage, at(NOW)).is_ok());
}
