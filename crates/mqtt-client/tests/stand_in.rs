//! The connection task against a stand-in broker on loopback, no container needed:
//!
//! - a shutdown is served while the in-flight window is full and the broker never answers;
//! - a publish made while the connection is being made fails at once with
//!   [`PublishError::NotConnected`] and is never sent.
//!
//! The stand-in answers CONNECT itself, well within the client's `CONNECT_TIMEOUT`.

#![allow(
    clippy::unwrap_used,
    reason = "tests unwrap (clippy.toml allows it in tests)"
)]

mod support;

use futures_util::{SinkExt as _, StreamExt as _};
use mqtt_client::{Client, Config, Event, PublishError, PublishOptions};
use rumqttc::v5::mqttbytes::v5::{
    Codec, ConnAck, ConnAckProperties, ConnectReturnCode, Packet, PubAck,
};
use support::{EVENT_WAIT, Scratch, USERNAME, evidence, expect_connected, random_password};
use tokio::io::AsyncReadExt as _;
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;
use tokio_util::codec::Framed;

type Wire = Framed<TcpStream, Codec>;

async fn listen() -> (TcpListener, u16) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    (listener, port)
}

fn config(port: u16, scratch: &Scratch) -> Config {
    Config {
        host: "127.0.0.1".to_owned(),
        port,
        username: USERNAME.to_owned(),
        password_file: scratch.write("password", &random_password()),
    }
}

/// The client's connection, after its CONNECT.
async fn accept(listener: &TcpListener) -> Wire {
    let (stream, _) = timeout(EVENT_WAIT, listener.accept())
        .await
        .expect("no connection from the client within EVENT_WAIT")
        .unwrap();
    let mut wire = Framed::new(
        stream,
        Codec {
            max_incoming_size: None,
            max_outgoing_size: None,
        },
    );
    assert_eq!(next_packet(&mut wire, "CONNECT").await, "CONNECT");
    wire
}

/// The next packet's type; the packet itself is not printed (CONNECT holds the password).
async fn next_packet(wire: &mut Wire, what: &str) -> &'static str {
    match timeout(EVENT_WAIT, wire.next()).await {
        Ok(Some(Ok(packet))) => match packet {
            Packet::Connect(..) => "CONNECT",
            Packet::Publish(_) => "PUBLISH",
            Packet::PingReq(_) => "PINGREQ",
            _ => "another packet",
        },
        Ok(Some(Err(error))) => panic!("cannot read {what}: {error}"),
        Ok(None) => panic!("the client closed the connection before {what}"),
        Err(_) => panic!("no {what} within {EVENT_WAIT:?}"),
    }
}

/// Every byte the client sends until it closes the connection.
async fn rest(wire: Wire) -> Vec<u8> {
    let parts = wire.into_parts();
    let mut bytes = parts.read_buf.to_vec();
    let mut stream = parts.io;
    timeout(EVENT_WAIT, stream.read_to_end(&mut bytes))
        .await
        .expect("the client did not close the connection within EVENT_WAIT")
        .unwrap();
    bytes
}

fn connack(receive_maximum: u16) -> Packet {
    Packet::ConnAck(ConnAck {
        session_present: false,
        code: ConnectReturnCode::Success,
        properties: Some(ConnAckProperties {
            session_expiry_interval: None,
            receive_max: Some(receive_maximum),
            max_qos: None,
            retain_available: None,
            max_packet_size: None,
            assigned_client_identifier: None,
            topic_alias_max: None,
            reason_string: None,
            user_properties: Vec::new(),
            wildcard_subscription_available: None,
            subscription_identifiers_available: None,
            shared_subscription_available: None,
            server_keep_alive: None,
            response_information: None,
            server_reference: None,
            authentication_method: None,
            authentication_data: None,
        }),
    })
}

fn spawn_publish(
    client: &Client,
    topic: &'static str,
) -> tokio::task::JoinHandle<Result<(), PublishError>> {
    let client = client.clone();
    tokio::spawn(async move {
        client
            .publish(topic, topic, PublishOptions::default())
            .await
    })
}

#[tokio::test]
async fn shutdown_is_served_while_the_inflight_window_is_full() {
    let scratch = Scratch::new();
    let (listener, port) = listen().await;
    let (client, mut events) = Client::start(&config(port, &scratch), Vec::new()).unwrap();
    let mut wire = accept(&listener).await;
    // A Receive Maximum of 1: one unacknowledged publish fills the window.
    wire.send(connack(1)).await.unwrap();
    expect_connected(&mut events, "the connection to the stand-in").await;

    let first = spawn_publish(&client, "test/first");
    assert_eq!(next_packet(&mut wire, "the first PUBLISH").await, "PUBLISH");
    // The stand-in never sends its PUBACK; this one waits for a free slot.
    let second = spawn_publish(&client, "test/second");

    timeout(EVENT_WAIT, client.shutdown())
        .await
        .expect("Client::shutdown did not return while the in-flight window was full");
    // A DISCONNECT with reason 0x00 and no properties, then the end of the connection. Read as
    // bytes: rumqttc 0.25.1 cannot decode a DISCONNECT with remaining length 0.
    let rest = rest(wire).await;
    assert_eq!(rest, [0xe0, 0x00], "the bytes after the shutdown");
    let first = first.await.unwrap();
    let second = second.await.unwrap();
    assert!(matches!(first, Err(PublishError::Closed)), "{first:?}");
    assert!(matches!(second, Err(PublishError::Closed)), "{second:?}");
    while let Some(event) = timeout(EVENT_WAIT, events.recv())
        .await
        .expect("the events did not end after the shutdown")
    {
        assert!(!matches!(event, Event::Disconnected { .. }), "{event:?}");
    }
    evidence(format!(
        "shutdown with a full in-flight window: the client sent {rest:02x?} (DISCONNECT 0x00) and closed; the publish in flight: {first:?}; the waiting one: {second:?}"
    ));
}

#[tokio::test]
async fn a_publish_while_connecting_fails_at_once_and_is_never_sent() {
    let scratch = Scratch::new();
    let (listener, port) = listen().await;
    let (client, mut events) = Client::start(&config(port, &scratch), Vec::new()).unwrap();
    // CONNECT received, no CONNACK yet: the client is connecting.
    let mut wire = accept(&listener).await;
    let early = timeout(
        EVENT_WAIT,
        client.publish("test/early", "early", PublishOptions::default()),
    )
    .await
    .expect("a publish while connecting did not return");
    assert!(
        matches!(early, Err(PublishError::NotConnected)),
        "{early:?}"
    );

    wire.send(connack(16)).await.unwrap();
    expect_connected(&mut events, "the connection to the stand-in").await;
    let later = spawn_publish(&client, "test/later");
    // The first PUBLISH the stand-in sees is the later one: the early one was never queued.
    let publish = match timeout(EVENT_WAIT, wire.next()).await {
        Ok(Some(Ok(Packet::Publish(publish)))) => publish,
        _ => panic!("no PUBLISH within {EVENT_WAIT:?} after the CONNACK"),
    };
    assert_eq!(&publish.topic[..], b"test/later");
    wire.send(Packet::PubAck(PubAck::new(publish.pkid, None)))
        .await
        .unwrap();
    later.await.unwrap().unwrap();
    evidence(format!(
        "publish while connecting: {early:?}; first PUBLISH after the CONNACK: test/later"
    ));
    client.shutdown().await;
}
