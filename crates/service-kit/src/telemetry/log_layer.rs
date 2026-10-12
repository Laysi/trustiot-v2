//! Turns tracing events into one JSON line each on stdout and, when OTLP export is on, into
//! OpenTelemetry log records. Both outputs carry the same message, the same redacted fields,
//! and the trace and span ids of the span the event belongs to.

use std::fmt::{self, Debug};
use std::io::Write;
use std::sync::OnceLock;
#[cfg(test)]
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use opentelemetry::logs::{AnyValue, LogRecord as _, Logger as _, Severity};
use opentelemetry::trace::{SpanContext, TraceContextExt};
use opentelemetry::{Key, StringValue};
use opentelemetry_sdk::logs::SdkLogger;
use serde::Serialize;
use serde::ser::SerializeMap;
use tracing::dispatcher::WeakDispatch;
use tracing::field::{Field, Visit};
use tracing::span::Id;
use tracing::{Dispatch, Event, Level, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;

use super::redact::{REDACTED, is_sensitive};

/// Whether events with this target come from the OpenTelemetry SDK crates (`opentelemetry`,
/// `opentelemetry_sdk`, `opentelemetry-otlp`, `opentelemetry-http`, ...). The SDK names its
/// crate in the target of its own diagnostics.
pub(crate) fn is_sdk_target(target: &str) -> bool {
    target.starts_with("opentelemetry")
}

/// Whether an event is dropped whatever the log filter says: the SDK's diagnostics below INFO.
///
/// At DEBUG and TRACE the exporter records the full request URL (`HttpClient.NetworkError`,
/// `HttpClient.StatusError`), the collector's response body, and its own configuration. The URL's
/// path can carry a token, so these levels never leave the process. The INFO, WARN and ERROR
/// diagnostics of the pinned SDK versions do not contain the URL.
pub(crate) fn sdk_diagnostic_suppressed(target: &str, level: &Level) -> bool {
    is_sdk_target(target) && *level > Level::INFO
}

/// Where JSON log lines go: stdout in services, a buffer in tests.
pub(crate) trait LineSink: Send + Sync + 'static {
    /// Writes one complete line, including its trailing newline.
    fn write_line(&self, line: &[u8]);
}

/// Standard output. A failed write (for example a closed pipe) is dropped; logging must never
/// take the service down.
pub(crate) struct Stdout;

impl LineSink for Stdout {
    fn write_line(&self, line: &[u8]) {
        let mut stdout = std::io::stdout().lock();
        let _ = stdout.write_all(line);
        let _ = stdout.flush();
    }
}

/// Collects lines in memory, for tests.
#[cfg(test)]
#[derive(Clone, Default)]
pub(crate) struct Capture(pub(crate) Arc<Mutex<Vec<u8>>>);

#[cfg(test)]
impl LineSink for Capture {
    fn write_line(&self, line: &[u8]) {
        if let Ok(mut buffer) = self.0.lock() {
            buffer.extend_from_slice(line);
        }
    }
}

/// A recorded field value.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum FieldValue {
    Str(String),
    I64(i64),
    U64(u64),
    F64(f64),
    Bool(bool),
}

impl Serialize for FieldValue {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Str(text) => serializer.serialize_str(text),
            Self::I64(number) => serializer.serialize_i64(*number),
            Self::U64(number) => serializer.serialize_u64(*number),
            Self::F64(number) => serializer.serialize_f64(*number),
            Self::Bool(flag) => serializer.serialize_bool(*flag),
        }
    }
}

impl From<&FieldValue> for AnyValue {
    fn from(value: &FieldValue) -> Self {
        match value {
            FieldValue::Str(text) => AnyValue::String(StringValue::from(text.clone())),
            FieldValue::I64(number) => AnyValue::Int(*number),
            FieldValue::U64(number) => match i64::try_from(*number) {
                Ok(number) => AnyValue::Int(number),
                Err(_) => AnyValue::String(StringValue::from(number.to_string())),
            },
            FieldValue::F64(number) => AnyValue::Double(*number),
            FieldValue::Bool(flag) => AnyValue::Boolean(*flag),
        }
    }
}

/// The message and fields of one event, with sensitive values already replaced.
#[derive(Debug, Default)]
pub(crate) struct Recorded {
    pub(crate) message: String,
    /// The original target of an event bridged from the `log` crate.
    pub(crate) log_target: Option<String>,
    pub(crate) fields: Vec<(&'static str, FieldValue)>,
}

impl Recorded {
    fn put(&mut self, field: &Field, value: impl FnOnce() -> FieldValue) {
        match field.name() {
            "message" => {
                self.message = match value() {
                    FieldValue::Str(text) => text,
                    other => format!("{other:?}"),
                };
            }
            "log.target" => {
                if let FieldValue::Str(target) = value() {
                    self.log_target = Some(target);
                }
            }
            // The rest of the `log` crate's metadata duplicates the event's own.
            name if name.starts_with("log.") => {}
            // The value is never formatted, so a redacted value cannot leak through a panic or
            // a side effect of its Debug implementation either.
            name if is_sensitive(name) => {
                self.fields
                    .push((name, FieldValue::Str(REDACTED.to_owned())));
            }
            name => self.fields.push((name, value())),
        }
    }
}

impl Visit for Recorded {
    fn record_f64(&mut self, field: &Field, value: f64) {
        self.put(field, || FieldValue::F64(value));
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.put(field, || FieldValue::I64(value));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.put(field, || FieldValue::U64(value));
    }

    fn record_i128(&mut self, field: &Field, value: i128) {
        self.put(field, || match i64::try_from(value) {
            Ok(number) => FieldValue::I64(number),
            Err(_) => FieldValue::Str(value.to_string()),
        });
    }

    fn record_u128(&mut self, field: &Field, value: u128) {
        self.put(field, || match u64::try_from(value) {
            Ok(number) => FieldValue::U64(number),
            Err(_) => FieldValue::Str(value.to_string()),
        });
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.put(field, || FieldValue::Bool(value));
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.put(field, || FieldValue::Str(value.to_owned()));
    }

    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        self.put(field, || {
            let mut text = value.to_string();
            let mut source = value.source();
            while let Some(error) = source {
                text.push_str(": ");
                text.push_str(&error.to_string());
                source = error.source();
            }
            FieldValue::Str(text)
        });
    }

    fn record_debug(&mut self, field: &Field, value: &dyn Debug) {
        self.put(field, || FieldValue::Str(format!("{value:?}")));
    }
}

struct FieldMap<'a>(&'a [(&'static str, FieldValue)]);

impl Serialize for FieldMap<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(self.0.len()))?;
        for (name, value) in self.0 {
            map.serialize_entry(name, value)?;
        }
        map.end()
    }
}

/// One JSON log line. Field order is fixed so lines stay easy to read in `kubectl logs`.
#[derive(Serialize)]
struct Line<'a> {
    timestamp: String,
    level: &'static str,
    service: &'a str,
    target: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    trace_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    span_id: Option<String>,
    message: &'a str,
    #[serde(skip_serializing_if = "is_empty")]
    fields: FieldMap<'a>,
}

fn is_empty(fields: &FieldMap<'_>) -> bool {
    fields.0.is_empty()
}

fn severity(level: &Level) -> Severity {
    match *level {
        Level::ERROR => Severity::Error,
        Level::WARN => Severity::Warn,
        Level::INFO => Severity::Info,
        Level::DEBUG => Severity::Debug,
        Level::TRACE => Severity::Trace,
    }
}

/// The layer that writes JSON lines and, optionally, OpenTelemetry log records.
pub(crate) struct LogLayer<W> {
    sink: W,
    service: &'static str,
    otel: Option<SdkLogger>,
    dispatch: OnceLock<WeakDispatch>,
}

impl<W: LineSink> LogLayer<W> {
    pub(crate) fn new(sink: W, service: &'static str, otel: Option<SdkLogger>) -> Self {
        Self {
            sink,
            service,
            otel,
            dispatch: OnceLock::new(),
        }
    }

    /// The OpenTelemetry span context of the closest span around the event that has one.
    fn span_context<S>(&self, event: &Event<'_>, ctx: &Context<'_, S>) -> Option<SpanContext>
    where
        S: Subscriber + for<'a> LookupSpan<'a>,
    {
        let span = ctx.event_span(event)?;
        let dispatch: Dispatch = self.dispatch.get()?.upgrade()?;
        let ids: Vec<Id> = span.scope().map(|span| span.id()).collect();
        // The span references are dropped before the OpenTelemetry layer locks span extensions.
        drop(span);
        ids.iter().find_map(|id| {
            let context = tracing_opentelemetry::get_otel_context(id, &dispatch)?;
            let span_context = context.span().span_context().clone();
            span_context.is_valid().then_some(span_context)
        })
    }
}

impl<S, W> Layer<S> for LogLayer<W>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    W: LineSink,
{
    fn on_register_dispatch(&self, dispatch: &Dispatch) {
        let _ = self.dispatch.set(dispatch.downgrade());
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        let metadata = event.metadata();
        let mut recorded = Recorded::default();
        event.record(&mut recorded);
        let target = recorded.log_target.as_deref().unwrap_or(metadata.target());
        // The filter judges by the event's own target; an event bridged from the `log` crate
        // carries its real target only as a field, so the decision is repeated here.
        if sdk_diagnostic_suppressed(target, metadata.level()) {
            return;
        }
        let span_context = self.span_context(event, &ctx);
        let now = SystemTime::now();

        let line = Line {
            timestamp: humantime::format_rfc3339_micros(now).to_string(),
            level: metadata.level().as_str(),
            service: self.service,
            target,
            trace_id: span_context
                .as_ref()
                .map(|context| format!("{:032x}", context.trace_id())),
            span_id: span_context
                .as_ref()
                .map(|context| format!("{:016x}", context.span_id())),
            message: &recorded.message,
            fields: FieldMap(&recorded.fields),
        };
        if let Ok(mut bytes) = serde_json::to_vec(&line) {
            bytes.push(b'\n');
            self.sink.write_line(&bytes);
        }

        // The SDK's own diagnostics stay on stdout: exporting them could loop while the export
        // they describe is failing.
        if let Some(logger) = &self.otel
            && !is_sdk_target(target)
        {
            let mut record = logger.create_log_record();
            record.set_timestamp(now);
            record.set_observed_timestamp(now);
            record.set_severity_number(severity(metadata.level()));
            record.set_severity_text(metadata.level().as_str());
            record.set_target(target.to_owned());
            record.set_body(AnyValue::String(StringValue::from(recorded.message)));
            record.add_attributes(
                recorded
                    .fields
                    .iter()
                    .map(|(name, value)| (Key::from_static_str(name), AnyValue::from(value))),
            );
            if let Some(context) = &span_context {
                record.set_trace_context(
                    context.trace_id(),
                    context.span_id(),
                    Some(context.trace_flags()),
                );
            }
            logger.emit(record);
        }
    }
}

impl<W> Debug for LogLayer<W> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LogLayer")
            .field("service", &self.service)
            .field("otel", &self.otel.is_some())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::test_support::{capture_subscriber, lines};

    /// One event of each level on `target`; the debug and trace ones carry a URL with a canary.
    macro_rules! emit_all_levels {
        ($target:literal) => {
            tracing::error!(target: $target, "kept");
            tracing::warn!(target: $target, "kept");
            tracing::info!(target: $target, "kept");
            tracing::debug!(target: $target, url = "http://c/canary", "x");
            tracing::trace!(target: $target, url = "http://c/canary", "x");
        };
    }

    #[test]
    fn events_become_json_lines_with_redacted_fields() {
        let (subscriber, capture, _provider) = capture_subscriber("info");
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(
                job = "purge",
                attempt = 3_u64,
                ratio = 0.5,
                ok = true,
                password = "hunter2",
                request_body = "{\"email\":\"alice@example.com\"}",
                "job {} finished",
                "purge"
            );
            tracing::debug!("filtered out at info");
        });
        let lines = lines(&capture);
        assert_eq!(lines.len(), 1, "{lines:?}");
        let line = &lines[0];
        assert_eq!(line["level"], "INFO");
        assert_eq!(line["service"], "unit-test");
        assert_eq!(line["message"], "job purge finished");
        assert_eq!(line["fields"]["job"], "purge");
        assert_eq!(line["fields"]["attempt"], 3);
        assert_eq!(line["fields"]["ratio"], 0.5);
        assert_eq!(line["fields"]["ok"], true);
        assert_eq!(line["fields"]["password"], REDACTED);
        assert_eq!(line["fields"]["request_body"], REDACTED);
        assert!(
            line.get("trace_id").is_none(),
            "no span, no trace id: {line}"
        );
        let raw = String::from_utf8(capture.0.lock().expect("capture lock").clone())
            .expect("JSON is UTF-8");
        assert!(!raw.contains("hunter2"), "{raw}");
        assert!(!raw.contains("alice@example.com"), "{raw}");
        assert!(
            humantime::parse_rfc3339(line["timestamp"].as_str().expect("timestamp string")).is_ok(),
            "{line}"
        );
    }

    #[test]
    fn events_in_spans_carry_the_span_trace_and_span_ids() {
        let (subscriber, capture, _provider) = capture_subscriber("info");
        tracing::subscriber::with_default(subscriber, || {
            let outer = tracing::info_span!("outer");
            let _outer = outer.enter();
            // A debug span is not created at all; events inside it belong to `outer`.
            let inner = tracing::debug_span!("inner");
            let _inner = inner.enter();
            tracing::warn!("inside");
            tracing::info!(parent: &outer, "explicit parent");
        });
        let lines = lines(&capture);
        assert_eq!(lines.len(), 2, "{lines:?}");
        let trace_id = lines[0]["trace_id"].as_str().expect("trace id");
        assert_eq!(trace_id.len(), 32);
        assert!(
            trace_id.bytes().all(|b| b.is_ascii_hexdigit()),
            "{trace_id}"
        );
        assert_ne!(trace_id, "0".repeat(32));
        assert_eq!(lines[0]["span_id"].as_str().map(str::len), Some(16));
        assert_eq!(lines[1]["trace_id"], lines[0]["trace_id"]);
        assert_eq!(lines[1]["span_id"], lines[0]["span_id"]);
    }

    #[test]
    fn trace_ids_survive_a_filter_above_the_span_level() {
        // Operators may raise the filter to `warn`; warnings inside a request must still carry
        // the request's trace id.
        let (subscriber, capture, _provider) = capture_subscriber("warn");
        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!("http.request");
            let _entered = span.enter();
            tracing::info!("dropped");
            tracing::warn!("kept");
        });
        let lines = lines(&capture);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert_eq!(lines[0]["message"], "kept");
        assert!(lines[0]["trace_id"].is_string(), "{}", lines[0]);
    }

    #[test]
    fn sdk_diagnostics_below_info_are_dropped_at_every_filter() {
        // `trace` for everything, including each OpenTelemetry crate by name.
        let (subscriber, capture, _provider) = capture_subscriber(
            "trace,opentelemetry=trace,opentelemetry_sdk=trace,opentelemetry-otlp=trace,\
             opentelemetry-http=trace",
        );
        tracing::subscriber::with_default(subscriber, || {
            emit_all_levels!("opentelemetry");
            emit_all_levels!("opentelemetry_sdk");
            emit_all_levels!("opentelemetry-otlp");
            emit_all_levels!("opentelemetry-http");
            // Other targets are not affected.
            tracing::debug!(target: "sqlx::query", "own debug");
            tracing::trace!("own trace");
        });
        let lines = lines(&capture);
        let kept: Vec<&str> = lines
            .iter()
            .filter_map(|line| line["message"].as_str())
            .collect();
        assert_eq!(
            kept.iter().filter(|m| **m == "kept").count(),
            4 * 3,
            "{lines:?}"
        );
        assert!(
            kept.contains(&"own debug") && kept.contains(&"own trace"),
            "{kept:?}"
        );
        let raw = String::from_utf8(capture.0.lock().expect("capture lock").clone())
            .expect("JSON is UTF-8");
        assert!(!raw.contains("canary"), "{raw}");
    }

    /// Sends one record through `tracing-log`'s `LogTracer`, the bridge `Telemetry::init`
    /// installs for the `log` crate, as `log::debug!(target: .., ..)` in a dependency would.
    fn bridge(target: &str, level: log::Level, message: &str) {
        use log::Log as _;
        tracing_log::LogTracer::new().log(
            &log::Record::builder()
                .target(target)
                .level(level)
                .args(format_args!("{message}"))
                .build(),
        );
    }

    /// The messages of the captured lines.
    fn messages(capture: &Capture) -> Vec<String> {
        lines(capture)
            .iter()
            .filter_map(|line| line["message"].as_str().map(str::to_owned))
            .collect()
    }

    #[test]
    fn bridged_log_events_are_judged_by_their_original_target() {
        let (subscriber, capture, _provider) = capture_subscriber("trace");
        tracing::subscriber::with_default(subscriber, || {
            bridge("opentelemetry-otlp", log::Level::Debug, "bridged sdk debug");
            bridge("opentelemetry-otlp", log::Level::Warn, "bridged sdk warn");
            bridge("reqwest::connect", log::Level::Debug, "bridged other");
        });
        assert_eq!(
            messages(&capture),
            ["bridged sdk warn", "bridged other"],
            "{:?}",
            lines(&capture)
        );
    }

    #[test]
    fn target_directives_suppress_bridged_log_events() {
        let (subscriber, capture, _provider) = capture_subscriber("debug,reqwest=warn");
        tracing::subscriber::with_default(subscriber, || {
            bridge("reqwest::connect", log::Level::Debug, "reqwest debug");
            bridge("reqwest::connect", log::Level::Info, "reqwest info");
            bridge("reqwest", log::Level::Warn, "reqwest warn");
            bridge("hyper_util::client", log::Level::Debug, "hyper debug");
            // The same directive applies to events from the same crate sent through tracing.
            tracing::debug!(target: "reqwest::native", "native reqwest debug");
        });
        let lines = lines(&capture);
        assert_eq!(
            messages(&capture),
            ["reqwest warn", "hyper debug"],
            "{lines:?}"
        );
        // The line names the dependency's own target, not the bridge's.
        assert_eq!(lines[0]["target"], "reqwest");
        assert_eq!(lines[1]["target"], "hyper_util::client");
    }

    #[test]
    fn target_directives_allow_bridged_log_events_above_the_default() {
        let (subscriber, capture, _provider) = capture_subscriber("warn,reqwest=debug,hyper=off");
        tracing::subscriber::with_default(subscriber, || {
            bridge("reqwest::connect", log::Level::Debug, "reqwest debug");
            bridge("reqwest::connect", log::Level::Trace, "reqwest trace");
            bridge("other", log::Level::Info, "other info");
            bridge("other", log::Level::Error, "other error");
            bridge("hyper::proto", log::Level::Error, "hyper error");
        });
        assert_eq!(
            messages(&capture),
            ["reqwest debug", "other error"],
            "{:?}",
            lines(&capture)
        );
    }

    #[test]
    fn bridged_log_events_keep_redaction() {
        let (subscriber, capture, _provider) = capture_subscriber("debug,reqwest=warn");
        tracing::subscriber::with_default(subscriber, || {
            // A suppressed bridged event leaves no trace; native events of the same target
            // are still redacted.
            bridge("reqwest", log::Level::Debug, "suppressed canary");
            bridge("reqwest", log::Level::Warn, "warn kept");
            tracing::warn!(target: "reqwest", password = "canary-field", "native");
        });
        let raw = String::from_utf8(capture.0.lock().expect("capture lock").clone())
            .expect("JSON is UTF-8");
        assert!(!raw.contains("canary"), "{raw}");
        assert_eq!(messages(&capture), ["warn kept", "native"]);
    }

    #[test]
    fn errors_record_their_source_chain() {
        #[derive(Debug)]
        struct Outer(std::io::Error);
        impl fmt::Display for Outer {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("connect failed")
            }
        }
        impl std::error::Error for Outer {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                Some(&self.0)
            }
        }
        let (subscriber, capture, _provider) = capture_subscriber("info");
        let error = Outer(std::io::Error::other("refused"));
        tracing::subscriber::with_default(subscriber, || {
            tracing::error!(
                error = &error as &(dyn std::error::Error + 'static),
                "failed"
            );
        });
        let lines = lines(&capture);
        assert_eq!(lines[0]["fields"]["error"], "connect failed: refused");
    }
}
