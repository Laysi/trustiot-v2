use super::*;

const DEVICE: &str = "0192f6e0-7a3c-7d4e-8f00-0123456789ab";

fn at(text: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(text)
        .expect("a time")
        .with_timezone(&Utc)
}

const NOW: &str = "2026-10-26T10:17:42Z";

fn t(text: &str) -> Time {
    DateTime::parse_from_rfc3339(text).expect("a time")
}

fn input(series: &[&str], extra: Value) -> Value {
    let mut input = json!({
        "series": series.iter().map(|point| json!({"device_id": DEVICE, "point": point})).collect::<Vec<_>>(),
        "tz": "Asia/Taipei",
    });
    for (key, value) in extra.as_object().expect("an object") {
        input[key] = value.clone();
    }
    input
}

fn refused(input: Value) -> String {
    match check(input, at(NOW)) {
        Err(ToolError::Input(message)) => message,
        other => panic!("not refused as input: {other:?}"),
    }
}

#[test]
fn a_checked_input_is_the_api_body() {
    let body = check(
        input(
            &["energy_kwh"],
            json!({"period": "this_month", "interval": "1d"}),
        ),
        at(NOW),
    )
    .expect("valid");
    assert_eq!(
        serde_json::to_value(&body).expect("JSON"),
        json!({
            "series": [{"device_id": DEVICE, "point": "energy_kwh"}],
            "tz": "Asia/Taipei",
            "period": "this_month",
            "interval": "1d",
        })
    );
    // `from` and `to` go on as written; the interval defaults to `total`.
    let body = check(
        input(
            &["energy_kwh"],
            json!({"from": "2026-10-01T00:00:00+08:00", "to": "2026-10-08T00:00:00+08:00"}),
        ),
        at(NOW),
    )
    .expect("valid");
    assert_eq!(body.from.as_deref(), Some("2026-10-01T00:00:00+08:00"));
    assert_eq!(body.interval, "total");
}

#[test]
fn malformed_input_gets_a_message_that_says_how_to_fix_it() {
    let many: Vec<String> = (0..21).map(|n| format!("p{n}")).collect();
    let many: Vec<&str> = many.iter().map(String::as_str).collect();
    let mut bad_device = input(&["energy_kwh"], json!({"period": "today"}));
    bad_device["series"][0]["device_id"] = json!("floor-3");
    let mut bad_tz = input(&["energy_kwh"], json!({"period": "today"}));
    bad_tz["tz"] = json!("");
    for (input, needle) in [
        (input(&[], json!({"period": "today"})), "1 to 20"),
        (input(&many, json!({"period": "today"})), "1 to 20"),
        (input(&["a", "a"], json!({"period": "today"})), "twice"),
        (bad_device, "UUID"),
        (
            input(&["x' OR 1=1"], json!({"period": "today"})),
            "data point key",
        ),
        (input(&["_x"], json!({"period": "today"})), "data point key"),
        (bad_tz, "IANA"),
        (input(&["a"], json!({})), "either `period`"),
        (
            input(
                &["a"],
                json!({"period": "today", "from": "2026-10-01T00:00:00Z"}),
            ),
            "either `period`",
        ),
        (
            input(&["a"], json!({"from": "2026-10-01T00:00:00Z"})),
            "either `period`",
        ),
        (
            input(&["a"], json!({"period": "last_0_days"})),
            "last_<N>_days",
        ),
        (
            input(&["a"], json!({"period": "last_367_days"})),
            "last_<N>_days",
        ),
        (
            input(&["a"], json!({"period": "last_07_days"})),
            "last_<N>_days",
        ),
        (
            input(&["a"], json!({"period": "last_73_hours"})),
            "last_<N>_days",
        ),
        (
            input(&["a"], json!({"period": "today' OR '1'='1"})),
            "last_<N>_days",
        ),
        (input(&["a"], json!({"period": "TODAY"})), "last_<N>_days"),
        (
            input(
                &["a"],
                json!({"from": "yesterday", "to": "2026-10-02T00:00:00Z"}),
            ),
            "RFC 3339",
        ),
        (
            input(
                &["a"],
                json!({"from": "2026-10-02T00:00:10Z", "to": "2026-10-02T00:00:50Z"}),
            ),
            "a minute before",
        ),
        (
            input(&["a"], json!({"period": "today", "interval": "2h"})),
            "one of 1m",
        ),
        (
            input(&["a"], json!({"period": "today", "tenant_id": DEVICE})),
            "nothing else",
        ),
        (
            input(&["a"], json!({"period": "today", "sql": "SELECT 1"})),
            "nothing else",
        ),
    ] {
        let message = refused(input.clone());
        assert!(
            message.contains(needle),
            "{input}: {message} lacks {needle}"
        );
    }
}

#[test]
fn the_rows_must_fit_the_service_limit() {
    // 20 series of one-hour buckets over a month: 20 x 748 rows.
    let points: Vec<String> = (0..20).map(|n| format!("p{n}")).collect();
    let points: Vec<&str> = points.iter().map(String::as_str).collect();
    let message = refused(input(
        &points,
        json!({"period": "this_month", "interval": "1h"}),
    ));
    assert!(message.contains("10000"), "{message}");
    assert!(
        check(
            input(
                &points[..10],
                json!({"period": "this_month", "interval": "1h"})
            ),
            at(NOW)
        )
        .is_ok()
    );
    assert!(
        check(
            input(&points, json!({"period": "this_year", "interval": "1d"})),
            at(NOW)
        )
        .is_ok()
    );
    // A range that starts before the queryable period is counted from its start.
    assert!(
        check(
            input(
                &points[..1],
                json!({"from": "2020-01-01T00:00:00Z", "to": "2026-10-26T00:00:00Z", "interval": "1h"}),
            ),
            at(NOW),
        )
        .is_ok()
    );
}

#[test]
fn a_range_wholly_before_the_queryable_period_is_refused_with_its_start() {
    // 2026-10-26T10:17:42Z - 3 months + 5 minutes, rounded up to the minute.
    assert_eq!(earliest(at(NOW)), at("2026-07-26T10:23:00Z"));
    assert_eq!(
        earliest(at("2026-10-26T10:17:00Z")),
        at("2026-07-26T10:22:00Z")
    );
    let message = refused(input(
        &["a"],
        json!({"from": "2026-01-01T00:00:00Z", "to": "2026-07-26T10:23:00Z"}),
    ));
    assert!(message.contains("2026-07-26T10:23:00Z"), "{message}");
    assert!(
        check(
            input(
                &["a"],
                json!({"from": "2026-01-01T00:00:00Z", "to": "2026-07-26T10:24:00Z"})
            ),
            at(NOW),
        )
        .is_ok()
    );
    // Last year ended before the period in October, not in February.
    assert!(refused(input(&["a"], json!({"period": "last_year"}))).contains("queryable"));
    assert!(
        check(
            input(&["a"], json!({"period": "last_year"})),
            at("2026-02-10T00:00:00Z")
        )
        .is_ok()
    );
    // Every other period ends within it.
    for period in [
        "yesterday",
        "last_week",
        "last_month",
        "last_366_days",
        "last_72_hours",
    ] {
        assert!(
            check(input(&["a"], json!({"period": period})), at(NOW)).is_ok(),
            "{period}"
        );
    }
}

fn range(truncated: bool) -> Range {
    Range {
        tz: "Asia/Taipei".into(),
        interval: "1d".into(),
        from: t("2026-10-01T00:00:00+08:00"),
        to: t("2026-10-03T00:00:00+08:00"),
        truncated,
        earliest: t("2026-07-26T18:23:00+08:00"),
        ends_after_now: false,
        resolution: "1h".into(),
        queried_at: t("2026-10-26T18:17:42+08:00"),
    }
}

fn body(points: &[&str], interval: &'static str) -> Body {
    Body {
        series: points
            .iter()
            .map(|point| Key {
                device_id: Uuid::try_parse(DEVICE).expect("a UUID"),
                point: (*point).to_owned(),
            })
            .collect(),
        tz: "Asia/Taipei".into(),
        period: Some("this_month".into()),
        from: None,
        to: None,
        interval,
    }
}

fn usage_series(
    point: &str,
    status: UsageStatus,
    verdict: Verdict,
    suspicious: u64,
) -> SeriesUsage {
    let ok = status == UsageStatus::Ok;
    SeriesUsage {
        device_id: Uuid::try_parse(DEVICE).expect("a UUID"),
        point: point.into(),
        status,
        check: Some(CumulativeCheck {
            verdict,
            drops: 26,
            rises: 21,
            comparisons: 47,
        }),
        total: ok.then_some(164.046),
        drops: 1,
        readings: 48,
        first_reading: None,
        last_reading: None,
        baseline: true,
        gaps: 0,
        longest_gap: None,
        suspicious_steps: suspicious,
        largest_suspicious_step: (suspicious > 0).then(|| TimedValue {
            time: t("2026-10-02T09:05:00+08:00"),
            value: 68_822.0,
        }),
        highest: None,
        lowest: None,
        rows: if ok {
            vec![
                UsageRow {
                    time: t("2026-10-01T00:00:00+08:00"),
                    usage: 100.0,
                    readings: 24,
                    drops: 1,
                    after_gap: false,
                    suspicious_steps: 0,
                },
                UsageRow {
                    time: t("2026-10-02T00:00:00+08:00"),
                    usage: 64.046,
                    readings: 24,
                    drops: 0,
                    after_gap: true,
                    suspicious_steps: suspicious,
                },
            ]
        } else {
            Vec::new()
        },
    }
}

fn text(output: &ToolOutput) -> Value {
    match output.content.as_slice() {
        [crate::tools::ToolContent::Text(text)] => serde_json::from_str(text).expect("JSON"),
        other => panic!("not one text: {other:?}"),
    }
}

#[test]
fn usage_answers_carry_rows_and_the_notes_the_model_must_follow() {
    let answer = Usage {
        range: range(true),
        series: vec![
            usage_series("energy_kwh", UsageStatus::Ok, Verdict::TooFewReadings, 2),
            usage_series(
                "power_kw",
                UsageStatus::NotCumulative,
                Verdict::NotCumulative,
                0,
            ),
            usage_series(
                "running",
                UsageStatus::NotNumeric,
                Verdict::TooFewReadings,
                0,
            ),
            usage_series("water_m3", UsageStatus::NoData, Verdict::TooFewReadings, 0),
        ],
    };
    let output = usage_output(
        &body(&["energy_kwh", "power_kw", "running", "water_m3"], "1d"),
        answer,
    )
    .expect("an output");
    assert!(!output.is_error, "one series has a usage");
    let result = text(&output);
    let energy = &result["series"][0];
    assert_eq!(energy["total"], 164.046);
    assert_eq!(
        energy["rows"][1],
        json!(["2026-10-02T00:00:00+08:00", 64.046, 24, 0, true, 2])
    );
    assert_eq!(energy["check"]["verdict"], "too_few_readings");
    assert_eq!(result["series"][1]["status"], "not_cumulative");
    assert!(result["series"][1].get("total").is_none());
    let notes: Vec<&str> = result["notes"]
        .as_array()
        .expect("notes")
        .iter()
        .map(|note| note.as_str().expect("text"))
        .collect();
    let has = |needle: &str| notes.iter().any(|note| note.contains(needle));
    assert!(has("the start of the queryable period"), "{notes:?}");
    assert!(has("unconfirmed"), "{notes:?}");
    assert!(has("may include a misread") && has("68822"), "{notes:?}");
    assert!(
        has("fell 26 times and rose 21 times in 47 comparisons"),
        "{notes:?}"
    );
    assert!(
        has("`running` of device") && has("on/off or text"),
        "{notes:?}"
    );
    assert!(has("`water_m3` of device") && has("資料不足"), "{notes:?}");
}

#[test]
fn a_usage_without_any_counter_is_an_error_that_points_to_the_statistics() {
    let answer = Usage {
        range: range(false),
        series: vec![usage_series(
            "power_kw",
            UsageStatus::NotCumulative,
            Verdict::NotCumulative,
            0,
        )],
    };
    let output = usage_output(&body(&["power_kw"], "total"), answer).expect("an output");
    assert!(output.is_error);
    let result = text(&output);
    assert!(
        result["notes"][0]
            .as_str()
            .expect("a note")
            .contains("telemetry_statistics")
    );
    assert!(result.get("columns").is_none(), "no rows for total");
}

#[test]
fn an_answer_for_other_series_is_a_failure() {
    let answer = Usage {
        range: range(false),
        series: vec![usage_series(
            "energy_kwh",
            UsageStatus::Ok,
            Verdict::Cumulative,
            0,
        )],
    };
    assert_eq!(
        usage_output(&body(&["water_m3"], "1d"), answer).err(),
        Some(ToolError::Failed)
    );
    let answer = Statistics {
        range: range(false),
        series: Vec::new(),
    };
    assert_eq!(
        statistics_output(&body(&["energy_kwh"], "1d"), answer).err(),
        Some(ToolError::Failed)
    );
}

fn statistics_series(point: &str, rows: usize) -> SeriesStatistics {
    SeriesStatistics {
        device_id: Uuid::try_parse(DEVICE).expect("a UUID"),
        point: point.into(),
        status: if rows == 0 {
            DataStatus::NoData
        } else {
            DataStatus::Ok
        },
        count: rows as u64,
        numeric_count: rows as u64,
        min: Some(1.0),
        min_time: Some(t("2026-10-01T00:00:00+08:00")),
        max: Some(2.0),
        max_time: Some(t("2026-10-01T01:00:00+08:00")),
        avg: Some(1.5),
        first: Some(TimedReading {
            time: t("2026-10-01T00:00:12+08:00"),
            value: Reading::Float(Some(1.0)),
        }),
        last: None,
        highest: None,
        lowest: None,
        rows: (0..rows)
            .map(|_| StatisticsRow {
                time: t("2026-10-01T00:00:00+08:00"),
                count: 1,
                min: Some(1.0),
                max: Some(1.0),
                avg: Some(1.0),
                last: Reading::String("auto".into()),
            })
            .collect(),
    }
}

#[test]
fn statistics_keep_the_figures_and_leave_out_rows_over_the_limit() {
    let answer = Statistics {
        range: range(false),
        series: vec![
            statistics_series("power_kw", 3),
            statistics_series("water_m3", 0),
        ],
    };
    let output =
        statistics_output(&body(&["power_kw", "water_m3"], "1d"), answer).expect("an output");
    assert!(!output.is_error, "no data is an answer");
    let result = text(&output);
    assert_eq!(
        result["columns"],
        json!(["time", "count", "min", "max", "avg", "last"])
    );
    assert_eq!(result["series"][0]["rows"][0][5], "auto");
    assert_eq!(
        result["series"][0]["first"],
        json!(["2026-10-01T00:00:12+08:00", 1.0])
    );
    assert_eq!(result["series"][1]["status"], "no_data");
    assert!(
        result["notes"][0]
            .as_str()
            .expect("a note")
            .contains("資料不足")
    );

    let answer = Statistics {
        range: range(false),
        series: vec![
            statistics_series("power_kw", 300),
            statistics_series("current_a", 201),
        ],
    };
    let result = text(
        &statistics_output(&body(&["power_kw", "current_a"], "1d"), answer).expect("an output"),
    );
    assert!(result["series"][0].get("rows").is_none());
    assert_eq!(result["series"][0]["avg"], 1.5);
    assert!(
        result["notes"][0]
            .as_str()
            .expect("a note")
            .contains("501 bucket rows")
    );
}
