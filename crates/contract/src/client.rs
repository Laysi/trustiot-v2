//! Typed HTTP/JSON client for synchronous internal API calls (decision 4).
//!
//! Services read from each other over HTTP/JSON inside the mesh. Istio ambient's ztunnel adds
//! mutual TLS and enforces which service may call which (decision 27), so the client speaks
//! plain HTTP/1.1: one request per connection at a time lets ztunnel spread calls over a
//! peer's replicas, which one multiplexed connection would not. Each internal API is an
//! [`Endpoint`] in this crate, and an [`InternalClient<S>`] calls the endpoints of service `S`.
//!
//! - A timeout is mandatory: [`InternalClient::new`] takes one and rejects zero or anything
//!   above [`MAX_TIMEOUT`]. It bounds the whole call, from connecting to the last body byte.
//! - Every failure is a [`CallError`] that maps to an M1 code ([`CallError::code`],
//!   [`CallError::to_error_body`]) and is logged with the peer service and the kind of failure.
//!   ztunnel rejects a call that no AuthorizationPolicy allows by resetting the connection, not
//!   with HTTP 403 (`docs/feasibility/2026-10-10-edge-poc.md`), so a permission problem
//!   surfaces as [`CallErrorKind::ConnectionReset`] rather than as a status code.
//! - Calls are never retried here; a caller that wants a retry decides so explicitly. Only
//!   reads cross services synchronously; writes go through events.

use std::error::Error as StdError;
use std::fmt;
use std::io;
use std::marker::PhantomData;
use std::time::Duration;

use http::StatusCode;
use http::header::{ACCEPT, CONTENT_TYPE, HeaderValue};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::error::{ErrorBody, ErrorCode};
use crate::identity::{INTERNAL_IDENTITY_HEADER, InternalToken};
use crate::json::JsonError;
use crate::service::{Service, ServiceName};

/// Every internal route starts with this prefix, which keeps internal APIs apart from the
/// public API that the entry gateway routes.
pub const ROUTE_PREFIX: &str = "/internal/";

/// The longest timeout a client accepts. Internal reads answer in milliseconds; a longer
/// timeout would only hold the user's request open behind a stuck peer.
pub const MAX_TIMEOUT: Duration = Duration::from_secs(60);

/// The default limit on a successful response body. Change it with
/// [`InternalClient::with_max_response_bytes`].
pub const DEFAULT_MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

/// The limit on an error response body: an M1 body is small, and anything else is only logged.
const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;

const APPLICATION_JSON: &str = "application/json";

/// Characters left as they are in a path parameter: RFC 3986 unreserved characters.
const PATH_PARAM: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// The HTTP method of an internal endpoint. Only reads cross services synchronously, so there
/// is no `PUT`, `PATCH` or `DELETE`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    /// A read addressed by its path alone; never sends a body.
    Get,
    /// A read that needs a JSON body, such as a batch lookup or a search; always sends one.
    Post,
}

/// One internal API operation: the serving service, the route, and the request and response
/// types. The serving service registers [`Endpoint::ROUTE`] with its router, so both sides use
/// the same string.
///
/// ```
/// use contract::client::{Endpoint, Method};
/// use contract::{TenantId, service};
/// # #[derive(serde::Deserialize)] pub struct TenantState {}
///
/// /// Reads a tenant's current state.
/// pub struct GetTenant {
///     pub tenant_id: TenantId,
/// }
///
/// impl Endpoint for GetTenant {
///     type Service = service::Identity;
///     type Body = ();
///     type Response = TenantState;
///     const METHOD: Method = Method::Get;
///     const ROUTE: &'static str = "/internal/tenants/{tenant_id}";
///
///     fn path_params(&self) -> Vec<String> {
///         vec![self.tenant_id.to_string()]
///     }
/// }
/// ```
pub trait Endpoint {
    /// The service that serves the endpoint.
    type Service: Service;
    /// The JSON request body; `()` for an endpoint that takes none.
    type Body: Serialize;
    /// The JSON response body; `()` for an endpoint that answers with no content.
    type Response: DeserializeOwned;
    /// The HTTP method.
    const METHOD: Method;
    /// The route template: [`ROUTE_PREFIX`], then `/`-separated segments of `a-z`, `0-9`, `-`
    /// and `_`, where a `{name}` segment is a path parameter. Check it with [`check_route`].
    const ROUTE: &'static str;

    /// The values of the route's `{name}` segments, in order. Each value is percent-encoded
    /// as one segment; empty, `.` and `..` values are rejected.
    fn path_params(&self) -> Vec<String> {
        Vec::new()
    }

    /// The request body: `None` for [`Method::Get`], `Some` for [`Method::Post`]. Any other
    /// combination fails the call with [`CallErrorKind::InvalidRequest`] before anything is
    /// sent, so a `Post` endpoint must override this.
    fn body(&self) -> Option<&Self::Body> {
        None
    }
}

/// A route template or path parameter that cannot form a request path.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RouteError {
    /// The route does not start with [`ROUTE_PREFIX`].
    #[error("route must start with `/internal/`")]
    Prefix,
    /// A segment is empty or has a character other than `a-z`, `0-9`, `-` and `_`.
    #[error("route segment {0} must be one or more of a-z, 0-9, `-` and `_`, or `{{name}}`")]
    Segment(usize),
    /// The number of path parameters differs from the number of `{name}` segments.
    #[error("route has {placeholders} path parameters but {params} values were given")]
    ParamCount {
        /// `{name}` segments in the route.
        placeholders: usize,
        /// Values given.
        params: usize,
    },
    /// A path parameter is empty, `.` or `..`, which would change the path's structure.
    #[error("path parameter {0} must not be empty, `.` or `..`")]
    Param(usize),
}

/// Checks a route template and returns its number of `{name}` path parameters. Every
/// endpoint's route should be checked by a test.
///
/// # Errors
///
/// When the route does not follow the rules of [`Endpoint::ROUTE`].
pub fn check_route(route: &str) -> Result<usize, RouteError> {
    let rest = route.strip_prefix(ROUTE_PREFIX).ok_or(RouteError::Prefix)?;
    let mut placeholders = 0;
    for (index, segment) in rest.split('/').enumerate() {
        let word = match placeholder(segment) {
            Some(name) => {
                placeholders += 1;
                name
            }
            None => segment,
        };
        if !is_route_word(word) {
            // Segment 0 is `internal`.
            return Err(RouteError::Segment(index + 1));
        }
    }
    Ok(placeholders)
}

fn placeholder(segment: &str) -> Option<&str> {
    segment.strip_prefix('{')?.strip_suffix('}')
}

fn is_route_word(word: &str) -> bool {
    !word.is_empty()
        && word
            .bytes()
            .all(|byte| matches!(byte, b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_'))
}

/// Fills the route's `{name}` segments with the percent-encoded parameters.
fn render_path(route: &str, params: &[String]) -> Result<String, RouteError> {
    let placeholders = check_route(route)?;
    if placeholders != params.len() {
        return Err(RouteError::ParamCount {
            placeholders,
            params: params.len(),
        });
    }
    let mut values = params.iter().enumerate();
    let mut path =
        String::with_capacity(route.len() + params.iter().map(String::len).sum::<usize>());
    // The route starts with `/`, so the first split item is empty.
    for segment in route.split('/').skip(1) {
        path.push('/');
        if placeholder(segment).is_none() {
            path.push_str(segment);
            continue;
        }
        let Some((index, value)) = values.next() else {
            return Err(RouteError::ParamCount {
                placeholders,
                params: params.len(),
            });
        };
        if value.is_empty() || value == "." || value == ".." {
            return Err(RouteError::Param(index));
        }
        path.extend(utf8_percent_encode(value, PATH_PARAM));
    }
    Ok(path)
}

/// Checks a base URL and returns it without the trailing `/`.
fn normalize_base_url(raw: &str) -> Result<String, &'static str> {
    let url = reqwest::Url::parse(raw).map_err(|_| "not an absolute URL")?;
    if url.scheme() != "http" {
        return Err("the scheme must be http: ztunnel adds mutual TLS between services");
    }
    if url.host_str().is_none_or(str::is_empty) {
        return Err("the host is missing");
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("credentials are not allowed");
    }
    if url.path() != "/" {
        return Err("a path is not allowed: endpoint routes start at /internal/");
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err("a query or fragment is not allowed");
    }
    Ok(url.as_str().trim_end_matches('/').to_owned())
}

/// A client for the internal APIs of service `S`. Cheap to clone: clones share one connection
/// pool. Build one per peer at startup and reuse it.
pub struct InternalClient<S> {
    http: reqwest::Client,
    base_url: String,
    timeout: Duration,
    max_response_bytes: usize,
    peer: PhantomData<fn() -> S>,
}

impl<S: Service> InternalClient<S> {
    /// A client for the service at `base_url`, `http://<host>[:<port>]` (the peer's Kubernetes
    /// Service address), where every call must finish within `timeout`.
    ///
    /// ```
    /// use std::time::Duration;
    /// use contract::{InternalClient, service};
    ///
    /// let identity = InternalClient::<service::Identity>::new(
    ///     "http://identity.trustiot.svc.cluster.local:8080",
    ///     Duration::from_secs(2),
    /// )?;
    /// # Ok::<(), contract::ClientConfigError>(())
    /// ```
    ///
    /// A client without a timeout does not compile:
    ///
    /// ```compile_fail,E0061
    /// # use contract::{InternalClient, service};
    /// let identity = InternalClient::<service::Identity>::new("http://identity:8080");
    /// ```
    ///
    /// # Errors
    ///
    /// When `timeout` is zero or above [`MAX_TIMEOUT`], when `base_url` is not a plain `http`
    /// origin, or when the HTTP client cannot be built.
    pub fn new(base_url: &str, timeout: Duration) -> Result<Self, ClientConfigError> {
        if timeout.is_zero() || timeout > MAX_TIMEOUT {
            return Err(ClientConfigError::Timeout {
                peer: S::NAME,
                timeout,
            });
        }
        let base_url =
            normalize_base_url(base_url).map_err(|reason| ClientConfigError::BaseUrl {
                peer: S::NAME,
                reason,
            })?;
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .http1_only()
            // Internal calls stay in the mesh: no proxy from the environment, no redirects.
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|source| ClientConfigError::Build {
                peer: S::NAME,
                source,
            })?;
        Ok(Self {
            http,
            base_url,
            timeout,
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
            peer: PhantomData,
        })
    }

    /// Replaces the limit on a successful response body ([`DEFAULT_MAX_RESPONSE_BYTES`]); a
    /// larger body fails the call with [`CallErrorKind::InvalidResponse`].
    #[must_use]
    pub fn with_max_response_bytes(mut self, limit: usize) -> Self {
        self.max_response_bytes = limit;
        self
    }

    /// The service this client calls.
    #[must_use]
    pub fn peer(&self) -> ServiceName {
        S::NAME
    }

    /// The deadline of every call.
    #[must_use]
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// Calls `endpoint` with `token` in [`INTERNAL_IDENTITY_HEADER`]. The token's audience must
    /// be this client's peer; how a caller obtains one is up to the identity design
    /// (Laysi/trustiot-v2#106).
    ///
    /// ```
    /// # use contract::client::{Endpoint, InternalClient, Method};
    /// # use contract::{InternalToken, service};
    /// struct ListDevices;
    ///
    /// impl Endpoint for ListDevices {
    ///     type Service = service::Assets;
    ///     type Body = ();
    ///     type Response = ();
    ///     const METHOD: Method = Method::Get;
    ///     const ROUTE: &'static str = "/internal/devices";
    /// }
    ///
    /// async fn list(assets: &InternalClient<service::Assets>, token: &InternalToken) {
    ///     let _ = assets.call(&ListDevices, token).await;
    /// }
    /// ```
    ///
    /// Calling it on another service's client does not compile:
    ///
    /// ```compile_fail,E0271
    /// # use contract::client::{Endpoint, InternalClient, Method};
    /// # use contract::{InternalToken, service};
    /// # struct ListDevices;
    /// # impl Endpoint for ListDevices {
    /// #     type Service = service::Assets;
    /// #     type Body = ();
    /// #     type Response = ();
    /// #     const METHOD: Method = Method::Get;
    /// #     const ROUTE: &'static str = "/internal/devices";
    /// # }
    /// async fn list(identity: &InternalClient<service::Identity>, token: &InternalToken) {
    ///     let _ = identity.call(&ListDevices, token).await;
    /// }
    /// ```
    ///
    /// # Errors
    ///
    /// A [`CallError`] for any failure; it is already logged with the peer service and the kind
    /// of failure, and [`CallError::to_error_body`] gives the M1 body to answer with.
    pub async fn call<E>(
        &self,
        endpoint: &E,
        token: &InternalToken,
    ) -> Result<E::Response, CallError>
    where
        E: Endpoint<Service = S>,
    {
        let result = self.send(endpoint, token).await;
        if let Err(error) = &result {
            error.log();
        }
        result
    }

    async fn send<E>(&self, endpoint: &E, token: &InternalToken) -> Result<E::Response, CallError>
    where
        E: Endpoint<Service = S>,
    {
        let failure = |kind| CallError::new(S::NAME, E::ROUTE, self.timeout, kind);

        let path = render_path(E::ROUTE, &endpoint.path_params())
            .map_err(|error| failure(CallErrorKind::InvalidRequest).with_source(error))?;
        let mut token_value = HeaderValue::from_str(token.expose())
            .map_err(|error| failure(CallErrorKind::InvalidRequest).with_source(error))?;
        token_value.set_sensitive(true);
        let url = format!("{}{path}", self.base_url);
        let mut request = match E::METHOD {
            Method::Get => self.http.get(url),
            Method::Post => self.http.post(url),
        }
        .header(INTERNAL_IDENTITY_HEADER, token_value)
        .header(ACCEPT, APPLICATION_JSON);
        match (E::METHOD, endpoint.body()) {
            (Method::Get, None) => {}
            (Method::Get, Some(_)) => {
                return Err(failure(CallErrorKind::InvalidRequest)
                    .with_reason("a GET endpoint sends no body"));
            }
            (Method::Post, None) => {
                return Err(failure(CallErrorKind::InvalidRequest)
                    .with_reason("a POST endpoint must send a JSON body"));
            }
            (Method::Post, Some(body)) => {
                let bytes = serde_json::to_vec(body).map_err(|error| {
                    failure(CallErrorKind::InvalidRequest).with_source(JsonError::from(error))
                })?;
                request = request.header(CONTENT_TYPE, APPLICATION_JSON).body(bytes);
            }
        }

        let mut response = request
            .send()
            .await
            .map_err(|error| failure(classify(&error)).with_source(error.without_url()))?;
        let status = response.status();

        if !status.is_success() {
            let error = failure(CallErrorKind::ErrorStatus).with_status(status);
            return Err(match read_body(&mut response, MAX_ERROR_BODY_BYTES).await {
                // A body that is not M1 (a proxy's error page, for example) is mapped by
                // status alone.
                Ok(body) => error.with_peer_error(parse_m1_body(&body)),
                Err(BodyError::TooLarge) => {
                    error.with_reason("the error body is too large to be an M1 body")
                }
                Err(BodyError::Transport(source)) => error.with_source(source.without_url()),
            });
        }

        let body = match read_body(&mut response, self.max_response_bytes).await {
            Ok(body) => body,
            Err(BodyError::TooLarge) => {
                return Err(failure(CallErrorKind::InvalidResponse)
                    .with_status(status)
                    .with_reason("the response body exceeds the client's limit"));
            }
            Err(BodyError::Transport(source)) => {
                return Err(failure(classify(&source))
                    .with_status(status)
                    .with_source(source.without_url()));
            }
        };
        // An empty body (204 No Content) decodes as JSON `null`, which is what `()` expects.
        let json: &[u8] = if body.is_empty() { b"null" } else { &body };
        serde_json::from_slice(json).map_err(|error| {
            failure(CallErrorKind::InvalidResponse)
                .with_status(status)
                .with_source(JsonError::from(error))
        })
    }
}

impl<S> Clone for InternalClient<S> {
    fn clone(&self) -> Self {
        Self {
            http: self.http.clone(),
            base_url: self.base_url.clone(),
            timeout: self.timeout,
            max_response_bytes: self.max_response_bytes,
            peer: PhantomData,
        }
    }
}

impl<S: Service> fmt::Debug for InternalClient<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InternalClient")
            .field("peer", &S::NAME)
            .field("base_url", &self.base_url)
            .field("timeout", &self.timeout)
            .field("max_response_bytes", &self.max_response_bytes)
            .finish_non_exhaustive()
    }
}

/// The peer's M1 error body, if the body is one. Only a JSON object counts: serde would also
/// accept a positional array for the struct.
fn parse_m1_body(body: &[u8]) -> Option<ErrorBody> {
    match serde_json::from_slice::<serde_json::Value>(body) {
        Ok(value) if value.is_object() => serde_json::from_value(value).ok(),
        _ => None,
    }
}

enum BodyError {
    TooLarge,
    Transport(reqwest::Error),
}

async fn read_body(response: &mut reqwest::Response, limit: usize) -> Result<Vec<u8>, BodyError> {
    let limit_u64 = u64::try_from(limit).unwrap_or(u64::MAX);
    if response
        .content_length()
        .is_some_and(|length| length > limit_u64)
    {
        return Err(BodyError::TooLarge);
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(BodyError::Transport)? {
        if chunk.len() > limit.saturating_sub(body.len()) {
            return Err(BodyError::TooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Sorts a transport failure into a [`CallErrorKind`].
fn classify(error: &reqwest::Error) -> CallErrorKind {
    if error.is_timeout() {
        return CallErrorKind::Timeout;
    }
    if error.is_connect() {
        return CallErrorKind::Connect;
    }
    let mut cause = error.source();
    while let Some(current) = cause {
        if let Some(io_error) = current.downcast_ref::<io::Error>()
            && matches!(
                io_error.kind(),
                io::ErrorKind::ConnectionReset
                    | io::ErrorKind::ConnectionAborted
                    | io::ErrorKind::BrokenPipe
                    | io::ErrorKind::UnexpectedEof
            )
        {
            return CallErrorKind::ConnectionReset;
        }
        if current
            .downcast_ref::<hyper::Error>()
            .is_some_and(hyper::Error::is_incomplete_message)
        {
            return CallErrorKind::ConnectionReset;
        }
        cause = current.source();
    }
    CallErrorKind::Transport
}

/// The kind of failure of an internal call.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CallErrorKind {
    /// No complete response within the client's timeout. M1: `timeout`.
    Timeout,
    /// No connection: the name does not resolve, or nothing listens. M1: `unavailable`.
    Connect,
    /// The connection was reset or closed before the response was complete. Besides a crashed
    /// or restarting peer, this is how ztunnel rejects a call that no AuthorizationPolicy
    /// allows. M1: `unavailable`.
    ConnectionReset,
    /// Another transport failure, such as a malformed HTTP response. M1: `unavailable`.
    Transport,
    /// The peer answered with a non-success status. M1: the peer's own code when it sent an M1
    /// body, except `unauthenticated`, which becomes `internal` (the caller's credential is
    /// wrong, not the user's session); otherwise derived from the status.
    ErrorStatus,
    /// The success response does not match the endpoint's response type, or is too large.
    /// M1: `internal`.
    InvalidResponse,
    /// The request could not be built: a bad route, path parameter, body or token. M1:
    /// `internal`.
    InvalidRequest,
}

impl CallErrorKind {
    /// The name used in logs, for example `connection_reset`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Timeout => "timeout",
            Self::Connect => "connect",
            Self::ConnectionReset => "connection_reset",
            Self::Transport => "transport",
            Self::ErrorStatus => "error_status",
            Self::InvalidResponse => "invalid_response",
            Self::InvalidRequest => "invalid_request",
        }
    }
}

impl fmt::Display for CallErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A failed internal call: which peer and route, what kind of failure, and the M1 code to
/// answer with.
#[derive(Debug)]
pub struct CallError {
    // Boxed so that `Result<_, CallError>` stays small on the success path.
    inner: Box<Inner>,
}

#[derive(Debug)]
struct Inner {
    peer: ServiceName,
    route: &'static str,
    timeout: Duration,
    kind: CallErrorKind,
    status: Option<StatusCode>,
    peer_error: Option<ErrorBody>,
    reason: Option<&'static str>,
    source: Option<Box<dyn StdError + Send + Sync>>,
}

impl CallError {
    fn new(peer: ServiceName, route: &'static str, timeout: Duration, kind: CallErrorKind) -> Self {
        Self {
            inner: Box::new(Inner {
                peer,
                route,
                timeout,
                kind,
                status: None,
                peer_error: None,
                reason: None,
                source: None,
            }),
        }
    }

    fn with_status(mut self, status: StatusCode) -> Self {
        self.inner.status = Some(status);
        self
    }

    fn with_peer_error(mut self, peer_error: Option<ErrorBody>) -> Self {
        self.inner.peer_error = peer_error;
        self
    }

    fn with_reason(mut self, reason: &'static str) -> Self {
        self.inner.reason = Some(reason);
        self
    }

    fn with_source(mut self, source: impl StdError + Send + Sync + 'static) -> Self {
        self.inner.source = Some(Box::new(source));
        self
    }

    /// The service that was called.
    #[must_use]
    pub fn peer(&self) -> ServiceName {
        self.inner.peer
    }

    /// The endpoint's route template; it never holds parameter values.
    #[must_use]
    pub fn route(&self) -> &'static str {
        self.inner.route
    }

    /// What kind of failure it was.
    #[must_use]
    pub fn kind(&self) -> CallErrorKind {
        self.inner.kind
    }

    /// The peer's HTTP status, when it answered.
    #[must_use]
    pub fn status(&self) -> Option<StatusCode> {
        self.inner.status
    }

    /// The peer's M1 error body, when it sent one with an error status.
    #[must_use]
    pub fn peer_error(&self) -> Option<&ErrorBody> {
        self.inner.peer_error.as_ref()
    }

    /// The M1 code for the caller to answer with; see [`CallErrorKind`] for the mapping.
    #[must_use]
    pub fn code(&self) -> ErrorCode {
        match self.inner.kind {
            CallErrorKind::Timeout => ErrorCode::Timeout,
            CallErrorKind::Connect | CallErrorKind::ConnectionReset | CallErrorKind::Transport => {
                ErrorCode::Unavailable
            }
            CallErrorKind::InvalidResponse | CallErrorKind::InvalidRequest => ErrorCode::Internal,
            CallErrorKind::ErrorStatus => match &self.inner.peer_error {
                Some(body) if body.code() == ErrorCode::Unauthenticated => ErrorCode::Internal,
                Some(body) => body.code(),
                None => match self.inner.status.map(|status| status.as_u16()) {
                    Some(408 | 504) => ErrorCode::Timeout,
                    Some(429 | 502 | 503) => ErrorCode::Unavailable,
                    _ => ErrorCode::Internal,
                },
            },
        }
    }

    /// The M1 body for the caller to answer with: the peer's own body when it is passed on
    /// unchanged, otherwise a body with [`CallError::code`] and its default message key.
    /// `internal` never carries the peer's details.
    #[must_use]
    pub fn to_error_body(&self) -> ErrorBody {
        let code = self.code();
        match &self.inner.peer_error {
            Some(body) if body.code() == code && code != ErrorCode::Internal => body.clone(),
            _ => ErrorBody::new(code),
        }
    }

    fn log(&self) {
        let inner = &self.inner;
        macro_rules! log_at {
            ($level:ident) => {
                tracing::$level!(
                    peer = inner.peer.as_str(),
                    route = inner.route,
                    kind = inner.kind.as_str(),
                    status = inner.status.map(|status| status.as_u16()),
                    peer_code = inner.peer_error.as_ref().map(|body| body.code().as_str()),
                    code = self.code().as_str(),
                    timeout_ms = u64::try_from(inner.timeout.as_millis()).unwrap_or(u64::MAX),
                    cause = self.cause_chain().as_deref(),
                    "{self}"
                )
            };
        }
        match inner.kind {
            CallErrorKind::Timeout
            | CallErrorKind::Connect
            | CallErrorKind::ConnectionReset
            | CallErrorKind::Transport => log_at!(warn),
            CallErrorKind::InvalidResponse | CallErrorKind::InvalidRequest => log_at!(error),
            // The peer's 4xx answers are ordinary outcomes the caller handles; failures of the
            // peer itself, or of this service's credential, are not.
            CallErrorKind::ErrorStatus => match self.code() {
                ErrorCode::Internal => log_at!(error),
                ErrorCode::Unavailable | ErrorCode::Timeout => log_at!(warn),
                _ => log_at!(debug),
            },
        }
    }

    /// The source errors, outermost first, joined by `: `.
    fn cause_chain(&self) -> Option<String> {
        let mut cause = self.source()?;
        let mut chain = cause.to_string();
        while let Some(next) = cause.source() {
            chain.push_str(": ");
            chain.push_str(&next.to_string());
            cause = next;
        }
        Some(chain)
    }
}

impl fmt::Display for CallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let inner = &self.inner;
        write!(
            f,
            "internal call to {} {} failed: ",
            inner.peer, inner.route
        )?;
        match inner.kind {
            CallErrorKind::Timeout => {
                write!(f, "no complete response within {:?}", inner.timeout)?;
            }
            CallErrorKind::Connect => f.write_str("could not connect")?,
            CallErrorKind::ConnectionReset => f.write_str(
                "connection reset or closed before the response was complete; ztunnel resets \
                 calls that no AuthorizationPolicy allows",
            )?,
            CallErrorKind::Transport => f.write_str("transport error")?,
            CallErrorKind::ErrorStatus => match (inner.status, &inner.peer_error) {
                (Some(status), Some(body)) => write!(f, "peer answered {status} with {body}")?,
                (Some(status), None) => write!(f, "peer answered {status} without an M1 body")?,
                (None, _) => f.write_str("peer answered with an error status")?,
            },
            CallErrorKind::InvalidResponse => {
                f.write_str("the response does not match the endpoint's contract")?;
            }
            CallErrorKind::InvalidRequest => f.write_str("the request could not be built")?,
        }
        if let Some(reason) = inner.reason {
            write!(f, " ({reason})")?;
        }
        Ok(())
    }
}

impl StdError for CallError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        self.inner
            .source
            .as_deref()
            .map(|source| source as &(dyn StdError + 'static))
    }
}

impl From<CallError> for ErrorBody {
    fn from(error: CallError) -> Self {
        error.to_error_body()
    }
}

/// An [`InternalClient`] could not be created.
#[derive(Debug, thiserror::Error)]
pub enum ClientConfigError {
    /// The timeout is zero or above [`MAX_TIMEOUT`].
    #[error("timeout for `{peer}` must be above zero and at most {max:?}, not {timeout:?}", max = MAX_TIMEOUT)]
    Timeout {
        /// The peer service.
        peer: ServiceName,
        /// The rejected timeout.
        timeout: Duration,
    },
    /// The base URL is not a plain `http` origin. The URL itself is left out of the message
    /// because it may hold credentials.
    #[error("base URL for `{peer}` is invalid: {reason}")]
    BaseUrl {
        /// The peer service.
        peer: ServiceName,
        /// What is wrong with it.
        reason: &'static str,
    },
    /// The HTTP client could not be built.
    #[error("HTTP client for `{peer}` cannot be built")]
    Build {
        /// The peer service.
        peer: ServiceName,
        /// The cause.
        source: reqwest::Error,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::MessageKey;
    use crate::service;

    fn params(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn routes_follow_the_rules() {
        assert_eq!(check_route("/internal/tenants"), Ok(0));
        assert_eq!(
            check_route("/internal/tenants/{tenant_id}/gateway-quota"),
            Ok(1)
        );
        assert_eq!(check_route("/internal/a/{x}/b/{y}"), Ok(2));
        assert_eq!(check_route("/api/tenants"), Err(RouteError::Prefix));
        assert_eq!(check_route("/internal"), Err(RouteError::Prefix));
        assert_eq!(check_route("/internal/"), Err(RouteError::Segment(1)));
        assert_eq!(
            check_route("/internal/tenants/"),
            Err(RouteError::Segment(2))
        );
        assert_eq!(
            check_route("/internal//tenants"),
            Err(RouteError::Segment(1))
        );
        assert_eq!(
            check_route("/internal/Tenants"),
            Err(RouteError::Segment(1))
        );
        assert_eq!(
            check_route("/internal/tenants/{}"),
            Err(RouteError::Segment(2))
        );
        assert_eq!(
            check_route("/internal/tenants/{id"),
            Err(RouteError::Segment(2))
        );
        assert_eq!(
            check_route("/internal/../admin"),
            Err(RouteError::Segment(1))
        );
        assert_eq!(
            check_route("/internal/tenants?all"),
            Err(RouteError::Segment(1))
        );
    }

    #[test]
    fn path_parameters_are_encoded_as_single_segments() {
        let route = "/internal/gateways/{hardware_id}/config";
        assert_eq!(
            render_path(route, &params(&["gw-01.a_b~c"])),
            Ok("/internal/gateways/gw-01.a_b~c/config".to_owned())
        );
        assert_eq!(
            render_path(route, &params(&["a/../b?c#d %2e"])),
            Ok("/internal/gateways/a%2F..%2Fb%3Fc%23d%20%252e/config".to_owned())
        );
        assert_eq!(
            render_path(route, &params(&["閘道器"])),
            Ok("/internal/gateways/%E9%96%98%E9%81%93%E5%99%A8/config".to_owned())
        );
        for dot in ["", ".", ".."] {
            assert_eq!(
                render_path(route, &params(&[dot])),
                Err(RouteError::Param(0)),
                "{dot:?}"
            );
        }
        assert_eq!(
            render_path(route, &[]),
            Err(RouteError::ParamCount {
                placeholders: 1,
                params: 0
            })
        );
        assert_eq!(
            render_path("/internal/tenants", &params(&["x"])),
            Err(RouteError::ParamCount {
                placeholders: 0,
                params: 1
            })
        );
    }

    #[test]
    fn base_urls_must_be_plain_http_origins() {
        assert_eq!(
            normalize_base_url("http://identity.trustiot.svc.cluster.local:8080"),
            Ok("http://identity.trustiot.svc.cluster.local:8080".to_owned())
        );
        assert_eq!(
            normalize_base_url("http://identity/"),
            Ok("http://identity".to_owned())
        );
        assert_eq!(
            normalize_base_url("http://[::1]:9000"),
            Ok("http://[::1]:9000".to_owned())
        );
        for bad in [
            "identity:8080",
            "https://identity",
            "http://user:secret@identity",
            "http://identity/api",
            "http://identity?x=1",
            "http://identity#top",
            "",
        ] {
            assert!(normalize_base_url(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn timeouts_are_mandatory_and_bounded() {
        let new = |timeout| InternalClient::<service::Assets>::new("http://assets:8080", timeout);
        assert!(matches!(
            new(Duration::ZERO),
            Err(ClientConfigError::Timeout {
                peer: ServiceName::Assets,
                ..
            })
        ));
        assert!(matches!(
            new(MAX_TIMEOUT + Duration::from_millis(1)),
            Err(ClientConfigError::Timeout { .. })
        ));
        let client = new(Duration::from_millis(1500)).expect("valid client");
        assert_eq!(client.timeout(), Duration::from_millis(1500));
        assert_eq!(client.peer(), ServiceName::Assets);
        assert!(new(MAX_TIMEOUT).is_ok());
        let error =
            InternalClient::<service::Assets>::new("https://u:p@assets", Duration::from_secs(1))
                .expect_err("https is rejected");
        assert!(!error.to_string().contains("u:p"), "{error}");
    }

    fn status_error(status: u16, peer_error: Option<ErrorBody>) -> CallError {
        CallError::new(
            ServiceName::Identity,
            "/internal/tenants/{tenant_id}",
            Duration::from_secs(2),
            CallErrorKind::ErrorStatus,
        )
        .with_status(StatusCode::from_u16(status).expect("status"))
        .with_peer_error(peer_error)
    }

    #[test]
    fn transport_failures_map_to_m1_codes() {
        let expected = [
            (CallErrorKind::Timeout, ErrorCode::Timeout),
            (CallErrorKind::Connect, ErrorCode::Unavailable),
            (CallErrorKind::ConnectionReset, ErrorCode::Unavailable),
            (CallErrorKind::Transport, ErrorCode::Unavailable),
            (CallErrorKind::InvalidResponse, ErrorCode::Internal),
            (CallErrorKind::InvalidRequest, ErrorCode::Internal),
        ];
        for (kind, code) in expected {
            let error = CallError::new(
                ServiceName::Assets,
                "/internal/x",
                Duration::from_secs(1),
                kind,
            );
            assert_eq!(error.code(), code, "{kind}");
            assert_eq!(error.to_error_body(), ErrorBody::new(code), "{kind}");
        }
    }

    #[test]
    fn peer_m1_errors_pass_through_except_unauthenticated() {
        let not_found = ErrorBody::new(ErrorCode::NotFound)
            .with_message_key(MessageKey::new("tenant.not_found"))
            .with_detail("tenant", "t-1");
        let error = status_error(404, Some(not_found.clone()));
        assert_eq!(error.code(), ErrorCode::NotFound);
        assert_eq!(error.to_error_body(), not_found);
        assert_eq!(error.peer_error(), Some(&not_found));

        let unauthenticated = status_error(401, Some(ErrorBody::new(ErrorCode::Unauthenticated)));
        assert_eq!(unauthenticated.code(), ErrorCode::Internal);
        assert_eq!(
            unauthenticated.to_error_body(),
            ErrorBody::new(ErrorCode::Internal)
        );
    }

    #[test]
    fn only_json_objects_count_as_m1_bodies() {
        let body = ErrorBody::new(ErrorCode::NotFound);
        let object = serde_json::to_vec(&body).expect("serialize");
        assert_eq!(parse_m1_body(&object), Some(body));
        for not_m1 in [
            br#"["not_found","error.not_found"]"#.as_slice(),
            b"no healthy upstream",
            b"",
            br#"{"code":"teapot","message_key":"error.teapot"}"#,
        ] {
            assert_eq!(
                parse_m1_body(not_m1),
                None,
                "{}",
                String::from_utf8_lossy(not_m1)
            );
        }
    }

    #[test]
    fn statuses_without_an_m1_body_map_by_status() {
        for (status, code) in [
            (408, ErrorCode::Timeout),
            (504, ErrorCode::Timeout),
            (429, ErrorCode::Unavailable),
            (502, ErrorCode::Unavailable),
            (503, ErrorCode::Unavailable),
            (500, ErrorCode::Internal),
            (404, ErrorCode::Internal),
            (403, ErrorCode::Internal),
            (302, ErrorCode::Internal),
        ] {
            assert_eq!(status_error(status, None).code(), code, "{status}");
        }
    }

    #[test]
    fn display_names_peer_route_and_kind_without_values() {
        let error = CallError::new(
            ServiceName::Identity,
            "/internal/tenants/{tenant_id}",
            Duration::from_millis(250),
            CallErrorKind::Timeout,
        );
        assert_eq!(
            error.to_string(),
            "internal call to identity /internal/tenants/{tenant_id} failed: no complete \
             response within 250ms"
        );
        let reset = CallError::new(
            ServiceName::Assets,
            "/internal/x",
            Duration::from_secs(1),
            CallErrorKind::ConnectionReset,
        );
        assert!(reset.to_string().contains("AuthorizationPolicy"), "{reset}");
    }
}
