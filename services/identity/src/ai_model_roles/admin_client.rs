//! The AI gateway admin API's client (Laysi/trustiot-v2#608; services/identity/README.md,
//! "AI 模型角色"): the only HTTP client of the service, apart from everything the ext_authz check
//! needs, so a slow or stuck gateway can never take what the cookie check of every request
//! waits for.
//!
//! - Its own hyper client, never shared: no connection is kept between calls
//!   (`pool_max_idle_per_host(0)`), so a call never lands on a connection the gateway closed
//!   meanwhile and nothing ever needs to be sent twice. Requests a connection lost before they
//!   were written are not retried either (`retry_canceled_requests(false)`): a change reaches the
//!   gateway at most once.
//! - At most [`MAX_IN_FLIGHT`] calls at once; one more is refused at once, without being sent.
//! - [`CONNECT_TIMEOUT`] to connect, and [`CALL_TIMEOUT`] for the whole call, the answer's body
//!   included, which is read up to [`MAX_ANSWER_BYTES`].
//! - Plain `http://` only: the address is a Service inside the cluster, and ztunnel's mTLS
//!   protects the connection (deploy/chart/README.md, "AI 閘道的管理埠"). No TLS, no proxy, no
//!   redirects (a 3xx is an answer like any other non-2xx).
//! - A call carries only what [`Call`] names: the method, the path, the query, the actor and a
//!   JSON body the service built itself. No header of the user's request is ever forwarded, so no
//!   cookie and no token reaches the gateway.

use std::sync::Arc;
use std::time::Duration;

use ai_gateway_api as api;
use axum::body::Bytes;
use axum::http::header::{ACCEPT, CONTENT_TYPE, HeaderValue};
use axum::http::{Method, Request, StatusCode, Uri};
use http_body_util::{BodyExt as _, Full, Limited};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use tokio::sync::Semaphore;

/// How long a call may take to connect.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

/// How long a whole call may take, connecting and reading the answer included. Above the
/// gateway's own database limit for a change (`database.timeout`, default 5 s), and well below
/// the service's handler timeout (`http.request_timeout`, default 30 s), so the route answers
/// for itself.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(10);

/// Calls in flight at most. Platform administrators are few and change roles rarely; a stuck
/// gateway holds at most this many of the service's tasks.
pub const MAX_IN_FLIGHT: usize = 4;

/// The largest answer read: a few roles and providers fit many times over.
pub const MAX_ANSWER_BYTES: usize = 256 << 10;

/// One call to the admin API.
#[derive(Debug)]
pub(crate) struct Call<'a> {
    pub method: Method,
    /// A path of the admin API ([`api::ROLES_PATH`], or [`api::role_path`] of a checked role).
    pub path: &'a str,
    /// The `DELETE`'s [`api::EXPECTED_VERSION_PARAM`].
    pub expected_version: Option<i64>,
    /// [`api::ACTOR_HEADER`]: `user:<user ID>` of the platform administrator.
    pub actor: &'a str,
    /// A JSON body, built by the service (never the user's bytes).
    pub json: Option<Vec<u8>>,
}

/// What became of a call.
#[derive(Debug)]
pub(crate) enum Exchange {
    /// The gateway answered with `status` and this whole body.
    Answered { status: StatusCode, body: Bytes },
    /// Nothing was sent, for the reason given: [`MAX_IN_FLIGHT`] calls were in flight, no
    /// connection could be opened in [`CONNECT_TIMEOUT`], or the request could not be built.
    NotSent(&'static str),
    /// The request may have reached the gateway, but no whole answer arrived: the call timed
    /// out, the connection broke, or the answer was too large. A change may have taken effect.
    Lost(&'static str),
}

/// The client; see the module's documentation.
#[derive(Clone)]
pub struct AdminClient {
    /// `http://<authority>`, without a trailing slash.
    base: String,
    http: Client<HttpConnector, Full<Bytes>>,
    permits: Arc<Semaphore>,
}

impl std::fmt::Debug for AdminClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdminClient")
            .field("base", &self.base)
            .finish_non_exhaustive()
    }
}

impl AdminClient {
    /// A client for the admin API at `url`: `http://<host>[:<port>]`, optionally with a trailing
    /// `/`.
    ///
    /// # Errors
    ///
    /// The rule `url` breaks, without its value: another scheme, user information, a path, a
    /// query or a fragment, or not a URL at all.
    pub fn new(url: &str) -> Result<Self, &'static str> {
        Ok(Self::with_limits(&base_url(url)?, MAX_IN_FLIGHT))
    }

    fn with_limits(base: &str, in_flight: usize) -> Self {
        let mut connector = HttpConnector::new();
        connector.enforce_http(true);
        connector.set_connect_timeout(Some(CONNECT_TIMEOUT));
        connector.set_nodelay(true);
        let http = Client::builder(TokioExecutor::new())
            .pool_max_idle_per_host(0)
            .retry_canceled_requests(false)
            .build(connector);
        Self {
            base: base.to_owned(),
            http,
            permits: Arc::new(Semaphore::new(in_flight)),
        }
    }

    /// Makes `call`, within [`CALL_TIMEOUT`].
    pub(crate) async fn call(&self, call: Call<'_>) -> Exchange {
        let Ok(_permit) = Arc::clone(&self.permits).try_acquire_owned() else {
            return Exchange::NotSent("too many calls in flight");
        };
        // The parts are checked or built by the service; a refusal here means a bug.
        let Some(request) = self.request(call) else {
            return Exchange::NotSent("the request could not be built");
        };
        let exchange = async {
            let response = match self.http.request(request).await {
                Ok(response) => response,
                Err(error) if error.is_connect() => return Exchange::NotSent("no connection"),
                Err(_) => return Exchange::Lost("the connection broke before the answer"),
            };
            let status = response.status();
            match Limited::new(response.into_body(), MAX_ANSWER_BYTES)
                .collect()
                .await
            {
                Ok(body) => Exchange::Answered {
                    status,
                    body: body.to_bytes(),
                },
                Err(_) => Exchange::Lost("the answer broke off or was too large"),
            }
        };
        tokio::time::timeout(CALL_TIMEOUT, exchange)
            .await
            .unwrap_or(Exchange::Lost("the call timed out"))
    }

    fn request(&self, call: Call<'_>) -> Option<Request<Full<Bytes>>> {
        if !call.path.starts_with("/internal/admin/") || !api::is_valid_actor(call.actor) {
            return None;
        }
        let uri = match call.expected_version {
            Some(version) => format!(
                "{}{}?{}={version}",
                self.base,
                call.path,
                api::EXPECTED_VERSION_PARAM
            ),
            None => format!("{}{}", self.base, call.path),
        };
        let uri: Uri = uri.parse().ok()?;
        let actor = HeaderValue::from_str(call.actor).ok()?;
        let mut request = Request::builder()
            .method(call.method)
            .uri(uri)
            .header(ACCEPT, "application/json")
            .header(api::ACTOR_HEADER, actor);
        let body = match call.json {
            Some(json) => {
                request = request.header(CONTENT_TYPE, "application/json");
                Bytes::from(json)
            }
            None => Bytes::new(),
        };
        request.body(Full::new(body)).ok()
    }
}

/// `http://<authority>` of `url`; see [`AdminClient::new`].
fn base_url(url: &str) -> Result<String, &'static str> {
    const RULE: &str = "must be http://<host>[:<port>], without user information, path, query or \
                        fragment (TLS comes from the mesh)";
    // `Uri` drops a fragment without a word, so it is refused here.
    if url.contains('#') {
        return Err(RULE);
    }
    let uri: Uri = url.trim().parse().map_err(|_| RULE)?;
    if uri.scheme_str() != Some("http") {
        return Err(RULE);
    }
    let Some(authority) = uri.authority() else {
        return Err(RULE);
    };
    let authority_text = authority.as_str();
    if authority_text.contains('@') || authority.host().is_empty() {
        return Err(RULE);
    }
    // `Uri` keeps a port that is not a number as text: after an IPv6 literal's `]`, whatever
    // follows the last `:` must be a port.
    let host_end = authority_text.rfind(']').map_or(0, |end| end + 1);
    if let Some((_, port)) = authority_text
        .get(host_end..)
        .unwrap_or_default()
        .rsplit_once(':')
        && port.parse::<u16>().is_err()
    {
        return Err(RULE);
    }
    // `Uri` keeps a query in `path_and_query`.
    let path_and_query = uri.path_and_query().map_or("", |path| path.as_str());
    if !matches!(path_and_query, "" | "/") {
        return Err(RULE);
    }
    Ok(format!("http://{authority}"))
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use axum::Router;
    use axum::routing::get;
    use tokio::net::TcpListener;

    use super::*;

    #[test]
    fn only_a_plain_http_origin_is_an_admin_url() {
        for (url, base) in [
            (
                "http://trustiot-ai-gateway-admin.trustiot.svc:8082",
                "http://trustiot-ai-gateway-admin.trustiot.svc:8082",
            ),
            ("http://127.0.0.1:8082/", "http://127.0.0.1:8082"),
            ("http://gateway", "http://gateway"),
            ("  http://[::1]:8082  ", "http://[::1]:8082"),
        ] {
            assert_eq!(base_url(url).as_deref(), Ok(base), "{url}");
        }
        for url in [
            "",
            "gateway:8082",
            "https://gateway:8082",
            "ftp://gateway",
            "http://user:pass@gateway:8082",
            "http://gateway:8082/internal",
            "http://gateway:8082/?a=1",
            "http://gateway:8082#x",
            "http://:8082",
            "http://gateway:port",
            "http://gateway:",
            "http://gateway:99999",
            "http://[::1]:x",
        ] {
            let refused = base_url(url).expect_err(url);
            assert!(!refused.contains("gateway"), "the error quotes nothing");
        }
    }

    async fn serve(app: Router) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("address");
        tokio::spawn(async move { axum::serve(listener, app).await });
        address
    }

    fn list(actor: &str) -> Call<'_> {
        Call {
            method: Method::GET,
            path: api::ROLES_PATH,
            expected_version: None,
            actor,
            json: None,
        }
    }

    #[tokio::test]
    async fn a_refused_connection_or_a_full_client_sends_nothing() {
        // A port that was free a moment ago: nothing listens there.
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("address");
        drop(listener);
        let client = AdminClient::new(&format!("http://{address}")).expect("client");
        assert!(matches!(
            client.call(list("user:1")).await,
            Exchange::NotSent("no connection")
        ));

        let client = AdminClient::with_limits(&format!("http://{address}"), 0);
        assert!(matches!(
            client.call(list("user:1")).await,
            Exchange::NotSent("too many calls in flight")
        ));
    }

    #[tokio::test]
    async fn an_invalid_actor_or_path_is_never_sent() {
        let address = serve(Router::new().route(api::ROLES_PATH, get(|| async { "{}" }))).await;
        let client = AdminClient::new(&format!("http://{address}")).expect("client");
        assert!(matches!(
            client.call(list("user:1\nx")).await,
            Exchange::NotSent(_)
        ));
        let mut call = list("user:1");
        call.path = "/api/other";
        assert!(matches!(client.call(call).await, Exchange::NotSent(_)));
        assert!(matches!(
            client.call(list("user:1")).await,
            Exchange::Answered { status, .. } if status == StatusCode::OK
        ));
    }

    #[tokio::test]
    async fn an_answer_over_the_limit_is_lost() {
        let address = serve(Router::new().route(
            api::ROLES_PATH,
            get(|| async { "x".repeat(MAX_ANSWER_BYTES + 1) }),
        ))
        .await;
        let client = AdminClient::new(&format!("http://{address}")).expect("client");
        assert!(matches!(
            client.call(list("user:1")).await,
            Exchange::Lost(_)
        ));
    }
}
