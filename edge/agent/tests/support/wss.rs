//! A gateway set up the way the binary sets itself up: configuration file, certificate and file
//! key in a scratch directory, [`Setup`], and the link; a stand-in front, and a stand-in broker
//! behind it with the broker's mTLS settings ([`InnerBroker`]).
//! Used by the tests that go through real sockets.

#![allow(
    clippy::unwrap_used,
    reason = "test helpers unwrap like the tests themselves (clippy.toml allows it in tests)"
)]

use std::net::SocketAddr;
use std::sync::Arc;

use agent::config::Config;
use agent::link::{self, Incoming, Link, LinkState, Subscriptions};
use agent::setup::Setup;
use rustls::{CipherSuite, HandshakeKind, ProtocolVersion};
use testkit::clock::bounded;
use testkit::mqtt::front::{FrontOptions, WsFront};
use testkit::mqtt::{BROKER_NAME, FRONT_HOST, ScratchDir, TestPki};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::server::TlsStream;
use tokio_util::sync::CancellationToken;

use super::{FakeBroker, GATEWAY, gateway};

/// The files of one gateway: the configuration and what it names.
pub struct Files {
    dir: ScratchDir,
}

impl Files {
    pub fn new(pki: &TestPki, front_port: u16) -> Self {
        let dir = ScratchDir::new("agent-transport");
        let leaf = pki.gateway(GATEWAY);
        let files = Self { dir };
        files.write("public-ca.pem", &pki.public_ca.pem);
        files.write("broker-ca.pem", &pki.broker_ca.pem);
        files.write("gateway.pem", &leaf.cert_pem);
        files.write("gateway.key", &leaf.key_pem);
        files.write_config(front_port, "public-ca.pem", BROKER_NAME);
        files
    }

    pub fn write(&self, name: &str, contents: &str) {
        self.dir.write(name, contents);
    }

    pub fn path(&self, name: &str) -> String {
        self.dir.path().join(name).to_str().unwrap().to_owned()
    }

    pub fn write_config(&self, front_port: u16, outer_trust: &str, broker_name: &str) {
        let config = format!(
            r#"
[cloud]
url = "wss://{FRONT_HOST}:{front_port}/mqtt"
trust = [{trust:?}]

[broker]
name = "{broker_name}"
ca = {ca:?}

[identity]
certificate = {certificate:?}
key = {{ source = "file", path = {key:?} }}

[connection]
heartbeat_interval_s = 3600
reconnect_base_ms = 100
reconnect_cap_ms = 1000
"#,
            trust = self.path(outer_trust),
            ca = self.path("broker-ca.pem"),
            certificate = self.path("gateway.pem"),
            key = self.path("gateway.key"),
        );
        self.write("agent.toml", &config);
    }

    pub fn gateway_cert(&self) -> Vec<u8> {
        let pem = std::fs::read(self.path("gateway.pem")).unwrap();
        use rustls_pki_types::pem::PemObject;
        rustls_pki_types::CertificateDer::from_pem_slice(&pem)
            .unwrap()
            .to_vec()
    }
}

pub struct Agent {
    pub link: Link,
    pub incoming: Incoming,
    pub shutdown: CancellationToken,
    pub supervisor: JoinHandle<()>,
}

impl Agent {
    /// The agent as the binary sets it up, subscribed to nothing.
    pub fn start(files: &Files) -> Self {
        Self::start_with(files, Subscriptions::none())
    }

    /// The agent as the binary sets it up, subscribed to `subscriptions`.
    pub fn start_with(files: &Files, subscriptions: Subscriptions) -> Self {
        let config = Config::load(std::path::Path::new(&files.path("agent.toml"))).unwrap();
        let mut setup = Setup::new(&config).unwrap();
        setup.link.subscriptions = subscriptions;
        assert_eq!(
            setup.gateway,
            gateway(),
            "the gateway ID is the certificate's CN"
        );
        assert!(setup.file_key);
        let shutdown = CancellationToken::new();
        let (link, incoming, supervisor) =
            link::link(setup.link, setup.connector, shutdown.clone());
        Self {
            link,
            incoming,
            shutdown,
            supervisor: tokio::spawn(supervisor.run()),
        }
    }

    pub async fn wait_state(
        &self,
        what: &str,
        wanted: impl FnMut(&LinkState) -> bool,
    ) -> LinkState {
        let mut state = self.link.state();
        bounded(what, state.wait_for(wanted)).await.unwrap().clone()
    }

    /// The error of the first failed attempt.
    pub async fn first_error(&self) -> String {
        match self
            .wait_state("a failed attempt", |state| {
                matches!(
                    state,
                    LinkState::Disconnected { .. } | LinkState::Connected { .. }
                )
            })
            .await
        {
            LinkState::Disconnected { error, .. } => error,
            other => panic!("the agent connected: {other:?}"),
        }
    }

    pub async fn stop(self) {
        self.shutdown.cancel();
        bounded("the supervisor to stop", self.supervisor)
            .await
            .unwrap();
    }
}

/// A front relaying to `upstream` with the given options.
pub async fn front(
    pki: &TestPki,
    upstream: SocketAddr,
    adjust: impl FnOnce(&mut FrontOptions),
) -> WsFront {
    let mut options = FrontOptions::new(pki.front_tls(), upstream);
    adjust(&mut options);
    WsFront::start(options).await.unwrap()
}

/// What the stand-in broker's TLS accept produced.
pub type Accepted = Result<TlsStream<TcpStream>, String>;

/// A TLS server with the broker's mTLS settings; hands every accepted connection to the test.
pub struct InnerBroker {
    pub address: SocketAddr,
    accepted: mpsc::UnboundedReceiver<Accepted>,
    task: JoinHandle<()>,
}

impl InnerBroker {
    pub async fn start(tls: Arc<rustls::ServerConfig>) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let (sender, accepted) = mpsc::unbounded_channel();
        let acceptor = TlsAcceptor::from(tls);
        let task = tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                let accepted = acceptor
                    .accept(tcp)
                    .await
                    .map_err(|error| error.to_string());
                if sender.send(accepted).is_err() {
                    break;
                }
            }
        });
        Self {
            address,
            accepted,
            task,
        }
    }

    pub async fn next(&mut self) -> Accepted {
        bounded("the broker to accept a connection", self.accepted.recv())
            .await
            .unwrap()
    }

    pub async fn next_broker(&mut self) -> (FakeBroker<TlsStream<TcpStream>>, HandshakeKind) {
        let stream = self.next().await.unwrap();
        let (_, server) = stream.get_ref();
        assert_eq!(server.protocol_version(), Some(ProtocolVersion::TLSv1_3));
        assert_eq!(
            server.negotiated_cipher_suite().map(|suite| suite.suite()),
            Some(CipherSuite::TLS13_CHACHA20_POLY1305_SHA256),
            "the client prefers ChaCha20 (no AES instructions on the gateway)"
        );
        let kind = server.handshake_kind().unwrap();
        (FakeBroker::new(stream), kind)
    }
}

impl Drop for InnerBroker {
    fn drop(&mut self) {
        self.task.abort();
    }
}
