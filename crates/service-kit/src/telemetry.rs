//! Structured JSON logs on stdout with trace and span ids, and optional OTLP export of logs,
//! traces, and metrics (docs/architecture/observability.md).
//!
//! Every log line is one JSON object: `timestamp`, `level`, `service`, `target`, `trace_id` and
//! `span_id` (inside a span), `message`, and `fields`. Field values whose names look sensitive
//! are replaced by [`REDACTED`] in the lines and in everything exported over OTLP.

mod log_layer;
mod otlp;
pub mod redact;

use std::time::Duration;

use opentelemetry::logs::LoggerProvider as _;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_sdk::logs::{SdkLogger, SdkLoggerProvider};
use opentelemetry_sdk::metrics::SdkMeterProvider;
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::trace::{SdkTracer, SdkTracerProvider};
use serde::Deserialize;
use tracing::subscriber::Interest;
use tracing::{Level, Metadata, Subscriber};
use tracing_subscriber::filter::{LevelFilter, Targets, filter_fn};
use tracing_subscriber::layer::{Context, Filter, Layer, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt;

use crate::ServiceInfo;
use crate::config::Invalid;

use self::log_layer::{LineSink, LogLayer, Stdout, sdk_diagnostic_suppressed};
pub use self::redact::{REDACTED, is_sensitive};

/// `telemetry.*`: logs and traces.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TelemetryConfig {
    /// Which events are logged: comma-separated `target=level` directives and a default level,
    /// for example `info` or `info,sqlx=warn`. Default `info`.
    pub log_filter: String,
    /// Base URL of an OTLP/HTTP receiver, such as `http://otel-collector:4318`. Unset (the
    /// default) turns export off; set, logs go to `<url>/v1/logs`, traces to `<url>/v1/traces`,
    /// and metrics to `<url>/v1/metrics`. Only `http://` is accepted: the service mesh
    /// encrypts the hop.
    pub otlp_endpoint: Option<String>,
}

impl Default for TelemetryConfig {
    fn default() -> Self {
        Self {
            log_filter: "info".to_owned(),
            otlp_endpoint: None,
        }
    }
}

impl TelemetryConfig {
    pub(crate) fn validate(&self) -> Result<(), Invalid> {
        self.targets()?;
        self.otlp_base()?;
        Ok(())
    }

    fn targets(&self) -> Result<Targets, Invalid> {
        self.log_filter.parse().map_err(|_| {
            Invalid::new(
                "telemetry.log_filter",
                "is not a valid filter; use a level and `target=level` directives separated by \
                 commas, for example `info,sqlx=warn`",
            )
        })
    }

    /// The endpoint without a trailing slash, after checking it is a plain-HTTP base URL.
    fn otlp_base(&self) -> Result<Option<String>, Invalid> {
        let Some(endpoint) = &self.otlp_endpoint else {
            return Ok(None);
        };
        let invalid = |message: &str| Invalid::new("telemetry.otlp_endpoint", message);
        let uri: axum::http::Uri = endpoint.parse().map_err(|_| {
            invalid("is not a URL; expected for example http://otel-collector:4318")
        })?;
        if uri.scheme_str() != Some("http") {
            return Err(invalid(
                "must be an http:// URL; the service mesh encrypts the connection",
            ));
        }
        let Some(authority) = uri.authority() else {
            return Err(invalid("must name a host"));
        };
        if authority.as_str().contains('@') {
            return Err(invalid("must not contain credentials"));
        }
        if uri.query().is_some() {
            return Err(invalid("must not have a query"));
        }
        let path = uri.path().trim_end_matches('/');
        Ok(Some(format!("http://{authority}{path}")))
    }
}

/// Why telemetry could not start.
#[derive(Debug, thiserror::Error)]
pub enum TelemetryError {
    /// A global tracing subscriber is already installed in this process.
    #[error("telemetry is already initialized in this process")]
    AlreadyInitialized,
    /// The configuration is invalid.
    #[error("configuration key `{}` is invalid: {}", .0.key, .0.message)]
    Config(Invalid),
    /// An OTLP exporter could not be built.
    #[error("cannot build the OTLP {signal} exporter: {message}")]
    Exporter {
        /// `trace`, `log`, or `metric`.
        signal: &'static str,
        /// The exporter's error.
        message: String,
    },
}

/// The installed telemetry. Call [`Telemetry::shutdown`] before the process exits so buffered
/// logs, spans, and the last metric readings are exported.
#[derive(Debug)]
pub struct Telemetry {
    tracer_provider: SdkTracerProvider,
    logger_provider: Option<SdkLoggerProvider>,
    meter_provider: Option<SdkMeterProvider>,
}

impl Telemetry {
    /// Installs the global subscriber: JSON lines on stdout, trace context for every span, and
    /// OTLP export when `config.otlp_endpoint` is set. Also installs the W3C trace-context
    /// propagator, a panic hook that logs panics as JSON, and, with an endpoint, the global
    /// meter provider: instruments created afterwards through `opentelemetry::global::meter`
    /// are exported, those created before stay no-ops.
    pub fn init(info: &ServiceInfo, config: &TelemetryConfig) -> Result<Self, TelemetryError> {
        let targets = config.targets().map_err(TelemetryError::Config)?;
        let endpoint = config.otlp_base().map_err(TelemetryError::Config)?;
        let otlp::Providers {
            tracer: tracer_provider,
            logger: logger_provider,
            meter: meter_provider,
        } = otlp::providers(info, endpoint.as_deref())?;
        let logger = logger_provider
            .as_ref()
            .map(|provider| provider.logger("service-kit"));
        subscriber(
            info.name,
            targets,
            info.log_ceilings,
            tracer_provider.tracer("service-kit"),
            logger,
            Stdout,
        )
        .try_init()
        .map_err(|_| TelemetryError::AlreadyInitialized)?;
        opentelemetry::global::set_text_map_propagator(TraceContextPropagator::new());
        opentelemetry::global::set_tracer_provider(tracer_provider.clone());
        if let Some(meter_provider) = &meter_provider {
            opentelemetry::global::set_meter_provider(meter_provider.clone());
        }
        install_panic_hook();
        Ok(Self {
            tracer_provider,
            logger_provider,
            meter_provider,
        })
    }

    /// Exports what is still buffered and stops the exporters, waiting at most `timeout`.
    pub async fn shutdown(self, timeout: Duration) {
        let Self {
            tracer_provider,
            logger_provider,
            meter_provider,
        } = self;
        // Shutting a provider down blocks until its export thread finishes. They shut down in
        // parallel, so the whole shutdown still takes at most `timeout`.
        let traces =
            tokio::task::spawn_blocking(move || tracer_provider.shutdown_with_timeout(timeout));
        let logs = tokio::task::spawn_blocking(move || {
            logger_provider.map(|provider| provider.shutdown_with_timeout(timeout))
        });
        let metrics = tokio::task::spawn_blocking(move || {
            meter_provider.map(|provider| provider.shutdown_with_timeout(timeout))
        });
        let margin = Duration::from_millis(500);
        let all = async { (traces.await, logs.await, metrics.await) };
        match tokio::time::timeout(timeout + margin, all).await {
            Ok((Ok(Ok(())), Ok(None | Some(Ok(()))), Ok(None | Some(Ok(()))))) => {}
            Ok((traces, logs, metrics)) => {
                tracing::warn!(
                    traces = ?traces.map(|result| result.err()),
                    logs = ?logs.map(|result| result.and_then(Result::err)),
                    metrics = ?metrics.map(|result| result.and_then(Result::err)),
                    "telemetry export did not shut down cleanly"
                );
            }
            Err(_) => tracing::warn!(
                timeout_ms = duration_ms(timeout),
                "telemetry export did not finish in time"
            ),
        }
    }
}

/// A level that a target's events never exceed, whatever `telemetry.log_filter` says: neither
/// its stdout line nor its OTLP record is written. The target matches itself and its submodules
/// (`tungstenite` covers `tungstenite::protocol`, not `tungstenite_extra`), for events sent
/// through `tracing` and events bridged from the `log` crate alike. A service sets its ceilings
/// with [`ServiceInfo::with_log_ceilings`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LogCeiling {
    target: &'static str,
    level: Level,
}

impl LogCeiling {
    /// Events of `target` (and its submodules) above `level` are never written.
    pub const fn new(target: &'static str, level: Level) -> Self {
        Self { target, level }
    }

    /// Whether this ceiling drops an event of `target` at `level`.
    fn drops(&self, target: &str, level: &Level) -> bool {
        *level > self.level
            && target
                .strip_prefix(self.target)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with("::"))
    }
}

/// Whether one of `ceilings` drops an event of `target` at `level`.
pub(crate) fn above_ceiling(ceilings: &[LogCeiling], target: &str, level: &Level) -> bool {
    ceilings.iter().any(|ceiling| ceiling.drops(target, level))
}

/// Logs to stdout at `info` without trace context or export. For reporting a configuration
/// error before the real telemetry settings are known.
pub fn init_fallback(info: &ServiceInfo) {
    let tracer_provider = SdkTracerProvider::builder().build();
    let targets = Targets::new().with_default(LevelFilter::INFO);
    // Fails only when a subscriber is already installed, which then reports the error instead.
    let _ = subscriber(
        info.name,
        targets,
        info.log_ceilings,
        tracer_provider.tracer("service-kit"),
        None,
        Stdout,
    )
    .try_init();
}

/// Duration in milliseconds, for log fields.
pub(crate) fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let location = info
            .location()
            .map(|location| format!("{}:{}", location.file(), location.line()));
        let payload = info.payload_as_str().unwrap_or("(not a string)");
        tracing::error!(
            target: "service_kit::panic",
            location = location.as_deref(),
            payload,
            "thread panicked"
        );
    }));
}

/// Spans are created at `info` and above whatever the log filter says, so that events inside a
/// request always find the request's trace context.
fn span_enabled(metadata: &Metadata<'_>) -> bool {
    metadata.is_span() && *metadata.level() <= Level::INFO
}

/// The log layer's filter: spans as above, events by the configured directives, except that
/// the OpenTelemetry SDK's diagnostics below INFO are always dropped (they can print the OTLP
/// endpoint's path; see [`sdk_diagnostic_suppressed`]), and so are events above the service's
/// [`LogCeiling`]s.
///
/// Events bridged from the `log` crate (what `reqwest` sends) need no special case here: the
/// bridge first asks the subscriber whether a record with the `log` crate's own target is
/// enabled, so `enabled` sees the real target, and the answer is kept for the event that follows.
/// The tests in `log_layer.rs` and `tests/otlp.rs` pin this down.
struct LogFilter {
    targets: Targets,
    ceilings: &'static [LogCeiling],
}

impl<S> Filter<S> for LogFilter {
    fn enabled(&self, metadata: &Metadata<'_>, _: &Context<'_, S>) -> bool {
        if metadata.is_span() {
            span_enabled(metadata)
        } else {
            !sdk_diagnostic_suppressed(metadata.target(), metadata.level())
                && !above_ceiling(self.ceilings, metadata.target(), metadata.level())
                && self
                    .targets
                    .would_enable(metadata.target(), metadata.level())
        }
    }

    fn callsite_enabled(&self, metadata: &'static Metadata<'static>) -> Interest {
        if metadata.is_span() {
            if span_enabled(metadata) {
                Interest::always()
            } else {
                Interest::never()
            }
        } else {
            // Events bridged from the `log` crate share callsites across targets, so the
            // decision is made per event.
            Interest::sometimes()
        }
    }

    fn max_level_hint(&self) -> Option<LevelFilter> {
        let configured = self
            .targets
            .iter()
            .map(|(_, level)| level)
            .chain(self.targets.default_level())
            .max()
            .unwrap_or(LevelFilter::OFF);
        Some(configured.max(LevelFilter::INFO))
    }
}

fn subscriber<W: LineSink>(
    service: &'static str,
    targets: Targets,
    ceilings: &'static [LogCeiling],
    tracer: SdkTracer,
    logger: Option<SdkLogger>,
    sink: W,
) -> impl Subscriber + Send + Sync + for<'a> LookupSpan<'a> {
    // Span events would duplicate the log records, so the OpenTelemetry layer sees spans only.
    let spans = tracing_opentelemetry::layer()
        .with_tracer(tracer)
        .with_filter(filter_fn(span_enabled).with_max_level_hint(LevelFilter::INFO));
    let logs =
        LogLayer::new(sink, service, logger, ceilings).with_filter(LogFilter { targets, ceilings });
    tracing_subscriber::registry().with(spans).with(logs)
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::log_layer::Capture;
    use super::*;

    /// A subscriber that writes JSON lines into the returned buffer. Keep the provider alive
    /// for as long as the subscriber is used.
    pub(crate) fn capture_subscriber(
        filter: &str,
    ) -> (
        impl Subscriber + Send + Sync + 'static,
        Capture,
        SdkTracerProvider,
    ) {
        capture_subscriber_with_ceilings(filter, &[])
    }

    /// [`capture_subscriber`] for a service with `ceilings`.
    pub(crate) fn capture_subscriber_with_ceilings(
        filter: &str,
        ceilings: &'static [LogCeiling],
    ) -> (
        impl Subscriber + Send + Sync + 'static,
        Capture,
        SdkTracerProvider,
    ) {
        let provider = SdkTracerProvider::builder().build();
        let capture = Capture::default();
        let subscriber = subscriber(
            "unit-test",
            filter.parse().expect("valid test filter"),
            ceilings,
            provider.tracer("unit-test"),
            None,
            capture.clone(),
        );
        (subscriber, capture, provider)
    }

    /// The captured lines, parsed.
    pub(crate) fn lines(capture: &Capture) -> Vec<serde_json::Value> {
        let bytes = capture.0.lock().expect("capture lock").clone();
        String::from_utf8(bytes)
            .expect("log lines are UTF-8")
            .lines()
            .map(|line| serde_json::from_str(line).expect("each line is one JSON object"))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(filter: &str, endpoint: Option<&str>) -> TelemetryConfig {
        TelemetryConfig {
            log_filter: filter.to_owned(),
            otlp_endpoint: endpoint.map(str::to_owned),
        }
    }

    #[test]
    fn log_filters_are_validated() {
        assert!(config("info", None).validate().is_ok());
        assert!(
            config("warn,service_kit=debug,sqlx=off", None)
                .validate()
                .is_ok()
        );
        let error = config("info,sqlx=loud", None)
            .validate()
            .expect_err("unknown level");
        assert_eq!(error.key, "telemetry.log_filter");
        assert!(!error.message.contains("loud"), "{}", error.message);
    }

    #[test]
    fn otlp_endpoints_must_be_plain_http_base_urls() {
        let base = |endpoint: &str| config("info", Some(endpoint)).otlp_base();
        assert_eq!(
            base("http://otel-collector:4318/").expect("valid"),
            Some("http://otel-collector:4318".to_owned())
        );
        assert_eq!(
            base("http://collector.observability.svc:4318/otlp").expect("valid"),
            Some("http://collector.observability.svc:4318/otlp".to_owned())
        );
        for endpoint in [
            "https://collector:4318",
            "collector:4318",
            "http://user:pass@collector:4318",
            "http://collector:4318/?token=x",
            "not a url",
        ] {
            let error = base(endpoint).expect_err(endpoint);
            assert_eq!(error.key, "telemetry.otlp_endpoint");
            assert!(!error.message.contains("pass"), "{}", error.message);
        }
    }

    #[test]
    fn filter_level_hint_covers_spans_and_configured_events() {
        let hint = |filter: &str| {
            Filter::<tracing_subscriber::Registry>::max_level_hint(&LogFilter {
                targets: filter.parse().expect("valid filter"),
                ceilings: &[],
            })
        };
        assert_eq!(hint("warn"), Some(LevelFilter::INFO));
        assert_eq!(hint("info,sqlx=debug"), Some(LevelFilter::DEBUG));
        assert_eq!(hint("trace"), Some(LevelFilter::TRACE));
    }
}
