//! What every provider client shares: the endpoint configuration and its validation, the API
//! key, the concurrency limit, time limits, and bounded body reads.

use std::fmt;
use std::future::Future;
use std::net::IpAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use reqwest::header::HeaderValue;
use reqwest::{StatusCode, Url};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::Instant;
use zeroize::Zeroizing;

use crate::error::{ApiError, ApiErrorBody, Error, ErrorClass, TimeoutKind};

/// A provider API key. `Debug` never shows it, it is wiped from memory on drop, and its header
/// is marked sensitive so HTTP tooling does not log it. Only this stored copy is wiped: the
/// `HeaderValue` built for each request and the HTTP stack's write buffers hold copies that
/// are freed without being cleared.
pub struct ApiKey(Zeroizing<String>);

impl ApiKey {
    /// Rejects an empty key and characters that cannot appear in an HTTP header.
    pub fn new(key: impl Into<String>) -> Result<Self, Error> {
        let key = Zeroizing::new(key.into());
        if key.is_empty() || !key.bytes().all(|byte| byte.is_ascii_graphic()) {
            return Err(Error::InvalidRequest(
                "an API key must be non-empty printable ASCII without spaces".into(),
            ));
        }
        Ok(Self(key))
    }

    /// The header value `<prefix><key>`, marked sensitive.
    pub(crate) fn header_value(&self, prefix: &str) -> Result<HeaderValue, Error> {
        let text = Zeroizing::new(format!("{prefix}{}", self.0.as_str()));
        let mut value = HeaderValue::from_str(&text)
            .map_err(|_| Error::InvalidRequest("the API key is not a valid header value".into()))?;
        value.set_sensitive(true);
        Ok(value)
    }
}

impl fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ApiKey([redacted])")
    }
}

/// Time limits of one endpoint ([`TimeoutKind`] says what each one means). The defaults are
/// starting values; the AI gateway sets them per model role (Laysi/trustiot-v2#139).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Timeouts {
    pub connect: Duration,
    pub first_token: Duration,
    pub stream_idle: Duration,
    /// The longest a stream may run, counted from sending the request. Without it, an
    /// upstream (or a proxy) that keeps sending pings or tiny deltas inside `stream_idle`
    /// would hold the concurrency slot forever.
    pub stream_total: Duration,
    pub response: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            connect: Duration::from_secs(10),
            first_token: Duration::from_secs(60),
            stream_idle: Duration::from_secs(60),
            // Long enough for a large response with extended thinking: 128k output tokens at
            // a slow 50 tokens per second take about 43 minutes.
            stream_total: Duration::from_secs(60 * 60),
            response: Duration::from_secs(300),
        }
    }
}

/// One provider endpoint: where it is, how many requests may be in flight, and the limits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointConfig {
    /// A short name for errors and logs, such as `anthropic`.
    pub name: String,
    /// `https://` origin of the API, such as `https://api.anthropic.com`. Plain `http://` is
    /// accepted only for loopback addresses (tests and a local proxy on the same host).
    pub base_url: String,
    /// Requests in flight at most; a streaming request holds its slot until the stream ends,
    /// fails, is cancelled or dropped.
    pub max_concurrent: usize,
    /// How long a request waits for a free slot; zero refuses at once.
    pub queue_wait: Duration,
    pub timeouts: Timeouts,
    /// Size limit of a response body: a non-streaming body, and every byte of a stream
    /// (events, pings and comments together), which bounds what `MessageStream` accumulates.
    /// Error bodies are read up to 64 KiB at most.
    pub max_response_bytes: usize,
    /// Size limit of one server-sent event.
    pub max_event_bytes: usize,
}

impl EndpointConfig {
    pub fn new(name: impl Into<String>, base_url: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            base_url: base_url.into(),
            max_concurrent: 32,
            queue_wait: Duration::ZERO,
            timeouts: Timeouts::default(),
            max_response_bytes: 32 << 20,
            // A real event is a few KiB (a delta, a whole tool call); the largest are server
            // tool results that arrive whole in `content_block_start` (a fetched document, code
            // output), which stay well under this. Raise it per endpoint if a tool needs more.
            max_event_bytes: 4 << 20,
        }
    }
}

/// Error bodies are read up to this many bytes, whatever the response limit.
const MAX_ERROR_BODY_BYTES: usize = 64 << 10;

pub(crate) struct Transport {
    pub(crate) http: reqwest::Client,
    base: String,
    limiter: Arc<Semaphore>,
    pub(crate) config: EndpointConfig,
}

impl Transport {
    /// Builds the HTTP client from the caller's builder (TLS backend and trust anchors are the
    /// caller's choice), adding the connect timeout, TCP keepalive and no redirects: a redirect
    /// would resend the API key (Anthropic's `x-api-key` is not a header reqwest strips on a
    /// cross-origin redirect) to wherever the response points. A plain `http://` (loopback)
    /// endpoint gets no proxy at all: reqwest applies `HTTP_PROXY`/`ALL_PROXY` without a
    /// loopback exemption, which would send the request, API key included, in clear text to the
    /// proxy. `https://` keeps the builder's proxies (an egress proxy tunnels TLS end to end).
    pub(crate) fn new(
        config: EndpointConfig,
        builder: reqwest::ClientBuilder,
    ) -> Result<Self, Error> {
        let base = validate_base_url(&config.base_url)?;
        if config.max_concurrent == 0 || config.max_concurrent > Semaphore::MAX_PERMITS {
            return Err(Error::InvalidRequest(format!(
                "max_concurrent of endpoint {} must be between 1 and {}",
                config.name,
                Semaphore::MAX_PERMITS
            )));
        }
        let builder = if base.starts_with("http://") {
            builder.no_proxy()
        } else {
            builder
        };
        let http = builder
            .connect_timeout(config.timeouts.connect)
            .tcp_keepalive(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(Error::Transport)?;
        Ok(Self {
            http,
            base,
            limiter: Arc::new(Semaphore::new(config.max_concurrent)),
            config,
        })
    }

    pub(crate) fn url(&self, path: &str) -> Result<Url, Error> {
        Url::parse(&format!("{}{path}", self.base))
            .map_err(|error| Error::InvalidRequest(format!("endpoint URL: {error}")))
    }

    /// Takes a slot of the endpoint's concurrency limit, waiting at most `queue_wait`.
    pub(crate) async fn acquire(&self) -> Result<OwnedSemaphorePermit, Error> {
        let limited = || Error::ConcurrencyLimited {
            endpoint: self.config.name.clone(),
        };
        if self.config.queue_wait.is_zero() {
            return Arc::clone(&self.limiter)
                .try_acquire_owned()
                .map_err(|_| limited());
        }
        match tokio::time::timeout(
            self.config.queue_wait,
            Arc::clone(&self.limiter).acquire_owned(),
        )
        .await
        {
            Ok(Ok(permit)) => Ok(permit),
            Ok(Err(_)) | Err(_) => Err(limited()),
        }
    }

    /// Sends the request; the response headers must arrive before `deadline`.
    pub(crate) async fn send(
        &self,
        request: reqwest::RequestBuilder,
        deadline: Instant,
        kind: TimeoutKind,
    ) -> Result<reqwest::Response, Error> {
        match tokio::time::timeout_at(deadline, request.send()).await {
            Ok(result) => result.map_err(Error::from_reqwest),
            Err(_) => Err(Error::Timeout(kind)),
        }
    }
}

/// `https://` anywhere, `http://` only on loopback; no credentials, query or fragment. Returns
/// the URL without a trailing slash.
fn validate_base_url(text: &str) -> Result<String, Error> {
    let invalid = |why: &str| Error::InvalidRequest(format!("endpoint URL {text:?}: {why}"));
    let url = Url::parse(text).map_err(|error| invalid(&error.to_string()))?;
    let Some(host) = url.host_str() else {
        return Err(invalid("no host"));
    };
    // IPv6 hosts come in brackets.
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    let loopback = host.eq_ignore_ascii_case("localhost")
        || bare.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback());
    match url.scheme() {
        "https" => {}
        "http" if loopback => {}
        _ => return Err(invalid("must be https (plain http only on loopback)")),
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(invalid("must not contain credentials"));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(invalid("must not have a query or fragment"));
    }
    Ok(url.as_str().trim_end_matches('/').to_owned())
}

/// A source of response body chunks. The clients read `reqwest::Response`; tests and relays
/// can feed recorded bytes.
pub trait ByteSource: Send {
    /// The next chunk, or `None` at the end of the body.
    fn next_chunk(&mut self) -> ChunkFuture<'_>;
}

pub type ChunkFuture<'a> = Pin<Box<dyn Future<Output = Result<Option<Bytes>, Error>> + Send + 'a>>;

impl ByteSource for reqwest::Response {
    fn next_chunk(&mut self) -> ChunkFuture<'_> {
        Box::pin(async move { self.chunk().await.map_err(Error::from_reqwest) })
    }
}

/// Reads the whole body, at most `limit` bytes, before `deadline`.
pub(crate) async fn read_body(
    source: &mut dyn ByteSource,
    limit: usize,
    deadline: Instant,
    kind: TimeoutKind,
) -> Result<Vec<u8>, Error> {
    let mut body = Vec::new();
    loop {
        let chunk = match tokio::time::timeout_at(deadline, source.next_chunk()).await {
            Ok(chunk) => chunk?,
            Err(_) => return Err(Error::Timeout(kind)),
        };
        let Some(chunk) = chunk else {
            return Ok(body);
        };
        if body.len().saturating_add(chunk.len()) > limit {
            return Err(Error::Protocol(format!(
                "the response body exceeds {limit} bytes"
            )));
        }
        body.extend_from_slice(&chunk);
    }
}

/// The value of `name`, if it is visible ASCII.
pub(crate) fn header_text(response: &reqwest::Response, name: &str) -> Option<String> {
    response
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

/// `retry-after-ms` (milliseconds) or `retry-after` (whole seconds); an HTTP date is ignored.
pub(crate) fn retry_after(response: &reqwest::Response) -> Option<Duration> {
    if let Some(ms) = header_text(response, "retry-after-ms").and_then(|v| v.trim().parse().ok()) {
        return Some(Duration::from_millis(ms));
    }
    header_text(response, "retry-after")
        .and_then(|v| v.trim().parse().ok())
        .map(Duration::from_secs)
}

/// Turns an error response into [`Error::Api`], reading at most a small body before the
/// deadline. A body that cannot be read or parsed leaves only the status to classify on.
pub(crate) async fn error_from_response(
    mut response: reqwest::Response,
    request_id_header: &str,
    deadline: Instant,
    classify: fn(StatusCode, &ApiErrorBody) -> ErrorClass,
) -> Error {
    let status = response.status();
    let request_id = header_text(&response, request_id_header);
    let retry_after = retry_after(&response);
    let body = read_body(
        &mut response,
        MAX_ERROR_BODY_BYTES,
        deadline,
        TimeoutKind::Response,
    )
    .await
    .map(|bytes| parse_error_body(&bytes))
    .unwrap_or_default();
    let mut class = classify(status, &body);
    if let ErrorClass::RateLimited { retry_after: slot } = &mut class {
        *slot = retry_after;
    }
    Error::Api(Box::new(ApiError {
        status: Some(status.as_u16()),
        body,
        request_id,
        retry_after,
        class,
    }))
}

/// `{"error": {...}}` (both providers); otherwise the start of the text as the message.
pub(crate) fn parse_error_body(bytes: &[u8]) -> ApiErrorBody {
    #[derive(serde::Deserialize)]
    struct Envelope {
        error: ApiErrorBody,
    }
    if let Ok(envelope) = serde_json::from_slice::<Envelope>(bytes) {
        return envelope.error;
    }
    let text = String::from_utf8_lossy(bytes);
    ApiErrorBody {
        message: text.chars().take(512).collect(),
        ..ApiErrorBody::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_urls_must_be_https_except_on_loopback() {
        for ok in [
            "https://api.anthropic.com",
            "https://api.openai.com/",
            "https://proxy.example/anthropic/",
            "http://127.0.0.1:8080",
            "http://[::1]:1",
            "http://localhost:9",
        ] {
            assert!(validate_base_url(ok).is_ok(), "{ok} should be accepted");
        }
        for bad in [
            "http://api.anthropic.com",
            "http://10.0.0.1",
            "ftp://127.0.0.1",
            "https://user:pass@api.anthropic.com",
            "https://api.anthropic.com/?key=1",
            "not a url",
        ] {
            assert!(validate_base_url(bad).is_err(), "{bad} should be refused");
        }
        assert_eq!(
            validate_base_url("https://proxy.example/anthropic/").unwrap(),
            "https://proxy.example/anthropic"
        );
    }

    #[test]
    fn api_keys_never_show_in_debug_and_reject_bad_characters() {
        let key = ApiKey::new(["sk", "test", "value"].join("-")).unwrap();
        assert_eq!(format!("{key:?}"), "ApiKey([redacted])");
        assert!(key.header_value("Bearer ").unwrap().is_sensitive());
        assert!(ApiKey::new("").is_err());
        assert!(ApiKey::new("with space").is_err());
        assert!(ApiKey::new("line\nbreak").is_err());
    }

    #[test]
    fn error_bodies_parse_from_either_provider_or_fall_back_to_text() {
        let anthropic = parse_error_body(
            br#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"},"request_id":"req_1"}"#,
        );
        assert_eq!(anthropic.error_type.as_deref(), Some("overloaded_error"));
        let openai = parse_error_body(
            br#"{"error":{"message":"quota","type":"insufficient_quota","param":null,"code":"insufficient_quota"}}"#,
        );
        assert_eq!(openai.code.as_deref(), Some("insufficient_quota"));
        let html = parse_error_body(b"<html>bad gateway</html>");
        assert_eq!(html.error_type, None);
        assert_eq!(html.message, "<html>bad gateway</html>");
    }
}
