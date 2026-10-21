//! The Anthropic client against a loopback HTTP server: request headers and body, error
//! classification of recorded error responses, the concurrency limit, cancellation that closes
//! the connection, every time limit, and no redirects.

mod support;

use std::net::SocketAddr;
use std::time::Duration;

use ai_clients::anthropic::{
    AnthropicClient, InputMessage, Message, MessagesRequest, RequestOptions,
};
use ai_clients::{EndpointConfig, Error, ErrorClass, TimeoutKind};
use support::{Server, Step, endpoint, fixture, fixture_json, http_response, sse_head, sse_prefix};

fn client(config: EndpointConfig) -> AnthropicClient {
    AnthropicClient::new(config, support::test_key(), reqwest::Client::builder())
        .expect("a valid endpoint")
}

fn request() -> MessagesRequest {
    MessagesRequest {
        model: "claude-opus-5-5".into(),
        max_tokens: 1024,
        messages: vec![InputMessage::user_text("Hello")],
        system: None,
        tools: Vec::new(),
        stream: None,
        extra: serde_json::Map::new(),
    }
}

fn json_reply(name: &str) -> Vec<Step> {
    vec![Step::Send(http_response(
        200,
        &[
            ("content-type", "application/json"),
            ("request-id", "req_test_json"),
        ],
        fixture(name).as_bytes(),
    ))]
}

fn sse_reply(body: &str) -> Vec<u8> {
    let mut bytes = sse_head();
    bytes.extend_from_slice(body.as_bytes());
    bytes
}

/// The stream up to (not including) `content_block_stop`, then a connection held open.
fn held_stream() -> Vec<Step> {
    let head = sse_prefix(&fixture("anthropic/stream_text.sse"), "content_block_stop");
    vec![Step::Send(sse_reply(&head)), Step::Hang]
}

fn full_stream() -> Vec<Step> {
    vec![Step::Send(sse_reply(&fixture("anthropic/stream_text.sse")))]
}

#[tokio::test]
async fn requests_carry_the_headers_and_body_the_api_expects() {
    let server = Server::start(vec![
        json_reply("anthropic/message_end_turn.json"),
        full_stream(),
    ])
    .await;
    let client = client(endpoint(server.addr, 4));
    let options = RequestOptions {
        betas: vec!["beta-one".into(), "beta-two".into()],
    };
    let mut streaming_request = request();
    // The caller's own `stream` value never decides; the method does.
    streaming_request.stream = Some(false);

    let reply = client.create_message(&request(), &options).await.unwrap();
    let expected: Message =
        serde_json::from_value(fixture_json("anthropic/message_end_turn.json")).unwrap();
    assert_eq!(reply.body, expected);
    assert_eq!(reply.request_id.as_deref(), Some("req_test_json"));

    let stream = client
        .stream_message(&streaming_request, &RequestOptions::default())
        .await
        .unwrap();
    assert_eq!(stream.request_id(), Some("req_test_stream"));
    let streamed = stream.into_message().await.unwrap();
    assert_eq!(
        streamed.content,
        vec![ai_clients::anthropic::ContentBlock::text("Hello!")]
    );

    let plain = server.request(0);
    assert!(
        plain.head.starts_with("POST /v1/messages HTTP/1.1\r\n"),
        "{}",
        plain.head
    );
    assert_eq!(plain.header("x-api-key"), Some(support::key_text()));
    assert_eq!(
        plain.header("anthropic-version").as_deref(),
        Some("2023-06-01")
    );
    assert_eq!(
        plain.header("anthropic-beta").as_deref(),
        Some("beta-one,beta-two")
    );
    assert_eq!(
        plain.header("content-type").as_deref(),
        Some("application/json")
    );
    assert_eq!(plain.header("authorization"), None);
    assert_eq!(plain.json(), serde_json::to_value(request()).unwrap());
    assert!(plain.json().get("stream").is_none());

    let streaming = server.request(1);
    assert_eq!(streaming.json()["stream"], true);
    assert_eq!(
        streaming.header("accept").as_deref(),
        Some("text/event-stream")
    );
    assert_eq!(streaming.header("anthropic-beta"), None);
}

#[tokio::test]
async fn recorded_error_responses_are_classified() {
    let cases = fixture_json("anthropic/errors.json");
    for case in cases.as_array().unwrap() {
        let situation = case["situation"].as_str().unwrap();
        let status = u16::try_from(case["status"].as_u64().unwrap()).unwrap();
        let mut headers = vec![("content-type", "application/json")];
        let retry = case["retry_after"].as_str();
        if let Some(retry) = retry {
            headers.push(("retry-after", retry));
        }
        let request_id = case["body"]["request_id"].as_str();
        if let Some(id) = request_id {
            headers.push(("request-id", id));
        }
        let body = serde_json::to_vec(&case["body"]).unwrap();
        let reply = vec![Step::Send(http_response(status, &headers, &body))];
        let server = Server::start(vec![reply]).await;
        let client = client(endpoint(server.addr, 4));

        let retry_after = retry.map(|seconds| Duration::from_secs(seconds.parse().unwrap()));
        let expected = match status {
            429 => ErrorClass::RateLimited { retry_after },
            529 => ErrorClass::Overloaded,
            504 => ErrorClass::Timeout(TimeoutKind::Provider),
            _ => ErrorClass::Other,
        };
        for streaming in [false, true] {
            let error = if streaming {
                client
                    .stream_message(&request(), &RequestOptions::default())
                    .await
                    .unwrap_err()
            } else {
                client
                    .create_message(&request(), &RequestOptions::default())
                    .await
                    .unwrap_err()
            };
            assert_eq!(
                error.class(),
                expected,
                "{situation}, streaming: {streaming}"
            );
            assert_eq!(error.request_id(), request_id, "{situation}");
            let Error::Api(api) = &error else {
                panic!("{situation}: expected an API error, got {error:?}");
            };
            assert_eq!(api.status, Some(status));
            assert_eq!(
                api.body.error_type.as_deref(),
                case["body"]["error"]["type"].as_str()
            );
            assert_eq!(api.retry_after, retry_after, "{situation}");
        }
    }
}

#[tokio::test]
async fn an_error_event_mid_stream_is_classified_with_the_request_id() {
    let server = Server::start(vec![vec![Step::Send(sse_reply(&fixture(
        "anthropic/stream_overloaded.sse",
    )))]])
    .await;
    let client = client(endpoint(server.addr, 4));
    let mut stream = client
        .stream_message(&request(), &RequestOptions::default())
        .await
        .unwrap();
    let error = loop {
        match stream.next().await {
            Ok(Some(_)) => {}
            Ok(None) => panic!("the stream finished"),
            Err(error) => break error,
        }
    };
    assert!(matches!(error, Error::StreamError(_)), "{error:?}");
    assert_eq!(error.class(), ErrorClass::Overloaded);
    assert_eq!(error.request_id(), Some("req_test_stream"));
}

#[tokio::test]
async fn requests_over_the_concurrency_limit_are_not_sent() {
    let mut server = Server::start(vec![held_stream(), full_stream()]).await;
    let client = client(endpoint(server.addr, 1));
    let other_handle = client.clone();

    let mut held = client
        .stream_message(&request(), &RequestOptions::default())
        .await
        .unwrap();
    held.next().await.unwrap().unwrap();
    server.wait_for_requests(1).await;

    for error in [
        other_handle
            .stream_message(&request(), &RequestOptions::default())
            .await
            .unwrap_err(),
        other_handle
            .create_message(&request(), &RequestOptions::default())
            .await
            .unwrap_err(),
    ] {
        assert!(
            matches!(error, Error::ConcurrencyLimited { .. }),
            "{error:?}"
        );
        assert_eq!(error.class(), ErrorClass::ConcurrencyLimited);
    }
    assert_eq!(
        server.request_count(),
        1,
        "a request over the limit reached the server"
    );

    // Cancelling frees the slot.
    held.cancel();
    server.wait_for_client_closes(1).await;
    let next = other_handle
        .stream_message(&request(), &RequestOptions::default())
        .await
        .unwrap()
        .into_message()
        .await
        .unwrap();
    assert_eq!(
        next.stop_reason,
        Some(ai_clients::anthropic::StopReason::EndTurn)
    );
    assert_eq!(server.request_count(), 2);
}

#[tokio::test]
async fn dropping_a_stream_closes_the_connection_and_frees_the_slot() {
    let mut server = Server::start(vec![held_stream(), full_stream()]).await;
    let client = client(endpoint(server.addr, 1));
    let mut held = client
        .stream_message(&request(), &RequestOptions::default())
        .await
        .unwrap();
    held.next().await.unwrap().unwrap();
    drop(held);
    server.wait_for_client_closes(1).await;
    client
        .stream_message(&request(), &RequestOptions::default())
        .await
        .unwrap()
        .into_message()
        .await
        .unwrap();
}

#[tokio::test]
async fn a_queued_request_waits_for_a_free_slot_up_to_the_queue_wait() {
    let server = Server::start(vec![
        held_stream(),
        json_reply("anthropic/message_end_turn.json"),
    ])
    .await;
    let short = client(EndpointConfig {
        queue_wait: Duration::from_millis(200),
        ..endpoint(server.addr, 1)
    });
    let mut held = short
        .stream_message(&request(), &RequestOptions::default())
        .await
        .unwrap();
    held.next().await.unwrap().unwrap();
    let error = short
        .create_message(&request(), &RequestOptions::default())
        .await
        .unwrap_err();
    assert!(
        matches!(error, Error::ConcurrencyLimited { .. }),
        "{error:?}"
    );
    assert_eq!(server.request_count(), 1);

    drop(held);

    // With a long queue wait the request goes out once the slot frees up.
    let queued_server = Server::start(vec![
        held_stream(),
        json_reply("anthropic/message_end_turn.json"),
    ])
    .await;
    let queued = client(EndpointConfig {
        queue_wait: Duration::from_secs(120),
        ..endpoint(queued_server.addr, 1)
    });
    let mut held = queued
        .stream_message(&request(), &RequestOptions::default())
        .await
        .unwrap();
    held.next().await.unwrap().unwrap();
    let waiting = {
        let queued = queued.clone();
        tokio::spawn(async move {
            queued
                .create_message(&request(), &RequestOptions::default())
                .await
        })
    };
    tokio::task::yield_now().await;
    assert_eq!(
        queued_server.request_count(),
        1,
        "the queued request was sent early"
    );
    held.cancel();
    let reply = tokio::time::timeout(support::WAIT, waiting)
        .await
        .expect("the queued request did not finish after the slot freed up")
        .unwrap()
        .unwrap();
    assert_eq!(reply.request_id.as_deref(), Some("req_test_json"));
    assert_eq!(queued_server.request_count(), 2);
}

#[tokio::test]
async fn no_response_headers_in_time_are_first_token_and_response_timeouts() {
    let server = Server::start(vec![vec![Step::Hang]]).await;
    let mut config = endpoint(server.addr, 4);
    config.timeouts.first_token = Duration::from_millis(300);
    config.timeouts.response = Duration::from_millis(300);
    let client = client(config);
    let error = client
        .stream_message(&request(), &RequestOptions::default())
        .await
        .unwrap_err();
    assert_eq!(
        error.class(),
        ErrorClass::Timeout(TimeoutKind::FirstToken),
        "{error:?}"
    );
    let error = client
        .create_message(&request(), &RequestOptions::default())
        .await
        .unwrap_err();
    assert_eq!(
        error.class(),
        ErrorClass::Timeout(TimeoutKind::Response),
        "{error:?}"
    );
}

#[tokio::test]
async fn headers_without_model_output_are_a_first_token_timeout() {
    let head = sse_prefix(&fixture("anthropic/stream_text.sse"), "content_block_start");
    let server = Server::start(vec![vec![Step::Send(sse_reply(&head)), Step::Hang]]).await;
    let mut config = endpoint(server.addr, 4);
    config.timeouts.first_token = Duration::from_millis(500);
    let client = client(config);
    let mut stream = client
        .stream_message(&request(), &RequestOptions::default())
        .await
        .unwrap();
    let error = loop {
        match stream.next().await {
            Ok(Some(_)) => {}
            Ok(None) => panic!("the stream finished"),
            Err(error) => break error,
        }
    };
    assert_eq!(
        error.class(),
        ErrorClass::Timeout(TimeoutKind::FirstToken),
        "{error:?}"
    );
}

#[tokio::test]
async fn a_stream_that_goes_silent_is_a_stream_idle_timeout() {
    let server = Server::start(vec![held_stream()]).await;
    let mut config = endpoint(server.addr, 4);
    config.timeouts.stream_idle = Duration::from_millis(300);
    let client = client(config);
    let mut stream = client
        .stream_message(&request(), &RequestOptions::default())
        .await
        .unwrap();
    let mut output = false;
    let error = loop {
        match stream.next().await {
            Ok(Some(item)) => output |= item.event.is_output(),
            Ok(None) => panic!("the stream finished"),
            Err(error) => break error,
        }
    };
    assert!(output, "model output arrived before the silence");
    assert_eq!(
        error.class(),
        ErrorClass::Timeout(TimeoutKind::StreamIdle),
        "{error:?}"
    );
}

/// A listener whose accept queue is full: further connection attempts get no answer.
fn unanswering_listener() -> (socket2::Socket, SocketAddr) {
    use socket2::{Domain, Socket, Type};
    let socket = Socket::new(Domain::IPV4, Type::STREAM, None).expect("a TCP socket");
    let any: SocketAddr = "127.0.0.1:0".parse().expect("a socket address");
    socket.bind(&any.into()).expect("bind a loopback port");
    socket.listen(0).expect("listen");
    let addr = socket
        .local_addr()
        .expect("the listener address")
        .as_socket()
        .expect("an IP address");
    (socket, addr)
}

#[tokio::test]
async fn an_unanswered_connection_attempt_is_a_connect_timeout() {
    let (_listener, addr) = unanswering_listener();
    // Fill the accept queue (nothing accepts) until an attempt gets no answer.
    let mut fillers = Vec::new();
    let mut full = false;
    for _ in 0..64 {
        match tokio::time::timeout(Duration::from_secs(5), tokio::net::TcpStream::connect(addr))
            .await
        {
            Ok(Ok(stream)) => fillers.push(stream),
            Ok(Err(error)) => panic!("filling the accept queue failed: {error}"),
            Err(_) => {
                full = true;
                break;
            }
        }
    }
    assert!(
        full,
        "the accept queue never filled up after 64 connections"
    );

    let mut config = endpoint(addr, 4);
    config.timeouts.connect = Duration::from_millis(500);
    let client = client(config);
    let error = client
        .create_message(&request(), &RequestOptions::default())
        .await
        .unwrap_err();
    assert_eq!(
        error.class(),
        ErrorClass::Timeout(TimeoutKind::Connect),
        "{error:?}"
    );
    let error = client
        .stream_message(&request(), &RequestOptions::default())
        .await
        .unwrap_err();
    assert_eq!(
        error.class(),
        ErrorClass::Timeout(TimeoutKind::Connect),
        "{error:?}"
    );
}

#[tokio::test]
async fn redirects_are_not_followed() {
    let server = Server::start(vec![vec![Step::Send(http_response(
        307,
        &[("location", "/v1/elsewhere")],
        b"",
    ))]])
    .await;
    let client = client(endpoint(server.addr, 4));
    let error = client
        .create_message(&request(), &RequestOptions::default())
        .await
        .unwrap_err();
    let Error::Api(api) = &error else {
        panic!("expected the redirect as an error, got {error:?}");
    };
    assert_eq!(api.status, Some(307));
    assert_eq!(error.class(), ErrorClass::Other);
    tokio::task::yield_now().await;
    assert_eq!(server.request_count(), 1);
}

#[tokio::test]
async fn responses_that_break_the_protocol_are_refused() {
    // A streaming request answered with JSON.
    let server = Server::start(vec![json_reply("anthropic/message_end_turn.json")]).await;
    let error = client(endpoint(server.addr, 4))
        .stream_message(&request(), &RequestOptions::default())
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Protocol(_)), "{error:?}");

    // A body over the size limit.
    let server = Server::start(vec![json_reply("anthropic/message_end_turn.json")]).await;
    let error = client(EndpointConfig {
        max_response_bytes: 64,
        ..endpoint(server.addr, 4)
    })
    .create_message(&request(), &RequestOptions::default())
    .await
    .unwrap_err();
    assert!(matches!(error, Error::Protocol(_)), "{error:?}");

    // A body that is not a message.
    let server = Server::start(vec![vec![Step::Send(http_response(
        200,
        &[("content-type", "application/json")],
        b"{\"type\":\"message\"}",
    ))]])
    .await;
    let error = client(endpoint(server.addr, 4))
        .create_message(&request(), &RequestOptions::default())
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Protocol(_)), "{error:?}");
}

#[test]
fn endpoints_must_be_https_or_loopback_with_a_positive_limit() {
    let build = |config: EndpointConfig| {
        AnthropicClient::new(config, support::test_key(), reqwest::Client::builder())
    };
    assert!(
        build(EndpointConfig::new(
            "anthropic",
            ai_clients::anthropic::DEFAULT_BASE_URL
        ))
        .is_ok()
    );
    assert!(matches!(
        build(EndpointConfig::new("plain", "http://api.anthropic.com")),
        Err(Error::InvalidRequest(_))
    ));
    let mut zero = EndpointConfig::new("zero", "http://127.0.0.1:1");
    zero.max_concurrent = 0;
    assert!(matches!(build(zero), Err(Error::InvalidRequest(_))));
    // The client's Debug shows no key.
    let client = build(EndpointConfig::new("anthropic", "http://127.0.0.1:1")).unwrap();
    assert!(!format!("{client:?}").contains(&support::key_text()));
}
