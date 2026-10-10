//! OpenTelemetry providers: always a tracer (so every log line in a span has a trace id), and
//! OTLP/HTTP exporters for traces, logs, and metrics only when an endpoint is configured.

use std::time::Duration;

use opentelemetry::KeyValue;
use opentelemetry_otlp::{ExporterBuildError, Protocol, WithExportConfig};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::logs::SdkLoggerProvider;
use opentelemetry_sdk::metrics::SdkMeterProvider;
use opentelemetry_sdk::trace::{SdkTracerProvider, SpanData, SpanExporter};

use super::TelemetryError;
use super::redact::{REDACTED, is_sensitive};
use crate::ServiceInfo;

/// Replaces sensitive span, event, and link attributes before spans leave the process.
#[derive(Debug)]
pub(crate) struct RedactingSpanExporter<E>(pub(crate) E);

fn redact(attributes: &mut [KeyValue]) {
    for attribute in attributes {
        if is_sensitive(attribute.key.as_str()) {
            attribute.value = REDACTED.into();
        }
    }
}

impl<E: SpanExporter> SpanExporter for RedactingSpanExporter<E> {
    fn export(&self, mut batch: Vec<SpanData>) -> impl Future<Output = OTelSdkResult> + Send {
        for span in &mut batch {
            redact(&mut span.attributes);
            for event in &mut span.events.events {
                redact(&mut event.attributes);
            }
            for link in &mut span.links.links {
                redact(&mut link.attributes);
            }
        }
        self.0.export(batch)
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        self.0.shutdown_with_timeout(timeout)
    }

    fn force_flush(&self) -> OTelSdkResult {
        self.0.force_flush()
    }

    fn set_resource(&mut self, resource: &Resource) {
        self.0.set_resource(resource);
    }
}

fn resource(info: &ServiceInfo) -> Resource {
    Resource::builder()
        .with_service_name(info.name)
        .with_attribute(KeyValue::new("service.version", info.version))
        .build()
}

/// The providers [`super::Telemetry`] installs.
pub(crate) struct Providers {
    pub(crate) tracer: SdkTracerProvider,
    /// Exports logs; only with an endpoint.
    pub(crate) logger: Option<SdkLoggerProvider>,
    /// Exports metrics; only with an endpoint.
    pub(crate) meter: Option<SdkMeterProvider>,
}

/// Builds the tracer provider and, when `endpoint` is set, the OTLP log and metric providers.
pub(crate) fn providers(
    info: &ServiceInfo,
    endpoint: Option<&str>,
) -> Result<Providers, TelemetryError> {
    let resource = resource(info);
    let Some(endpoint) = endpoint else {
        let tracer = SdkTracerProvider::builder().with_resource(resource).build();
        return Ok(Providers {
            tracer,
            logger: None,
            meter: None,
        });
    };
    // reqwest's blocking client, which the OTLP exporters use from their own export threads,
    // panics when it is built on an async runtime thread, so build it on a plain thread.
    std::thread::scope(|scope| {
        scope
            .spawn(|| exporting_providers(resource, endpoint))
            .join()
            .unwrap_or_else(|_| {
                Err(TelemetryError::Exporter {
                    signal: "OTLP",
                    message: "building the exporters panicked".to_owned(),
                })
            })
    })
}

fn exporting_providers(resource: Resource, endpoint: &str) -> Result<Providers, TelemetryError> {
    let spans = opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .with_protocol(Protocol::HttpBinary)
        .with_endpoint(format!("{endpoint}/v1/traces"))
        .build()
        .map_err(|error| exporter_error("trace", error))?;
    let logs = opentelemetry_otlp::LogExporter::builder()
        .with_http()
        .with_protocol(Protocol::HttpBinary)
        .with_endpoint(format!("{endpoint}/v1/logs"))
        .build()
        .map_err(|error| exporter_error("log", error))?;
    let metrics = opentelemetry_otlp::MetricExporter::builder()
        .with_http()
        .with_protocol(Protocol::HttpBinary)
        .with_endpoint(format!("{endpoint}/v1/metrics"))
        .build()
        .map_err(|error| exporter_error("metric", error))?;
    let tracer = SdkTracerProvider::builder()
        .with_resource(resource.clone())
        .with_batch_exporter(RedactingSpanExporter(spans))
        .build();
    let logger = SdkLoggerProvider::builder()
        .with_resource(resource.clone())
        .with_batch_exporter(logs)
        .build();
    // The periodic reader collects and exports from its own thread every
    // `OTEL_METRIC_EXPORT_INTERVAL` milliseconds (60 s by default).
    let meter = SdkMeterProvider::builder()
        .with_resource(resource)
        .with_periodic_exporter(metrics)
        .build();
    Ok(Providers {
        tracer,
        logger: Some(logger),
        meter: Some(meter),
    })
}

/// The SDK's configuration errors read `<setting>: <reason>` and the reason can quote the
/// endpoint, whose path could carry a token, so only the setting is kept.
fn exporter_error(signal: &'static str, error: ExporterBuildError) -> TelemetryError {
    let message = match error {
        ExporterBuildError::InvalidConfiguration(detail) => format!(
            "invalid exporter configuration of `{}`",
            detail.split(": ").next().unwrap_or_default()
        ),
        other => other.to_string(),
    };
    TelemetryError::Exporter { signal, message }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use opentelemetry::trace::{Event, SpanContext, SpanKind, Status};
    use opentelemetry_sdk::trace::{SpanEvents, SpanLinks};

    use super::*;

    #[derive(Debug, Default)]
    struct Collect(Arc<Mutex<Vec<SpanData>>>);

    impl SpanExporter for Collect {
        async fn export(&self, batch: Vec<SpanData>) -> OTelSdkResult {
            self.0.lock().expect("collector lock").extend(batch);
            Ok(())
        }
    }

    fn span(attributes: Vec<KeyValue>) -> SpanData {
        let mut events = SpanEvents::default();
        events.events.push(Event::new(
            "event",
            std::time::SystemTime::now(),
            vec![KeyValue::new("body", "{\"secret\":1}")],
            0,
        ));
        SpanData {
            span_context: SpanContext::empty_context(),
            parent_span_id: opentelemetry::trace::SpanId::INVALID,
            parent_span_is_remote: false,
            span_kind: SpanKind::Server,
            name: "GET /v1/hello".into(),
            start_time: std::time::SystemTime::now(),
            end_time: std::time::SystemTime::now(),
            attributes,
            dropped_attributes_count: 0,
            events,
            links: SpanLinks::default(),
            status: Status::Unset,
            instrumentation_scope: opentelemetry::InstrumentationScope::builder("test").build(),
        }
    }

    #[test]
    fn exported_spans_have_sensitive_attributes_redacted() {
        let collected = Arc::new(Mutex::new(Vec::new()));
        let exporter = RedactingSpanExporter(Collect(Arc::clone(&collected)));
        let batch = vec![span(vec![
            KeyValue::new("http.route", "/v1/users/{id}"),
            KeyValue::new("email", "alice@example.com"),
            KeyValue::new("authorization", "Bearer abc"),
        ])];
        let result = futures_executor_block_on(exporter.export(batch));
        assert!(result.is_ok());
        let spans = collected.lock().expect("collector lock");
        let attributes: Vec<(String, String)> = spans[0]
            .attributes
            .iter()
            .map(|kv| (kv.key.to_string(), kv.value.to_string()))
            .collect();
        assert_eq!(
            attributes,
            [
                ("http.route".to_owned(), "/v1/users/{id}".to_owned()),
                ("email".to_owned(), REDACTED.to_owned()),
                ("authorization".to_owned(), REDACTED.to_owned()),
            ]
        );
        assert_eq!(
            spans[0].events.events[0].attributes[0].value.to_string(),
            REDACTED
        );
    }

    #[test]
    fn exporter_errors_keep_the_setting_but_not_the_endpoint() {
        let error = exporter_error(
            "trace",
            ExporterBuildError::InvalidConfiguration(
                "endpoint: invalid endpoint 'http://collector:4318/token-abc/v1/traces': bad"
                    .to_owned(),
            ),
        );
        let message = error.to_string();
        assert!(message.contains("`endpoint`"), "{message}");
        assert!(!message.contains("token-abc"), "{message}");

        let error = exporter_error(
            "log",
            ExporterBuildError::InternalFailure("failed to build the HTTP client".to_owned()),
        );
        assert!(
            error
                .to_string()
                .contains("failed to build the HTTP client"),
            "{error}"
        );
    }

    /// Drives a future to completion on the current thread; the exporter future is ready at
    /// once, so no runtime is needed.
    fn futures_executor_block_on<F: Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("build a test runtime")
            .block_on(future)
    }
}
