//! Error responses, in the error envelope of the protocol the caller speaks, so the caller's
//! provider SDK reads the gateway's errors like the provider's own (README.md, 「錯誤」).
//!
//! A provider's error keeps its status, type and message, with the gateway's provider keys
//! scrubbed from the message; the gateway's own errors use the provider's error types for the
//! same situation. Nothing here quotes the caller's request.

use std::time::Duration;

use ai_clients::{ApiError, Error, ErrorClass};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};

use crate::config::Protocol;
use crate::gateway::Keys;

/// The longest `retry-after` passed on, in seconds.
const MAX_RETRY_AFTER_SECS: u64 = 60 * 60;
/// How many characters of a provider's error message are passed on.
const MAX_MESSAGE_CHARS: usize = 2048;

/// What went wrong, for the gateway's own errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// The request cannot be forwarded as it is: 400.
    InvalidRequest,
    /// No such model role: 404.
    NotFound,
    /// The request body is over `max_request_bytes`: 413.
    TooLarge,
    /// No model answered in time: 504.
    Timeout,
    /// Every model of the role is at its concurrency limit: Anthropic 529, OpenAI 503.
    Overloaded,
    /// The provider could not be reached, refused the gateway's credentials, or sent a
    /// response that cannot be used: 502.
    BadGateway,
    /// The model role settings could not be checked (the database is unreachable or too slow),
    /// so no provider was called: 503.
    Unavailable,
}

impl Kind {
    fn status(self, protocol: Protocol) -> StatusCode {
        match (self, protocol) {
            (Self::InvalidRequest, _) => StatusCode::BAD_REQUEST,
            (Self::NotFound, _) => StatusCode::NOT_FOUND,
            (Self::TooLarge, _) => StatusCode::PAYLOAD_TOO_LARGE,
            (Self::Timeout, _) => StatusCode::GATEWAY_TIMEOUT,
            (Self::Overloaded, Protocol::AnthropicMessages) => overloaded(),
            (Self::Overloaded, Protocol::OpenaiDecisions) => StatusCode::SERVICE_UNAVAILABLE,
            (Self::BadGateway, _) => StatusCode::BAD_GATEWAY,
            (Self::Unavailable, _) => StatusCode::SERVICE_UNAVAILABLE,
        }
    }

    /// The error `type` (and for OpenAI the `code`) the provider uses for the same situation.
    fn wire_type(self, protocol: Protocol) -> (&'static str, Option<&'static str>) {
        match protocol {
            Protocol::AnthropicMessages => (
                match self {
                    Self::InvalidRequest => "invalid_request_error",
                    Self::NotFound => "not_found_error",
                    Self::TooLarge => "request_too_large",
                    Self::Timeout => "timeout_error",
                    Self::Overloaded => "overloaded_error",
                    Self::BadGateway | Self::Unavailable => "api_error",
                },
                None,
            ),
            Protocol::OpenaiDecisions => match self {
                Self::InvalidRequest | Self::TooLarge => ("invalid_request_error", None),
                Self::NotFound => ("invalid_request_error", Some("role_not_found")),
                Self::Timeout | Self::Overloaded | Self::BadGateway | Self::Unavailable => {
                    ("server_error", None)
                }
            },
        }
    }
}

/// Anthropic's "overloaded" status.
fn overloaded() -> StatusCode {
    StatusCode::from_u16(529).unwrap_or(StatusCode::SERVICE_UNAVAILABLE)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Origin {
    Gateway(Kind),
    /// A provider's error, passed on.
    Provider {
        status: StatusCode,
        error_type: Option<String>,
        code: Option<String>,
    },
}

/// An error response, rendered for the caller's protocol by [`Rejection::render`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejection {
    origin: Origin,
    message: String,
    retry_after: Option<u64>,
}

impl Rejection {
    pub fn new(kind: Kind, message: impl Into<String>) -> Self {
        Self {
            origin: Origin::Gateway(kind),
            message: message.into(),
            retry_after: None,
        }
    }

    /// The response for an error of the last model tried: a provider error as the provider
    /// sent it (minus credentials), anything else as the gateway's own error.
    pub fn from_error(error: &Error, keys: &Keys) -> Self {
        match error {
            Error::Api(api) | Error::StreamError(api) => Self::from_api(api, keys),
            Error::Timeout(_) => Self::new(Kind::Timeout, "the model did not answer in time"),
            Error::ConcurrencyLimited { .. } => Self::new(
                Kind::Overloaded,
                "every model of this role is at its concurrency limit",
            ),
            // The clients' own checks of the request (a Decisions request that does not
            // validate): the caller's data, never a key, but scrubbed all the same.
            Error::InvalidRequest(message) => Self::new(Kind::InvalidRequest, keys.scrub(message)),
            Error::Transport(_) => Self::new(Kind::BadGateway, "the provider could not be reached"),
            Error::Protocol(_)
            | Error::InvalidResponse { .. }
            | Error::Sse(_)
            | Error::Cancelled => Self::new(
                Kind::BadGateway,
                "the provider's response could not be used",
            ),
        }
    }

    fn from_api(api: &ApiError, keys: &Keys) -> Self {
        let status = match api.status {
            // The gateway's key was refused: the caller cannot fix that, and the provider's
            // message may describe the key.
            Some(401 | 403) => {
                return Self::new(
                    Kind::BadGateway,
                    "the provider refused the gateway's credentials",
                );
            }
            Some(status @ 400..=599) => StatusCode::from_u16(status).ok(),
            Some(_) => None,
            // An error event that ended a stream before its first output.
            None => match api.class {
                ErrorClass::RateLimited { .. } => Some(StatusCode::TOO_MANY_REQUESTS),
                ErrorClass::Overloaded => Some(overloaded()),
                ErrorClass::Timeout(_) => Some(StatusCode::GATEWAY_TIMEOUT),
                ErrorClass::ConcurrencyLimited | ErrorClass::Cancelled | ErrorClass::Other => None,
            },
        };
        let Some(status) = status else {
            return Self::new(Kind::BadGateway, "the provider answered with an error");
        };
        Self {
            origin: Origin::Provider {
                status,
                error_type: api.body.error_type.as_deref().map(|t| keys.scrub_field(t)),
                code: api.body.code.as_deref().map(|code| keys.scrub_field(code)),
            },
            message: truncate(&keys.scrub(&api.body.message)),
            retry_after: api.retry_after.map(retry_after_secs),
        }
    }

    /// The HTTP status of the response.
    pub fn status(&self, protocol: Protocol) -> StatusCode {
        match &self.origin {
            Origin::Gateway(kind) => kind.status(protocol),
            Origin::Provider { status, .. } => *status,
        }
    }

    /// The response in `protocol`'s error envelope:
    /// Anthropic `{"type":"error","error":{"type":…,"message":…}}`, OpenAI
    /// `{"error":{"message":…,"type":…,"param":null,"code":…}}`.
    pub fn render(&self, protocol: Protocol) -> Response {
        let (error_type, code) = match &self.origin {
            Origin::Gateway(kind) => {
                let (error_type, code) = kind.wire_type(protocol);
                (Some(error_type.to_owned()), code.map(str::to_owned))
            }
            Origin::Provider {
                error_type, code, ..
            } => (error_type.clone(), code.clone()),
        };
        let body = match protocol {
            Protocol::AnthropicMessages => serde_json::json!({
                "type": "error",
                "error": {
                    "type": error_type.unwrap_or_else(|| "api_error".to_owned()),
                    "message": self.message,
                },
            }),
            Protocol::OpenaiDecisions => serde_json::json!({
                "error": {
                    "message": self.message,
                    "type": error_type,
                    "param": null,
                    "code": code,
                },
            }),
        };
        let mut response = (self.status(protocol), body.to_string()).into_response();
        let headers = response.headers_mut();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        if let Some(seconds) = self.retry_after {
            headers.insert(header::RETRY_AFTER, HeaderValue::from(seconds));
        }
        response
    }
}

/// Whole seconds, rounded up and at most an hour: a provider's value is passed on, never used
/// for the gateway's own waits.
fn retry_after_secs(wait: Duration) -> u64 {
    let seconds = wait
        .as_secs()
        .saturating_add(u64::from(wait.subsec_nanos() > 0));
    seconds.min(MAX_RETRY_AFTER_SECS)
}

fn truncate(text: &str) -> String {
    if text.chars().nth(MAX_MESSAGE_CHARS).is_none() {
        return text.to_owned();
    }
    let mut cut: String = text.chars().take(MAX_MESSAGE_CHARS).collect();
    cut.push('…');
    cut
}

#[cfg(test)]
mod tests {
    use ai_clients::{ApiErrorBody, TimeoutKind};

    use super::*;

    fn api(status: Option<u16>, error_type: &str, message: &str, class: ErrorClass) -> Error {
        Error::Api(Box::new(ApiError {
            status,
            body: ApiErrorBody {
                error_type: Some(error_type.to_owned()),
                code: None,
                message: message.to_owned(),
            },
            request_id: None,
            retry_after: Some(Duration::from_millis(16_500)),
            class,
        }))
    }

    async fn json(response: Response) -> serde_json::Value {
        let body = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        serde_json::from_slice(&body).unwrap()
    }

    #[tokio::test]
    async fn provider_errors_keep_status_type_and_message_without_keys() {
        // Made at run time, so no key-like literal sits in the repository.
        let key = ["test", "key", &std::process::id().to_string()].join("-");
        let keys = Keys::new(vec![key.clone()]);
        let error = api(
            Some(429),
            "rate_limit_error",
            &format!("slow down, {key}"),
            ErrorClass::RateLimited { retry_after: None },
        );
        let response = Rejection::from_error(&error, &keys).render(Protocol::AnthropicMessages);
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.headers()[header::RETRY_AFTER], "17");
        assert_eq!(
            json(response).await,
            serde_json::json!({
                "type": "error",
                "error": {"type": "rate_limit_error", "message": "slow down, [REDACTED]"},
            })
        );

        // The type and code are the provider's too.
        let error = Error::Api(Box::new(ApiError {
            status: Some(400),
            body: ApiErrorBody {
                error_type: Some(format!("type_{key}")),
                code: Some(format!("code_{key}")),
                message: "bad".to_owned(),
            },
            request_id: None,
            retry_after: None,
            class: ErrorClass::Other,
        }));
        let rejection = Rejection::from_error(&error, &keys);
        for protocol in [Protocol::AnthropicMessages, Protocol::OpenaiDecisions] {
            let body = json(rejection.render(protocol)).await.to_string();
            assert!(!body.contains(&key), "{body}");
            assert!(body.contains("type_[REDACTED]"), "{body}");
        }
        let openai = json(rejection.render(Protocol::OpenaiDecisions)).await;
        assert_eq!(openai["error"]["code"], "code_[REDACTED]");
    }

    #[tokio::test]
    async fn refused_credentials_and_odd_statuses_become_bad_gateway() {
        let keys = Keys::new(vec![]);
        for status in [401, 403, 302] {
            let error = api(
                Some(status),
                "authentication_error",
                "key xyz",
                ErrorClass::Other,
            );
            let response = Rejection::from_error(&error, &keys).render(Protocol::OpenaiDecisions);
            assert_eq!(response.status(), StatusCode::BAD_GATEWAY, "{status}");
            let body = json(response).await;
            assert_eq!(body["error"]["type"], "server_error");
            assert!(!body.to_string().contains("xyz"), "{body}");
        }
    }

    #[tokio::test]
    async fn gateway_errors_use_each_protocols_envelope() {
        let keys = Keys::new(vec![]);
        let busy = Error::ConcurrencyLimited {
            endpoint: "main:anthropic/claude-opus-5-5".into(),
        };
        let anthropic = Rejection::from_error(&busy, &keys).render(Protocol::AnthropicMessages);
        assert_eq!(anthropic.status().as_u16(), 529);
        assert_eq!(json(anthropic).await["error"]["type"], "overloaded_error");
        let openai = Rejection::from_error(&busy, &keys).render(Protocol::OpenaiDecisions);
        assert_eq!(openai.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = json(openai).await;
        assert_eq!(body["error"]["type"], "server_error");
        assert!(body["error"]["param"].is_null());

        let late = Error::Timeout(TimeoutKind::FirstToken);
        let response = Rejection::from_error(&late, &keys).render(Protocol::AnthropicMessages);
        assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
        assert_eq!(json(response).await["error"]["type"], "timeout_error");

        for protocol in [Protocol::AnthropicMessages, Protocol::OpenaiDecisions] {
            let response = Rejection::new(Kind::Unavailable, "settings").render(protocol);
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
            let body = json(response).await;
            let error_type = match protocol {
                Protocol::AnthropicMessages => "api_error",
                Protocol::OpenaiDecisions => "server_error",
            };
            assert_eq!(body["error"]["type"], error_type, "{body}");
        }

        let missing = Rejection::new(Kind::NotFound, "no such model role");
        let response = missing.render(Protocol::OpenaiDecisions);
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(json(response).await["error"]["code"], "role_not_found");
    }

    #[test]
    fn retry_after_is_whole_seconds_up_to_an_hour() {
        assert_eq!(retry_after_secs(Duration::from_secs(17)), 17);
        assert_eq!(retry_after_secs(Duration::from_millis(1)), 1);
        assert_eq!(retry_after_secs(Duration::MAX), MAX_RETRY_AFTER_SECS);
    }
}
