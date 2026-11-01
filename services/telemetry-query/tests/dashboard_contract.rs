//! The dashboards of Laysi/trustiot-v2#150 store a semantic request in `contract` types and read
//! the answer's range into them (`contract::api::dashboard::spec::SemanticRequest`,
//! `contract::api::dashboard::result::SemanticRange`). This test keeps those types and this
//! service's own (`semantic::SeriesQuery`, `semantic::QueryRange`) alike: they accept the same
//! bodies, a stored request reaches the service as it was written, and a range reads over with
//! every field. No containers.

use contract::api::dashboard::result::SemanticRange;
use contract::api::dashboard::spec::{SemanticInterval, SemanticRequest};
use serde_json::{Value, json};
use telemetry_query::semantic::range::Interval;
use telemetry_query::semantic::{QueryRange, SeriesQuery};

const DEVICE: &str = "0190a5b2-7c3d-7e4f-8a1b-2c3d4e5f6a7b";

fn bodies() -> Vec<Value> {
    let series = json!([{"device_id": DEVICE, "point": "energy"}]);
    vec![
        json!({"series": series, "tz": "Asia/Taipei", "period": "last_7_days"}),
        json!({"series": series, "tz": "Asia/Taipei", "period": "today", "interval": "1h"}),
        json!({
            "series": series,
            "tz": "Asia/Taipei",
            "from": "2026-10-01T00:00:00+08:00",
            "to": "2026-10-08T00:00:00+08:00",
            "interval": "1d"
        }),
        json!({"series": series, "tz": "UTC", "from": "2026-10-01T00:00:00Z", "to": "2026-10-02T00:00:00Z"}),
        json!({"series": [], "tz": "Asia/Taipei", "period": "today"}),
        // Refused by both: unknown fields, a wrong type, a bad interval or time.
        json!({"series": series, "tz": "Asia/Taipei", "period": "today", "tenant_id": DEVICE}),
        json!({"series": [{"device_id": DEVICE, "point": "energy", "extra": 1}], "tz": "UTC", "period": "today"}),
        json!({"series": series, "tz": "Asia/Taipei", "period": "today", "interval": "2h"}),
        json!({"series": series, "tz": "Asia/Taipei", "from": "2026-10-01"}),
        json!({"series": series, "tz": 8, "period": "today"}),
        json!({"series": [{"device_id": "not-a-uuid", "point": "energy"}], "tz": "UTC", "period": "today"}),
    ]
}

#[test]
fn both_types_accept_the_same_bodies() {
    for body in bodies() {
        let service = serde_json::from_value::<SeriesQuery>(body.clone());
        let dashboard = serde_json::from_value::<SemanticRequest>(body.clone());
        assert_eq!(service.is_ok(), dashboard.is_ok(), "{body}");
        if let (Ok(service), Ok(dashboard)) = (service, dashboard) {
            // What the assistant sends at every run is what the service read from the body.
            let sent = serde_json::to_value(&dashboard).expect("serialize");
            let received: SeriesQuery = serde_json::from_value(sent).expect("the service reads it");
            assert_eq!(received, service, "{body}");
        }
    }
}

#[test]
fn every_interval_is_known_to_both() {
    for interval in [
        Interval::Minute1,
        Interval::Minutes5,
        Interval::Minutes15,
        Interval::Hour1,
        Interval::Day1,
        Interval::Week1,
        Interval::Month1,
        Interval::Total,
    ] {
        let json = serde_json::to_value(interval).expect("serialize");
        let dashboard: SemanticInterval = serde_json::from_value(json.clone()).expect("known");
        assert_eq!(serde_json::to_value(dashboard).expect("serialize"), json);
    }
    assert_eq!(
        serde_json::to_value(Interval::default()).expect("serialize"),
        serde_json::to_value(SemanticInterval::default()).expect("serialize")
    );
}

#[test]
fn a_range_reads_over_with_every_field() {
    let json = json!({
        "tz": "Asia/Taipei",
        "interval": "1d",
        "from": "2026-10-20T00:00:00+08:00",
        "to": "2026-10-27T00:00:00+08:00",
        "truncated": true,
        "earliest": "2026-10-21T08:00:00+08:00",
        "ends_after_now": true,
        "resolution": "1h",
        "queried_at": "2026-10-26T09:02:03.456+08:00"
    });
    let service: QueryRange = serde_json::from_value(json.clone()).expect("the service's range");
    let written = serde_json::to_value(&service).expect("serialize");
    assert_eq!(written, json, "the fixture names every field of QueryRange");
    let dashboard: SemanticRange = serde_json::from_value(written.clone()).expect("read over");
    assert_eq!(
        serde_json::to_value(&dashboard).expect("serialize"),
        written
    );
}
