//! The typed internal client against real sockets: what goes on the wire, and how timeouts,
//! resets, refused connections and peer errors map to M1 codes.

mod common;

use std::time::{Duration, Instant};

use common::peer::{Reply, http_response, serve_once};
use contract::client::{CallErrorKind, Endpoint, InternalClient, Method};
use contract::{ErrorBody, ErrorCode, InternalToken, MessageKey, TenantId, service};
use http::StatusCode;
use serde::{Deserialize, Serialize};

// Shaped unlike a JWT, so secret scanners do not mistake the fixture for a credential.
const TOKEN: &str = "test-internal-token.not-a-jwt";

#[derive(Debug, PartialEq, Deserialize)]
struct Tenant {
    id: TenantId,
    name: String,
}

/// `GET` with a string path parameter, to see the encoding on the wire.
struct GetTenant {
    key: String,
}

impl Endpoint for GetTenant {
    type Service = service::Identity;
    type Body = ();
    type Response = Tenant;
    const METHOD: Method = Method::Get;
    const ROUTE: &'static str = "/internal/tenants/{tenant_key}";

    fn path_params(&self) -> Vec<String> {
        vec![self.key.clone()]
    }
}

#[derive(Serialize)]
struct LookupRequest {
    ids: Vec<TenantId>,
}

/// `POST` with a JSON body.
struct LookupTenants(LookupRequest);

impl Endpoint for LookupTenants {
    type Service = service::Identity;
    type Body = LookupRequest;
    type Response = Vec<Tenant>;
    const METHOD: Method = Method::Post;
    const ROUTE: &'static str = "/internal/tenants/lookup";

    fn body(&self) -> Option<&LookupRequest> {
        Some(&self.0)
    }
}

/// Answers with no content.
struct Ping;

impl Endpoint for Ping {
    type Service = service::Identity;
    type Body = ();
    type Response = ();
    const METHOD: Method = Method::Get;
    const ROUTE: &'static str = "/internal/ping";
}

/// A `GET` endpoint that wrongly returns a body.
struct GetWithBody;

impl Endpoint for GetWithBody {
    type Service = service::Identity;
    type Body = ();
    type Response = ();
    const METHOD: Method = Method::Get;
    const ROUTE: &'static str = "/internal/ping";

    fn body(&self) -> Option<&()> {
        Some(&())
    }
}

/// A `POST` endpoint that wrongly keeps the default `body()`, so it has no body.
struct PostWithoutBody;

impl Endpoint for PostWithoutBody {
    type Service = service::Identity;
    type Body = LookupRequest;
    type Response = Vec<Tenant>;
    const METHOD: Method = Method::Post;
    const ROUTE: &'static str = "/internal/tenants/lookup";
}

fn token() -> InternalToken {
    InternalToken::new(TOKEN).expect("valid token")
}

fn client(base_url: &str, timeout: Duration) -> InternalClient<service::Identity> {
    InternalClient::new(base_url, timeout).expect("valid client")
}

fn tenant_json() -> String {
    r#"{"id":"01928f6e-7a3c-7d4e-8f00-00000000000a","name":"Acme"}"#.to_owned()
}

#[tokio::test]
async fn get_sends_the_token_and_decodes_the_response() {
    let (base_url, request) = serve_once(Reply::Respond(http_response(
        200,
        "application/json",
        &tenant_json(),
    )))
    .await;
    let tenant = client(&base_url, Duration::from_secs(5))
        .call(
            &GetTenant {
                key: "acme/east 1".to_owned(),
            },
            &token(),
        )
        .await
        .expect("call succeeds");
    assert_eq!(tenant.name, "Acme");

    let request = request.await.expect("server task");
    assert!(
        request.starts_with("GET /internal/tenants/acme%2Feast%201 HTTP/1.1\r\n"),
        "{request}"
    );
    let lower = request.to_ascii_lowercase();
    assert!(lower.contains(&format!(
        "x-internal-identity: {}",
        TOKEN.to_ascii_lowercase()
    )));
    assert!(lower.contains("accept: application/json"));
    assert!(
        !lower.contains("content-type"),
        "GET sends no body: {request}"
    );
}

#[tokio::test]
async fn post_sends_a_json_body() {
    let (base_url, request) = serve_once(Reply::Respond(http_response(
        200,
        "application/json",
        &format!("[{}]", tenant_json()),
    )))
    .await;
    let id: TenantId = "01928f6e-7a3c-7d4e-8f00-00000000000a".parse().expect("id");
    let tenants = client(&base_url, Duration::from_secs(5))
        .call(&LookupTenants(LookupRequest { ids: vec![id] }), &token())
        .await
        .expect("call succeeds");
    assert_eq!(tenants.len(), 1);
    assert_eq!(tenants[0].id, id);

    let request = request.await.expect("server task");
    assert!(
        request.starts_with("POST /internal/tenants/lookup HTTP/1.1\r\n"),
        "{request}"
    );
    assert!(
        request
            .to_ascii_lowercase()
            .contains("content-type: application/json")
    );
    assert!(
        request.ends_with(r#"{"ids":["01928f6e-7a3c-7d4e-8f00-00000000000a"]}"#),
        "{request}"
    );
}

#[tokio::test]
async fn no_content_decodes_as_unit() {
    let (base_url, _request) = serve_once(Reply::Respond(
        "HTTP/1.1 204 No Content\r\nconnection: close\r\n\r\n".to_owned(),
    ))
    .await;
    client(&base_url, Duration::from_secs(5))
        .call(&Ping, &token())
        .await
        .expect("204 decodes as ()");
}

#[tokio::test]
async fn a_silent_peer_times_out_as_m1_timeout() {
    let (base_url, _request) = serve_once(Reply::Silent).await;
    let timeout = Duration::from_millis(300);
    let started = Instant::now();
    let error = client(&base_url, timeout)
        .call(&Ping, &token())
        .await
        .expect_err("the peer never answers");
    let elapsed = started.elapsed();

    assert_eq!(error.kind(), CallErrorKind::Timeout);
    assert_eq!(error.code(), ErrorCode::Timeout);
    assert_eq!(error.to_error_body(), ErrorBody::new(ErrorCode::Timeout));
    assert_eq!(error.code().http_status(), StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(error.peer(), contract::ServiceName::Identity);
    assert!(
        elapsed >= timeout,
        "returned before the timeout: {elapsed:?}"
    );
    assert!(
        elapsed < timeout + Duration::from_secs(2),
        "took {elapsed:?}"
    );
}

#[tokio::test]
async fn a_reset_connection_maps_to_unavailable() {
    // ztunnel rejects a call that no AuthorizationPolicy allows this way.
    let (base_url, _request) = serve_once(Reply::Reset).await;
    let error = client(&base_url, Duration::from_secs(5))
        .call(
            &GetTenant {
                key: "acme".to_owned(),
            },
            &token(),
        )
        .await
        .expect_err("the peer resets the connection");

    assert_eq!(error.kind(), CallErrorKind::ConnectionReset, "{error:?}");
    assert_eq!(error.code(), ErrorCode::Unavailable);
    assert_eq!(
        error.to_error_body(),
        ErrorBody::new(ErrorCode::Unavailable)
    );
    assert_eq!(error.route(), "/internal/tenants/{tenant_key}");
    assert_eq!(error.status(), None);
}

#[tokio::test]
async fn a_connection_closed_without_a_response_is_a_reset() {
    let (base_url, _request) = serve_once(Reply::Close).await;
    let error = client(&base_url, Duration::from_secs(5))
        .call(&Ping, &token())
        .await
        .expect_err("the peer closes without answering");
    assert_eq!(error.kind(), CallErrorKind::ConnectionReset, "{error:?}");
    assert_eq!(error.code(), ErrorCode::Unavailable);
}

#[tokio::test]
async fn a_refused_connection_maps_to_unavailable() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let base_url = format!("http://{}", listener.local_addr().expect("address"));
    drop(listener);
    let error = client(&base_url, Duration::from_secs(5))
        .call(&Ping, &token())
        .await
        .expect_err("nothing listens");
    assert_eq!(error.kind(), CallErrorKind::Connect, "{error:?}");
    assert_eq!(error.code(), ErrorCode::Unavailable);
}

#[tokio::test]
async fn peer_m1_errors_pass_through() {
    let body = ErrorBody::new(ErrorCode::NotFound)
        .with_message_key(MessageKey::new("tenant.not_found"))
        .with_detail("tenant_key", "acme");
    let json = serde_json::to_string(&body).expect("serialize");
    let (base_url, _request) = serve_once(Reply::Respond(http_response(
        404,
        "application/json",
        &json,
    )))
    .await;
    let error = client(&base_url, Duration::from_secs(5))
        .call(
            &GetTenant {
                key: "acme".to_owned(),
            },
            &token(),
        )
        .await
        .expect_err("404");

    assert_eq!(error.kind(), CallErrorKind::ErrorStatus);
    assert_eq!(error.status(), Some(StatusCode::NOT_FOUND));
    assert_eq!(error.peer_error(), Some(&body));
    assert_eq!(error.code(), ErrorCode::NotFound);
    assert_eq!(ErrorBody::from(error), body);
}

#[tokio::test]
async fn a_peer_rejecting_our_credential_is_internal() {
    let json = serde_json::to_string(&ErrorBody::new(ErrorCode::Unauthenticated)).expect("json");
    let (base_url, _request) = serve_once(Reply::Respond(http_response(
        401,
        "application/json",
        &json,
    )))
    .await;
    let error = client(&base_url, Duration::from_secs(5))
        .call(&Ping, &token())
        .await
        .expect_err("401");
    // The user's session is fine; re-login would not help.
    assert_eq!(error.code(), ErrorCode::Internal);
    assert_eq!(error.to_error_body(), ErrorBody::new(ErrorCode::Internal));
}

#[tokio::test]
async fn statuses_without_an_m1_body_map_by_status() {
    let cases = [
        (
            503,
            "text/plain",
            "no healthy upstream",
            ErrorCode::Unavailable,
        ),
        (
            502,
            "text/plain",
            "upstream connect error",
            ErrorCode::Unavailable,
        ),
        (
            504,
            "text/plain",
            "upstream request timeout",
            ErrorCode::Timeout,
        ),
        (500, "text/html", "<h1>oops</h1>", ErrorCode::Internal),
        (
            403,
            "text/plain",
            "RBAC: access denied",
            ErrorCode::Internal,
        ),
    ];
    for (status, content_type, body, code) in cases {
        let (base_url, _request) =
            serve_once(Reply::Respond(http_response(status, content_type, body))).await;
        let error = client(&base_url, Duration::from_secs(5))
            .call(&Ping, &token())
            .await
            .expect_err("error status");
        assert_eq!(error.kind(), CallErrorKind::ErrorStatus, "{status}");
        assert_eq!(error.peer_error(), None, "{status}");
        assert_eq!(error.code(), code, "{status}");
    }
}

#[tokio::test]
async fn redirects_are_not_followed() {
    let (base_url, _request) = serve_once(Reply::Respond(
        "HTTP/1.1 302 Found\r\nlocation: http://example.com/\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
            .to_owned(),
    ))
    .await;
    let error = client(&base_url, Duration::from_secs(5))
        .call(&Ping, &token())
        .await
        .expect_err("302 is an error");
    assert_eq!(error.status(), Some(StatusCode::FOUND));
    assert_eq!(error.code(), ErrorCode::Internal);
}

#[tokio::test]
async fn a_response_that_breaks_the_contract_is_internal() {
    let (base_url, _request) = serve_once(Reply::Respond(http_response(
        200,
        "application/json",
        r#"{"id":"not-a-uuid","name":"Acme"}"#,
    )))
    .await;
    let error = client(&base_url, Duration::from_secs(5))
        .call(
            &GetTenant {
                key: "acme".to_owned(),
            },
            &token(),
        )
        .await
        .expect_err("invalid response");
    assert_eq!(error.kind(), CallErrorKind::InvalidResponse);
    assert_eq!(error.code(), ErrorCode::Internal);
    // The message describes the failure without quoting the peer's data.
    assert!(!format!("{error} {:?}", std::error::Error::source(&error)).contains("not-a-uuid"));
}

#[tokio::test]
async fn an_oversized_response_is_rejected() {
    let (base_url, _request) = serve_once(Reply::Respond(http_response(
        200,
        "application/json",
        &tenant_json(),
    )))
    .await;
    let error = client(&base_url, Duration::from_secs(5))
        .with_max_response_bytes(16)
        .call(
            &GetTenant {
                key: "acme".to_owned(),
            },
            &token(),
        )
        .await
        .expect_err("too large");
    assert_eq!(error.kind(), CallErrorKind::InvalidResponse);
    assert_eq!(error.code(), ErrorCode::Internal);
}

#[tokio::test]
async fn bad_requests_fail_before_reaching_the_network() {
    // Nothing listens here; an InvalidRequest proves no connection was attempted.
    let unreachable = client("http://127.0.0.1:9", Duration::from_secs(5));
    let error = unreachable
        .call(&GetWithBody, &token())
        .await
        .expect_err("GET with a body");
    assert_eq!(error.kind(), CallErrorKind::InvalidRequest);
    assert_eq!(error.code(), ErrorCode::Internal);

    let error = unreachable
        .call(&PostWithoutBody, &token())
        .await
        .expect_err("POST without a body");
    assert_eq!(error.kind(), CallErrorKind::InvalidRequest);
    assert_eq!(error.code(), ErrorCode::Internal);

    for key in ["", ".", ".."] {
        let error = unreachable
            .call(
                &GetTenant {
                    key: key.to_owned(),
                },
                &token(),
            )
            .await
            .expect_err("dot segment");
        assert_eq!(error.kind(), CallErrorKind::InvalidRequest, "{key:?}");
    }
}
