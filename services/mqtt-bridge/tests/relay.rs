//! The relay moves bytes and nothing else: what the gateway sends reaches the broker unchanged
//! and in order, and back; the gateway's mTLS to the broker passes through the bridge, which has
//! no key and cannot terminate it; the bridge's messages are at most 8 KiB.

mod support;

use std::sync::Arc;

use futures_util::{SinkExt as _, StreamExt as _};
use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, Issuer, KeyPair};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use support::{
    Behaviour, Bridge, FakeBroker, WsBytes, connect_ws, noise, read_bytes, send_binary,
    via_cloudflare, wait_until, within,
};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

#[tokio::test]
async fn bytes_pass_unchanged_both_ways_in_messages_of_at_most_8_kib() {
    let broker = FakeBroker::start(Behaviour::Echo).await;
    let mut bridge = Bridge::start(broker.address, &[]).await;
    let mut websocket = connect_ws(
        bridge.address,
        &bridge.url(),
        &via_cloudflare("203.0.113.7"),
    )
    .await
    .expect("open the WebSocket");

    // Sizes up to the gateway's largest message (32 KiB), including bytes that look like a TLS
    // record header, MQTT packets and zeros: none of them may be read as anything.
    let mut sent = Vec::new();
    let mut tls_like = vec![0x16, 0x03, 0x03, 0x00, 0x05, 0x01, 0x00, 0x00, 0x01, 0x00];
    tls_like.extend_from_slice(&[0; 32]);
    send_binary(&mut websocket, &tls_like).await;
    sent.extend_from_slice(&tls_like);
    for (seed, size) in [
        (1, 1),
        (2, 1000),
        (3, 8 * 1024),
        (4, 8 * 1024 + 1),
        (5, 32 * 1024),
    ] {
        let chunk = noise(seed, size);
        send_binary(&mut websocket, &chunk).await;
        sent.extend_from_slice(&chunk);
    }
    let (echoed, sizes) = read_bytes(&mut websocket, sent.len()).await;
    assert_eq!(
        echoed, sent,
        "the broker's bytes reach the gateway unchanged and in order"
    );
    assert!(
        sizes.iter().all(|size| *size <= 8 * 1024),
        "every message from the bridge is at most 8 KiB: {sizes:?}"
    );
    let at_broker = wait_until("the broker to have every byte", || {
        let connection = broker.connections().into_iter().next()?;
        (connection.received.len() >= sent.len()).then_some(connection)
    })
    .await;
    assert_eq!(
        at_broker.received, sent,
        "the gateway's bytes reach the broker unchanged"
    );

    // The connection's log lines carry the counts, never the bytes.
    within("the close", websocket.close(None))
        .await
        .expect("close");
    let ended = bridge.wait_for_line("gateway connection ended").await;
    assert_eq!(ended["fields"]["reason"], "gateway_closed", "{ended}");
    assert_eq!(ended["fields"]["bytes_to_broker"], sent.len(), "{ended}");
    assert_eq!(ended["fields"]["bytes_to_gateway"], sent.len(), "{ended}");
    wait_until("the broker connection to close", || {
        broker.connections()[0].closed.then_some(())
    })
    .await;
}

#[tokio::test]
async fn the_gateways_mtls_goes_through_to_the_broker() {
    // A CA, the broker's server certificate for the name the gateway verifies, and a gateway's
    // client certificate: the broker requires it, as Mosquitto's gateway port does.
    let ca_key = KeyPair::generate().expect("CA key");
    let mut ca = CertificateParams::new(Vec::new()).expect("CA params");
    ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca.distinguished_name.push(DnType::CommonName, "test root");
    let ca_cert = ca.self_signed(&ca_key).expect("CA certificate");
    let issuer = Issuer::new(ca, ca_key);
    let server_key = KeyPair::generate().expect("server key");
    let server = CertificateParams::new(vec!["mqtt-broker.trustiot.internal".to_owned()])
        .expect("server params")
        .signed_by(&server_key, &issuer)
        .expect("server certificate");
    let client_key = KeyPair::generate().expect("client key");
    let mut client_params = CertificateParams::new(Vec::new()).expect("client params");
    client_params
        .distinguished_name
        .push(DnType::CommonName, "gw-test");
    let client = client_params
        .signed_by(&client_key, &issuer)
        .expect("client certificate");

    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(CertificateDer::from(ca_cert.der().to_vec()))
        .expect("trust the CA");
    let roots = Arc::new(roots);
    let verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(
        Arc::clone(&roots),
        Arc::clone(&provider),
    )
    .build()
    .expect("client verifier");
    let server_config = rustls::ServerConfig::builder_with_provider(Arc::clone(&provider))
        .with_protocol_versions(&[&rustls::version::TLS13])
        .expect("TLS 1.3")
        .with_client_cert_verifier(verifier)
        .with_single_cert(
            vec![CertificateDer::from(server.der().to_vec())],
            PrivateKeyDer::try_from(server_key.serialize_der()).expect("server key DER"),
        )
        .expect("server config");
    let client_config = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .expect("TLS 1.3")
        .with_root_certificates(roots)
        .with_client_auth_cert(
            vec![CertificateDer::from(client.der().to_vec())],
            PrivateKeyDer::try_from(client_key.serialize_der()).expect("client key DER"),
        )
        .expect("client config");

    let broker = FakeBroker::start(Behaviour::Tls(Arc::new(server_config))).await;
    let mut bridge = Bridge::start(broker.address, &[]).await;
    let websocket = connect_ws(
        bridge.address,
        &bridge.url(),
        &via_cloudflare("203.0.113.7"),
    )
    .await
    .expect("open the WebSocket");
    let name = ServerName::try_from("mqtt-broker.trustiot.internal").expect("server name");
    let mut tls = within(
        "the inner TLS handshake through the bridge",
        tokio_rustls::TlsConnector::from(Arc::new(client_config))
            .connect(name, WsBytes::new(websocket)),
    )
    .await
    .expect("the gateway verifies the broker's certificate end to end");
    let marker = b"plaintext-only-the-broker-sees";
    tls.write_all(marker).await.expect("write through TLS");
    tls.flush().await.expect("flush");
    let mut echoed = vec![0; marker.len()];
    within("the echo through TLS", tls.read_exact(&mut echoed))
        .await
        .expect("read through TLS");
    assert_eq!(echoed, marker);
    let at_broker = wait_until("the broker's plaintext", || {
        let connection = broker.connections().into_iter().next()?;
        (connection.received.len() >= marker.len()).then_some(connection)
    })
    .await;
    assert_eq!(
        at_broker.received, marker,
        "the broker decrypted what the gateway sent"
    );
    within("the TLS shutdown", tls.shutdown())
        .await
        .expect("shutdown");
    drop(tls);
    bridge.wait_for_line("gateway connection ended").await;
    let marker_text = String::from_utf8_lossy(marker);
    assert!(
        !bridge.dump().contains(marker_text.as_ref()),
        "the bridge cannot see, and does not log, the inner plaintext"
    );
}

#[tokio::test]
async fn a_text_message_ends_the_connection() {
    let broker = FakeBroker::start(Behaviour::Echo).await;
    let mut bridge = Bridge::start(broker.address, &[]).await;
    let mut websocket = connect_ws(
        bridge.address,
        &bridge.url(),
        &via_cloudflare("203.0.113.7"),
    )
    .await
    .expect("open the WebSocket");
    within("sending text", websocket.send(Message::text("CONNECT")))
        .await
        .expect("send text");
    let close = loop {
        match within("the bridge's close", websocket.next()).await {
            Some(Ok(Message::Close(frame))) => break frame,
            Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
            other => panic!("expected a close, got {other:?}"),
        }
    };
    assert_eq!(close.map(|frame| frame.code), Some(CloseCode::Unsupported));
    let ended = bridge.wait_for_line("gateway connection ended").await;
    assert_eq!(ended["fields"]["reason"], "text_message", "{ended}");
    let connection = wait_until("the broker connection to close", || {
        let connection = broker.connections().into_iter().next()?;
        connection.closed.then_some(connection)
    })
    .await;
    assert!(
        connection.received.is_empty(),
        "the text never reached the broker"
    );
}

#[tokio::test]
async fn when_the_broker_closes_the_gateway_gets_its_last_bytes_then_a_close() {
    let broker = FakeBroker::start(Behaviour::Mqtt).await;
    let mut bridge = Bridge::start(broker.address, &[]).await;
    let mut websocket = support::mqtt_session(bridge.address, &bridge.url(), "203.0.113.7").await;
    // DISCONNECT: the broker stand-in ends the connection.
    send_binary(&mut websocket, &[0xe0, 0x00]).await;
    let close = loop {
        match within("the bridge's close", websocket.next()).await {
            Some(Ok(Message::Close(frame))) => break frame,
            Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
            other => panic!("expected a close, got {other:?}"),
        }
    };
    assert_eq!(close.map(|frame| frame.code), Some(CloseCode::Normal));
    let ended = bridge.wait_for_line("gateway connection ended").await;
    assert_eq!(ended["fields"]["reason"], "broker_closed", "{ended}");
}
