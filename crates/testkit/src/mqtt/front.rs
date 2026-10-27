//! An in-process stand-in for the ingress and the MQTT relay (mqtt-bridge): outer TLS, the
//! WebSocket upgrade on `/mqtt` with subprotocol `mqtt`, and a relay of binary messages to a
//! TCP upstream (the broker's mTLS port, or an in-process TLS server). The relay only moves
//! bytes, like mqtt-bridge: the inner mTLS goes from the gateway straight to the upstream.
//!
//! It records what every connection negotiated ([`FrontSession`]), can send WebSocket pings
//! like mqtt-bridge, and can be stopped (every connection cut at once, as a crash would) and
//! started again on the same port.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use rustls::{CipherSuite, ProtocolVersion};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio::task::{JoinHandle, JoinSet};
use tokio_rustls::TlsAcceptor;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::{HeaderValue, StatusCode};
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

use super::stop_listening;

/// How the front behaves.
#[derive(Clone)]
pub struct FrontOptions {
    /// The outer TLS server.
    pub tls: Arc<rustls::ServerConfig>,
    /// Where to relay the bytes.
    pub upstream: SocketAddr,
    /// Send a WebSocket ping this often (mqtt-bridge pings every 50 seconds).
    pub ping_interval: Option<Duration>,
    /// Answer the `mqtt` subprotocol (a front that does not answer it tests the client's check).
    pub answer_subprotocol: bool,
    /// The relay's read buffer (mqtt-bridge uses 8 KiB).
    pub read_buffer: usize,
}

impl FrontOptions {
    /// A front with `tls` relaying to `upstream`, answering the subprotocol, without pings.
    #[must_use]
    pub fn new(tls: Arc<rustls::ServerConfig>, upstream: SocketAddr) -> Self {
        Self {
            tls,
            upstream,
            ping_interval: None,
            answer_subprotocol: true,
            read_buffer: 8 * 1024,
        }
    }
}

impl std::fmt::Debug for FrontOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FrontOptions")
            .field("upstream", &self.upstream)
            .field("ping_interval", &self.ping_interval)
            .field("answer_subprotocol", &self.answer_subprotocol)
            .finish_non_exhaustive()
    }
}

/// What one client connection negotiated with the front.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FrontSession {
    /// The outer TLS version.
    pub tls_version: Option<ProtocolVersion>,
    /// The outer TLS cipher suite.
    pub cipher_suite: Option<CipherSuite>,
    /// The ALPN protocol the client and the front agreed on.
    pub alpn: Option<Vec<u8>>,
    /// Whether the outer TLS handshake resumed an earlier session.
    pub resumed: bool,
    /// The SNI the client sent.
    pub server_name: Option<String>,
    /// The request path of the upgrade.
    pub path: Option<String>,
    /// The `Sec-WebSocket-Protocol` the client asked for.
    pub subprotocols: Option<String>,
    /// Whether the WebSocket upgrade succeeded.
    pub upgraded: bool,
    /// Why the connection failed before relaying, if it did.
    pub error: Option<String>,
}

/// A running front; stopped when dropped.
#[derive(Debug)]
pub struct WsFront {
    address: SocketAddr,
    options: FrontOptions,
    sessions: Arc<Mutex<Vec<FrontSession>>>,
    serving: Option<Serving>,
}

/// The accept loop and the way to end it.
#[derive(Debug)]
struct Serving {
    /// Shared with the loop, so that `Drop`, which cannot wait for it, can still stop listening.
    listener: Arc<TcpListener>,
    stop: oneshot::Sender<()>,
    task: JoinHandle<()>,
}

impl WsFront {
    /// Starts a front on a free port of the loopback interface.
    ///
    /// # Errors
    ///
    /// The port cannot be bound.
    pub async fn start(options: FrontOptions) -> std::io::Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let mut front = Self {
            address: listener.local_addr()?,
            options,
            sessions: Arc::new(Mutex::new(Vec::new())),
            serving: None,
        };
        front.serve(listener);
        Ok(front)
    }

    /// Where the front listens.
    #[must_use]
    pub fn address(&self) -> SocketAddr {
        self.address
    }

    /// Every connection so far, oldest first.
    #[must_use]
    pub fn sessions(&self) -> Vec<FrontSession> {
        self.sessions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Stops listening and cuts every connection without a WebSocket close, as a crashed relay
    /// would. Returns once the listener is closed and every connection has ended, so the port
    /// can be bound again at once.
    ///
    /// # Panics
    ///
    /// When the accept loop panicked.
    pub async fn stop(&mut self) {
        let Some(serving) = self.serving.take() else {
            return;
        };
        // The loop may already have ended; then there is nobody to tell.
        let _ = serving.stop.send(());
        if let Err(error) = serving.task.await
            && error.is_panic()
        {
            std::panic::resume_unwind(error.into_panic());
        }
    }

    /// Whether the front is listening.
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.serving
            .as_ref()
            .is_some_and(|serving| !serving.task.is_finished())
    }

    /// Starts listening again on the same port, after [`Self::stop`] has closed the old
    /// listener (binding while it is still open fails with `AddrInUse`).
    ///
    /// # Errors
    ///
    /// The port cannot be bound again (another process took it in between).
    pub async fn restart(&mut self) -> std::io::Result<()> {
        self.stop().await;
        let listener = TcpListener::bind(self.address).await?;
        self.serve(listener);
        Ok(())
    }

    fn serve(&mut self, listener: TcpListener) {
        let options = self.options.clone();
        let sessions = Arc::clone(&self.sessions);
        let (stop, mut stopped) = oneshot::channel();
        let listener = Arc::new(listener);
        let accepting = Arc::clone(&listener);
        let task = tokio::spawn(async move {
            let listener = accepting;
            let mut connections = JoinSet::new();
            loop {
                tokio::select! {
                    biased;
                    _ = &mut stopped => break,
                    accepted = listener.accept() => {
                        let Ok((tcp, _)) = accepted else { continue };
                        connections.spawn(connection(tcp, options.clone(), Arc::clone(&sessions)));
                    }
                    Some(_) = connections.join_next(), if !connections.is_empty() => {}
                }
            }
            // Stop listening first (for every copy of the descriptor: see `stop_listening`),
            // then abort every connection and wait until each one is gone, so nothing of this
            // front outlives `stop`.
            stop_listening(&*listener);
            drop(listener);
            connections.shutdown().await;
        });
        self.serving = Some(Serving {
            listener,
            stop,
            task,
        });
    }
}

impl Drop for WsFront {
    fn drop(&mut self) {
        // Nothing can be awaited here: stop listening at once (for every copy of the
        // descriptor), then abort the loop, which drops the set of connections and aborts them.
        if let Some(serving) = self.serving.take() {
            stop_listening(&*serving.listener);
            serving.task.abort();
        }
    }
}

fn record(
    sessions: &Mutex<Vec<FrontSession>>,
    index: usize,
    update: impl FnOnce(&mut FrontSession),
) {
    if let Some(session) = sessions
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get_mut(index)
    {
        update(session);
    }
}

#[allow(
    clippy::result_large_err,
    reason = "tungstenite's handshake callback returns its ErrorResponse by value"
)]
async fn connection(
    tcp: TcpStream,
    options: FrontOptions,
    sessions: Arc<Mutex<Vec<FrontSession>>>,
) {
    let index = {
        let mut sessions = sessions.lock().unwrap_or_else(PoisonError::into_inner);
        sessions.push(FrontSession::default());
        sessions.len() - 1
    };
    let _ = tcp.set_nodelay(true);
    let tls = match TlsAcceptor::from(Arc::clone(&options.tls))
        .accept(tcp)
        .await
    {
        Ok(tls) => tls,
        Err(error) => {
            record(&sessions, index, |session| {
                session.error = Some(format!("TLS: {error}"))
            });
            return;
        }
    };
    {
        let (_, server) = tls.get_ref();
        let negotiated = FrontSession {
            tls_version: server.protocol_version(),
            cipher_suite: server.negotiated_cipher_suite().map(|suite| suite.suite()),
            alpn: server.alpn_protocol().map(<[u8]>::to_vec),
            resumed: matches!(
                server.handshake_kind(),
                Some(rustls::HandshakeKind::Resumed)
            ),
            server_name: server.server_name().map(str::to_owned),
            ..FrontSession::default()
        };
        record(&sessions, index, |session| *session = negotiated);
    }
    let seen = Arc::new(Mutex::new((None::<String>, None::<String>)));
    let callback = {
        let seen = Arc::clone(&seen);
        let answer = options.answer_subprotocol;
        move |request: &Request, mut response: Response| -> Result<Response, ErrorResponse> {
            let path = request.uri().path().to_owned();
            let protocols = request
                .headers()
                .get("Sec-WebSocket-Protocol")
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);
            *seen.lock().unwrap_or_else(PoisonError::into_inner) =
                (Some(path.clone()), protocols.clone());
            if path != "/mqtt" {
                return Err(error_response(StatusCode::NOT_FOUND));
            }
            let asks_mqtt = protocols
                .as_deref()
                .is_some_and(|list| list.split(',').any(|protocol| protocol.trim() == "mqtt"));
            if !asks_mqtt {
                return Err(error_response(StatusCode::BAD_REQUEST));
            }
            if answer {
                response
                    .headers_mut()
                    .insert("Sec-WebSocket-Protocol", HeaderValue::from_static("mqtt"));
            }
            Ok(response)
        }
    };
    let config = WebSocketConfig::default()
        .read_buffer_size(options.read_buffer)
        .write_buffer_size(0)
        .max_message_size(Some(64 * 1024))
        .max_frame_size(Some(64 * 1024));
    let upgraded =
        tokio_tungstenite::accept_hdr_async_with_config(tls, callback, Some(config)).await;
    let (path, subprotocols) = seen.lock().unwrap_or_else(PoisonError::into_inner).clone();
    record(&sessions, index, |session| {
        session.path = path;
        session.subprotocols = subprotocols;
    });
    let mut websocket = match upgraded {
        Ok(websocket) => websocket,
        Err(error) => {
            record(&sessions, index, |session| {
                session.error = Some(format!("WebSocket: {error}"))
            });
            return;
        }
    };
    record(&sessions, index, |session| session.upgraded = true);
    let mut upstream = match TcpStream::connect(options.upstream).await {
        Ok(upstream) => upstream,
        Err(error) => {
            record(&sessions, index, |session| {
                session.error = Some(format!("upstream: {error}"))
            });
            let _ = websocket.close(None).await;
            return;
        }
    };
    let _ = upstream.set_nodelay(true);
    let mut buffer = vec![0_u8; options.read_buffer.max(1)];
    let ping_every = options.ping_interval.unwrap_or(Duration::from_secs(86_400));
    let mut ping = tokio::time::interval_at(tokio::time::Instant::now() + ping_every, ping_every);
    loop {
        tokio::select! {
            message = websocket.next() => match message {
                Some(Ok(Message::Binary(bytes))) => {
                    if upstream.write_all(&bytes).await.is_err() {
                        break;
                    }
                }
                Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => {}
                Some(Ok(Message::Text(_))) => break,
                Some(Ok(Message::Close(_)) | Err(_)) | None => break,
            },
            read = upstream.read(&mut buffer) => match read {
                Ok(0) | Err(_) => {
                    // The broker went away: close the WebSocket, as mqtt-bridge does.
                    let _ = websocket.close(None).await;
                    break;
                }
                Ok(len) => {
                    let copy = tokio_tungstenite::tungstenite::Bytes::copy_from_slice(&buffer[..len]);
                    if websocket.send(Message::Binary(copy)).await.is_err() {
                        break;
                    }
                }
            },
            _ = ping.tick(), if options.ping_interval.is_some() => {
                if websocket.send(Message::Ping(Default::default())).await.is_err() {
                    break;
                }
            }
        }
    }
}

fn error_response(status: StatusCode) -> ErrorResponse {
    let mut response = ErrorResponse::new(None);
    *response.status_mut() = status;
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::bounded;
    use crate::mqtt::{FRONT_HOST, TestPki};
    use rustls_pki_types::pem::PemObject;
    use rustls_pki_types::{CertificateDer, ServerName};
    use tokio_rustls::client::TlsStream;
    use tokio_tungstenite::WebSocketStream;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::tungstenite::{Bytes, Error as WsError};

    type Upgraded = Result<WebSocketStream<TlsStream<TcpStream>>, WsError>;

    /// A WebSocket client of `front`: outer TLS that trusts the test's public CA, then the
    /// upgrade on `path` asking for `protocols` (no `Sec-WebSocket-Protocol` when `None`).
    async fn upgrade(
        pki: &TestPki,
        front: &WsFront,
        path: &str,
        protocols: Option<&str>,
    ) -> Upgraded {
        let mut roots = rustls::RootCertStore::empty();
        roots
            .add(CertificateDer::from_pem_slice(pki.public_ca.pem.as_bytes()).unwrap())
            .unwrap();
        let mut config = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        let tcp = TcpStream::connect(front.address()).await.unwrap();
        let tls = tokio_rustls::TlsConnector::from(Arc::new(config))
            .connect(ServerName::try_from(FRONT_HOST).unwrap(), tcp)
            .await
            .unwrap();
        let mut request = format!("wss://{FRONT_HOST}:{}{path}", front.address().port())
            .into_client_request()
            .unwrap();
        if let Some(protocols) = protocols {
            request.headers_mut().insert(
                "Sec-WebSocket-Protocol",
                HeaderValue::from_str(protocols).unwrap(),
            );
        }
        tokio_tungstenite::client_async(request, tls)
            .await
            .map(|(websocket, _)| websocket)
    }

    fn refused_with(upgraded: &Upgraded) -> Option<StatusCode> {
        match upgraded {
            Err(WsError::Http(response)) => Some(response.status()),
            _ => None,
        }
    }

    #[tokio::test]
    async fn the_front_upgrades_only_mqtt_on_its_path_relays_and_pings() {
        let pki = TestPki::new();
        let upstream = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let mut options = FrontOptions::new(pki.front_tls(), upstream.local_addr().unwrap());
        options.ping_interval = Some(Duration::from_millis(10));
        let front = WsFront::start(options).await.unwrap();

        let wrong_path = upgrade(&pki, &front, "/other", Some("mqtt")).await;
        assert_eq!(
            refused_with(&wrong_path),
            Some(StatusCode::NOT_FOUND),
            "{wrong_path:?}"
        );
        for protocols in [None, Some("mqttv3.1"), Some("wamp")] {
            let refused = upgrade(&pki, &front, "/mqtt", protocols).await;
            assert_eq!(
                refused_with(&refused),
                Some(StatusCode::BAD_REQUEST),
                "{protocols:?}: {refused:?}"
            );
        }
        // `mqtt` among other subprotocols is accepted (and the client checks the answer).
        let mut websocket = upgrade(&pki, &front, "/mqtt", Some("wamp, mqtt"))
            .await
            .unwrap();
        let (mut relayed, _) = bounded("the upstream connection", upstream.accept())
            .await
            .unwrap();
        let message = bounded("a ping", websocket.next()).await.unwrap().unwrap();
        assert!(matches!(message, Message::Ping(_)), "{message:?}");

        websocket
            .send(Message::Binary(Bytes::from_static(b"up")))
            .await
            .unwrap();
        let mut bytes = [0_u8; 2];
        bounded("the relayed bytes", relayed.read_exact(&mut bytes))
            .await
            .unwrap();
        assert_eq!(&bytes, b"up");
        relayed.write_all(b"down").await.unwrap();
        let down = bounded("the bytes from upstream", async {
            loop {
                match websocket.next().await.unwrap().unwrap() {
                    Message::Ping(_) => {}
                    other => return other,
                }
            }
        })
        .await;
        assert_eq!(down, Message::Binary(Bytes::from_static(b"down")));

        let sessions = front.sessions();
        assert_eq!(sessions.len(), 5);
        assert_eq!(
            (sessions[0].path.as_deref(), sessions[0].upgraded),
            (Some("/other"), false)
        );
        let accepted = &sessions[4];
        assert_eq!(
            (
                accepted.path.as_deref(),
                accepted.subprotocols.as_deref(),
                accepted.upgraded,
                accepted.server_name.as_deref(),
                accepted.alpn.as_deref(),
            ),
            (
                Some("/mqtt"),
                Some("wamp, mqtt"),
                true,
                Some(FRONT_HOST),
                Some(&b"http/1.1"[..])
            )
        );
    }

    #[tokio::test]
    async fn a_restart_rebinds_the_same_port_once_the_old_listener_is_gone() {
        let pki = TestPki::new();
        // Nothing listens upstream; the connections below never get that far.
        let upstream = SocketAddr::from(([127, 0, 0, 1], 9));
        let mut front = WsFront::start(FrontOptions::new(pki.front_tls(), upstream))
            .await
            .unwrap();
        let address = front.address();
        for round in 0..3 {
            // A connection in progress, as an agent's would be when the relay crashes.
            let _client = TcpStream::connect(address).await.unwrap();
            front
                .restart()
                .await
                .unwrap_or_else(|error| panic!("restart {round}: {error}"));
            assert_eq!(front.address(), address);
            assert!(front.is_running());
        }
        TcpStream::connect(address).await.unwrap();

        // After stop the port is closed and nothing is left running.
        front.stop().await;
        assert!(!front.is_running());
        let refused = TcpStream::connect(address).await.unwrap_err();
        assert_eq!(refused.kind(), std::io::ErrorKind::ConnectionRefused);
    }

    /// A copy of the listening descriptor, as a child process between fork and exec holds one.
    fn copy_of_the_listener(front: &WsFront) -> std::os::fd::OwnedFd {
        use std::os::fd::AsFd;
        let serving = front.serving.as_ref().expect("the front is running");
        serving.listener.as_fd().try_clone_to_owned().unwrap()
    }

    async fn assert_port_closed(address: SocketAddr, what: &str) {
        let refused = TcpStream::connect(address).await.unwrap_err();
        assert_eq!(
            refused.kind(),
            std::io::ErrorKind::ConnectionRefused,
            "{what}"
        );
        TcpListener::bind(address)
            .await
            .unwrap_or_else(|error| panic!("{what}: rebind: {error}"));
    }

    #[tokio::test]
    async fn stopping_or_dropping_closes_the_port_while_a_copy_of_the_listener_is_open() {
        let pki = TestPki::new();
        let upstream = SocketAddr::from(([127, 0, 0, 1], 9));
        let mut front = WsFront::start(FrontOptions::new(pki.front_tls(), upstream))
            .await
            .unwrap();
        let address = front.address();
        let copy = copy_of_the_listener(&front);
        front.stop().await;
        assert_port_closed(address, "after stop").await;
        drop(copy);

        front.restart().await.unwrap();
        let copy = copy_of_the_listener(&front);
        drop(front);
        assert_port_closed(address, "after drop").await;
        drop(copy);
    }
}
