//! HTTPS endpoints: the client verifies the provider's certificate against the roots it was
//! given and refuses any other certificate before sending the request (fail closed). There is no
//! option to skip the check.

mod support;

use ai_clients::anthropic::{AnthropicClient, InputMessage, MessagesRequest, RequestOptions};
use ai_clients::{Error, ErrorClass, Tls};
use support::{Server, Step, TestCa, fixture, http_response, tls_endpoint};

fn request() -> MessagesRequest {
    MessagesRequest {
        model: "claude-opus-5-5".into(),
        max_tokens: 16,
        messages: vec![InputMessage::user_text("Hello")],
        system: None,
        tools: Vec::new(),
        stream: None,
        extra: serde_json::Map::new(),
    }
}

fn reply() -> Vec<Step> {
    vec![Step::Send(http_response(
        200,
        &[("content-type", "application/json")],
        fixture("anthropic/message_end_turn.json").as_bytes(),
    ))]
}

#[tokio::test]
async fn a_certificate_from_a_trusted_root_carries_the_request() {
    let ca = TestCa::generate();
    let server = Server::start_tls(vec![reply()], ca.acceptor(&["127.0.0.1"])).await;
    let client =
        AnthropicClient::new(tls_endpoint(server.addr), support::test_key(), &ca.tls()).unwrap();
    let message = client
        .create_message(&request(), &RequestOptions::default())
        .await
        .unwrap()
        .body;
    assert_eq!(message.id, "msg_01234");
    assert_eq!(
        server.request(0).header("x-api-key"),
        Some(support::key_text())
    );
}

#[tokio::test]
async fn certificates_the_client_does_not_trust_are_refused_before_the_request() {
    let trusted = TestCa::generate();
    let other = TestCa::generate();
    // Signed by a CA the client does not know, and valid for another name.
    for (acceptor, why) in [
        (other.acceptor(&["127.0.0.1"]), "an unknown issuer"),
        (trusted.acceptor(&["localhost"]), "another host name"),
    ] {
        let server = Server::start_tls(vec![reply()], acceptor).await;
        let client = AnthropicClient::new(
            tls_endpoint(server.addr),
            support::test_key(),
            &trusted.tls(),
        )
        .unwrap();
        let error = client
            .create_message(&request(), &RequestOptions::default())
            .await
            .unwrap_err();
        assert!(matches!(error, Error::Transport(_)), "{why}: {error:?}");
        assert_eq!(error.class(), ErrorClass::Other, "{why}");
        assert!(
            error.to_string().contains("certificate"),
            "{why}: the message should name the certificate problem: {error}"
        );
        assert_eq!(
            server.request_count(),
            0,
            "{why}: the request reached a server the client does not trust"
        );
    }
}

#[test]
fn a_tls_configuration_needs_a_root() {
    assert!(matches!(
        Tls::roots(std::iter::empty()),
        Err(Error::InvalidRequest(_))
    ));
    assert_eq!(format!("{:?}", Tls::none()), "Tls(none)");
    assert_eq!(format!("{:?}", TestCa::generate().tls()), "Tls(verified)");
}
