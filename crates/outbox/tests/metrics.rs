//! The lag gauges of a real `outbox-example` process, exported over OTLP/HTTP to a fake
//! receiver and decoded, against a real Postgres (`TEST_DATABASE_URL`, or a throwaway
//! container; skipped without either unless `CI` is set).

mod support;

use std::time::Duration;

use opentelemetry_proto::tonic::collector::metrics::v1::ExportMetricsServiceRequest;
use opentelemetry_proto::tonic::common::v1::any_value;
use opentelemetry_proto::tonic::metrics::v1::{metric, number_data_point};
use prost::Message as _;
use support::{Example, FakeCollector, database, eventually};

const SUBSCRIPTION: &str = "outbox-example.widget-names";

/// One exported gauge point: the value, and the subscription and source attributes.
#[derive(Debug, Clone, PartialEq)]
struct Point {
    metric: String,
    unit: String,
    value: f64,
    subscription: String,
    source: String,
}

fn points(collector: &FakeCollector) -> Vec<Point> {
    let mut points = Vec::new();
    for body in collector.bodies("/v1/metrics") {
        let request = ExportMetricsServiceRequest::decode(body.as_slice()).expect("OTLP metrics");
        for resource in request.resource_metrics {
            for scope in resource.scope_metrics {
                for metric in scope.metrics {
                    let Some(metric::Data::Gauge(gauge)) = metric.data else {
                        continue;
                    };
                    for point in gauge.data_points {
                        let attribute = |key: &str| {
                            point
                                .attributes
                                .iter()
                                .find(|attribute| attribute.key == key)
                                .and_then(|attribute| attribute.value.as_ref())
                                .and_then(|value| match &value.value {
                                    Some(any_value::Value::StringValue(text)) => Some(text.clone()),
                                    _ => None,
                                })
                                .unwrap_or_default()
                        };
                        #[allow(
                            clippy::cast_precision_loss,
                            reason = "test values are small counts"
                        )]
                        let value = match point.value {
                            Some(number_data_point::Value::AsInt(value)) => value as f64,
                            Some(number_data_point::Value::AsDouble(value)) => value,
                            None => continue,
                        };
                        points.push(Point {
                            metric: metric.name.clone(),
                            unit: metric.unit.clone(),
                            value,
                            subscription: attribute("outbox.subscription"),
                            source: attribute("outbox.source"),
                        });
                    }
                }
            }
        }
    }
    points
}

#[tokio::test]
async fn the_lag_is_exported_over_otlp() {
    let Some(database) = database().await else {
        return;
    };
    let setup = database.setup().await;
    let collector = FakeCollector::start();
    let endpoint = collector.url();
    // Processing waits an hour, so the events pile up; the lag is measured every 200 ms and
    // exported every 300 ms.
    let example = Example::start(
        &database,
        &setup,
        &[
            ("TRUSTIOT__TELEMETRY__OTLP_ENDPOINT", endpoint.as_str()),
            ("OTEL_METRIC_EXPORT_INTERVAL", "300"),
            ("TRUSTIOT__CONSUMER__PROCESS_AFTER", "1h"),
            ("TRUSTIOT__CONSUMER__LAG_INTERVAL", "200ms"),
        ],
    );
    for (id, name) in [("a", "one"), ("b", "two"), ("c", "three")] {
        let (status, body) = example.rename(id, name);
        assert_eq!(status, 200, "{body}");
    }

    let lag = eventually(Duration::from_secs(30), "a lag of 3 over OTLP", || async {
        points(&collector)
            .into_iter()
            .rfind(|point| point.metric == "outbox.consumer.lag" && point.value >= 3.0)
    })
    .await;
    assert_eq!(
        lag,
        Point {
            metric: "outbox.consumer.lag".to_owned(),
            unit: "{event}".to_owned(),
            value: 3.0,
            subscription: SUBSCRIPTION.to_owned(),
            source: setup.producer_schema.clone(),
        }
    );
    let age = points(&collector)
        .into_iter()
        .rfind(|point| point.metric == "outbox.consumer.lag.age")
        .expect("the age gauge");
    assert_eq!(age.unit, "s");
    assert_eq!(age.subscription, SUBSCRIPTION);
    assert!(age.value > 0.0, "{age:?}");
    drop(example);
}
