//! The HTTP server: request spans and access logs, a handler timeout, panic recovery, and the
//! health routes.
//!
//! Request logs and spans record the method, the matched route template (never the raw path or
//! query, which can carry identifiers or tokens), the status, and the duration. Headers and
//! bodies are never recorded. An incoming W3C `traceparent` header continues the caller's trace.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::{MatchedPath, Request, State};
use axum::http::HeaderMap;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use tokio_util::sync::CancellationToken;
use tower_http::catch_panic::CatchPanicLayer;
use tracing::Instrument;
use tracing_opentelemetry::OpenTelemetrySpanExt;

use crate::config::Invalid;
use crate::telemetry::duration_ms;

/// The longest `http.request_timeout` accepted: one hour. A larger value is a typo, and one beyond
/// what `tokio::time::Instant` can add silently disables the timeout.
pub const MAX_REQUEST_TIMEOUT: Duration = Duration::from_secs(60 * 60);

/// `http.*`: the HTTP server.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HttpConfig {
    /// The address to listen on. Default `0.0.0.0:8080`.
    pub listen_addr: SocketAddr,
    /// How long a handler may take to produce the response head; streamed bodies are not
    /// limited. A timed-out request gets 504, which the ingress does not retry, so a slow
    /// non-idempotent request is not executed twice. Default 30s.
    #[serde(deserialize_with = "crate::config::duration")]
    pub request_timeout: Duration,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            listen_addr: SocketAddr::from(([0, 0, 0, 0], 8080)),
            request_timeout: Duration::from_secs(30),
        }
    }
}

impl HttpConfig {
    pub(crate) fn validate(&self) -> Result<(), Invalid> {
        if self.request_timeout.is_zero() {
            return Err(Invalid::new(
                "http.request_timeout",
                "must be longer than zero",
            ));
        }
        if self.request_timeout > MAX_REQUEST_TIMEOUT {
            return Err(Invalid::new(
                "http.request_timeout",
                "must not be longer than 1 hour",
            ));
        }
        Ok(())
    }
}

/// Wraps the service's routes, including its fallback (the default 404 when it has none), in
/// the kit's layers and adds the health routes, which stay outside the logging and timeout layers
/// so probes neither flood the logs nor queue behind them. `halt` is cancelled when the drain
/// timeout runs out; handlers still running then are dropped and their requests answer 504.
pub(crate) fn compose(
    app: Router,
    health: Router,
    config: &HttpConfig,
    halt: CancellationToken,
) -> Router {
    let limits = Limits {
        request_timeout: config.request_timeout,
        halt,
    };
    let app = app
        .layer(middleware::from_fn_with_state(limits, enforce_limits))
        .layer(CatchPanicLayer::custom(|_| {
            // The panic hook has already logged the panic with its location.
            KitError::Internal.into_response()
        }))
        .layer(middleware::from_fn(trace_request));
    // `merge` keeps the fallback of its argument when neither router set one, so the service's
    // layered fallback must be the argument.
    health.merge(app)
}

/// An error the kit answers by itself, without the service's handler.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum KitError {
    /// The handler did not respond within `http.request_timeout`, or was still running when the
    /// drain timeout ran out: 504. Not 503, which the ingress retries, so a partly executed
    /// non-idempotent request is not run twice.
    Timeout,
    /// The handler panicked: 500.
    Internal,
}

impl IntoResponse for KitError {
    /// With the `contract` feature, the M1 error body of crates/contract with the matching code;
    /// without it (services/ai-gateway), the status alone.
    fn into_response(self) -> Response {
        #[cfg(feature = "contract")]
        {
            let body = contract::ErrorBody::new(match self {
                Self::Timeout => contract::ErrorCode::Timeout,
                Self::Internal => contract::ErrorCode::Internal,
            });
            (body.http_status(), axum::Json(body)).into_response()
        }
        #[cfg(not(feature = "contract"))]
        {
            match self {
                Self::Timeout => axum::http::StatusCode::GATEWAY_TIMEOUT,
                Self::Internal => axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            }
            .into_response()
        }
    }
}

struct HeaderExtractor<'a>(&'a HeaderMap);

impl opentelemetry::propagation::Extractor for HeaderExtractor<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(|value| value.to_str().ok())
    }

    fn keys(&self) -> Vec<&str> {
        self.0.keys().map(axum::http::HeaderName::as_str).collect()
    }
}

async fn trace_request(request: Request, next: Next) -> Response {
    let method = request.method().clone();
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map(|path| path.as_str().to_owned());
    let name = match &route {
        Some(route) => format!("{method} {route}"),
        None => method.to_string(),
    };
    let span = tracing::info_span!(
        "http.request",
        otel.name = %name,
        otel.kind = "server",
        otel.status_code = tracing::field::Empty,
        http.request.method = %method,
        http.route = route.as_deref(),
        http.response.status_code = tracing::field::Empty,
    );
    let parent = opentelemetry::global::get_text_map_propagator(|propagator| {
        propagator.extract(&HeaderExtractor(request.headers()))
    });
    // Fails only when the span is disabled, in which case there is nothing to continue.
    let _ = span.set_parent(parent);

    let started = Instant::now();
    let response = next.run(request).instrument(span.clone()).await;
    let status = response.status().as_u16();
    // Milliseconds with microsecond precision, so the value prints without float noise.
    let duration_ms = (started.elapsed().as_secs_f64() * 1_000_000.0).round() / 1000.0;
    span.record("http.response.status_code", status);
    if status >= 500 {
        span.record("otel.status_code", "error");
        tracing::error!(
            parent: &span,
            http.request.method = %method,
            http.route = route.as_deref(),
            http.response.status_code = status,
            duration_ms,
            "request failed"
        );
    } else {
        tracing::info!(
            parent: &span,
            http.request.method = %method,
            http.route = route.as_deref(),
            http.response.status_code = status,
            duration_ms,
            "request completed"
        );
    }
    response
}

/// How long a handler may run: `http.request_timeout`, and no longer than the drain allows.
#[derive(Clone)]
struct Limits {
    request_timeout: Duration,
    halt: CancellationToken,
}

async fn enforce_limits(State(limits): State<Limits>, request: Request, next: Next) -> Response {
    tokio::select! {
        outcome = tokio::time::timeout(limits.request_timeout, next.run(request)) => match outcome {
            Ok(response) => response,
            Err(_) => {
                tracing::warn!(
                    timeout_ms = duration_ms(limits.request_timeout),
                    "request timed out before the handler responded"
                );
                KitError::Timeout.into_response()
            }
        },
        () = limits.halt.cancelled() => {
            tracing::warn!("request cut off by the drain timeout");
            KitError::Timeout.into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;

    use super::*;

    async fn body_of(response: Response) -> (StatusCode, Option<String>, Vec<u8>) {
        let status = response.status();
        let content_type = response
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .expect("read the body");
        (status, content_type, body.to_vec())
    }

    #[cfg(feature = "contract")]
    #[tokio::test]
    async fn kit_errors_carry_the_m1_body() {
        for (error, status, code) in [
            (KitError::Timeout, StatusCode::GATEWAY_TIMEOUT, "timeout"),
            (
                KitError::Internal,
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
            ),
        ] {
            let (actual, content_type, body) = body_of(error.into_response()).await;
            assert_eq!(actual, status);
            assert_eq!(content_type.as_deref(), Some("application/json"));
            let body: serde_json::Value = serde_json::from_slice(&body).expect("JSON body");
            assert_eq!(
                body,
                serde_json::json!({ "code": code, "message_key": format!("error.{code}") })
            );
        }
    }

    #[cfg(not(feature = "contract"))]
    #[tokio::test]
    async fn kit_errors_are_status_only_without_contract() {
        for (error, status) in [
            (KitError::Timeout, StatusCode::GATEWAY_TIMEOUT),
            (KitError::Internal, StatusCode::INTERNAL_SERVER_ERROR),
        ] {
            let (actual, _, body) = body_of(error.into_response()).await;
            assert_eq!(actual, status);
            assert!(body.is_empty());
        }
    }

    #[test]
    fn request_timeout_must_be_positive() {
        let config = HttpConfig {
            request_timeout: Duration::ZERO,
            ..HttpConfig::default()
        };
        assert_eq!(
            config.validate().expect_err("zero timeout").key,
            "http.request_timeout"
        );
    }

    #[test]
    fn request_timeout_is_capped_at_one_hour() {
        let at_cap = HttpConfig {
            request_timeout: MAX_REQUEST_TIMEOUT,
            ..HttpConfig::default()
        };
        assert!(at_cap.validate().is_ok());
        for request_timeout in [MAX_REQUEST_TIMEOUT + Duration::from_secs(1), Duration::MAX] {
            let config = HttpConfig {
                request_timeout,
                ..HttpConfig::default()
            };
            let error = config.validate().expect_err("over the cap");
            assert_eq!(error.key, "http.request_timeout");
            assert!(error.message.contains("1 hour"), "{}", error.message);
        }
    }
}
