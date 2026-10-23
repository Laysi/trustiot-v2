//! The gateway's only transport (decision 6): TCP to port 443 of our own domain, outer TLS,
//! a WebSocket upgrade on `wss://<host>/mqtt` with subprotocol `mqtt`, and the inner mTLS to
//! the broker carried in binary WebSocket messages. MQTT runs inside the inner TLS.
//!
//! The WebSocket buffers are bounded ([`websocket_config`]): a message or frame from the relay
//! may not exceed [`MAX_INCOMING_MESSAGE`]; the MQTT relay (mqtt-bridge) sends what it reads
//! from the broker with 8 KiB buffers (deployment.md), well below it. tungstenite answers the
//! relay's pings by itself; the agent's own liveness check is the MQTT keepalive.

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, ready};
use std::time::Duration;

use bytes::{Buf, Bytes};
use futures_util::{Sink, Stream};
use rustls::ClientConfig;
use rustls_pki_types::ServerName;
use socket2::{SockRef, TcpKeepalive};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::{HeaderValue, Uri};
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};

use crate::link::{ConnectError, ConnectStage, Connector};

/// The WebSocket subprotocol of MQTT (MQTT 5 section 6).
pub const SUBPROTOCOL: &str = "mqtt";
/// The only path the gateway connects to (`hosts.gateway` + `/mqtt`, deployment.md).
pub const PATH: &str = "/mqtt";
/// The largest WebSocket message or frame the agent accepts from the relay.
pub const MAX_INCOMING_MESSAGE: usize = 64 * 1024;
/// The largest WebSocket message the agent sends: one inner TLS record (16 KiB of plaintext
/// plus its overhead) fits.
pub const MAX_OUTGOING_MESSAGE: usize = 32 * 1024;
/// The WebSocket read buffer.
const READ_BUFFER: usize = 16 * 1024;
/// How long one TCP connection attempt may take, per resolved address.
const TCP_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// TCP keepalive: the kernel probes a connection that carried nothing for this long ...
pub const TCP_KEEPALIVE_TIME: Duration = Duration::from_secs(60);
/// ... every this often ...
pub const TCP_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(15);
/// ... and drops it after this many unanswered probes (two minutes of silence in all). A
/// backstop under the MQTT keepalive, which normally notices a dead connection first.
pub const TCP_KEEPALIVE_RETRIES: u32 = 4;

/// The bounded WebSocket settings of the agent.
pub fn websocket_config() -> WebSocketConfig {
    WebSocketConfig::default()
        .read_buffer_size(READ_BUFFER)
        // Every message is written at once; latency matters more than batching here.
        .write_buffer_size(0)
        .max_write_buffer_size(MAX_OUTGOING_MESSAGE + 1024)
        .max_message_size(Some(MAX_INCOMING_MESSAGE))
        .max_frame_size(Some(MAX_INCOMING_MESSAGE))
}

/// The validated `wss` URL of the cloud's gateway endpoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CloudUrl {
    url: String,
    host: String,
    port: u16,
}

impl CloudUrl {
    /// Checks `url`: scheme `wss`, a DNS name (our own domain, never an IP address), no user
    /// information, path exactly [`PATH`], no query or fragment. The port defaults to 443.
    ///
    /// # Errors
    ///
    /// A message naming the rule that `url` breaks.
    pub fn parse(url: &str) -> Result<Self, String> {
        if url.contains('#') {
            return Err("must not have a fragment".to_owned());
        }
        let uri: Uri = url
            .parse()
            .map_err(|error| format!("is not a URL: {error}"))?;
        if !uri
            .scheme_str()
            .is_some_and(|scheme| scheme.eq_ignore_ascii_case("wss"))
        {
            return Err(
                "must use the wss scheme (the gateway connects only over TLS on 443)".to_owned(),
            );
        }
        let authority = uri.authority().ok_or("has no host")?;
        if authority.as_str().contains('@') {
            return Err("must not contain user information".to_owned());
        }
        let host = authority.host().to_ascii_lowercase();
        // An IPv6 literal keeps its brackets in the authority.
        let bare = host
            .strip_prefix('[')
            .and_then(|rest| rest.strip_suffix(']'))
            .unwrap_or(&host);
        let not_an_address = "must name a DNS host (our own domain), not an IP address";
        if ends_in_a_number(bare) {
            return Err(format!(
                "{not_an_address} (a host whose last label is a number reads as one)"
            ));
        }
        match ServerName::try_from(bare) {
            Ok(ServerName::DnsName(_)) => {}
            Ok(_) => return Err(not_an_address.to_owned()),
            Err(_) => return Err(format!("host {host:?} is not a valid DNS name")),
        }
        let port = explicit_port(authority)?.unwrap_or(443);
        if uri.path() != PATH || uri.query().is_some() {
            return Err(format!("must have the path {PATH} and no query"));
        }
        let url = if port == 443 {
            format!("wss://{host}{PATH}")
        } else {
            format!("wss://{host}:{port}{PATH}")
        };
        Ok(Self { url, host, port })
    }

    /// The normalized URL.
    pub fn as_str(&self) -> &str {
        &self.url
    }

    /// The host: the outer TLS server name (SNI) and the HTTP `Host`.
    pub fn host(&self) -> &str {
        &self.host
    }

    /// The TCP port.
    pub const fn port(&self) -> u16 {
        self.port
    }
}

/// The port written in `authority`, if any. Only a missing port means the default: an empty,
/// non-numeric, signed or out-of-range one (`:`, `:abc`, `:+443`, `:0`, `:65536`) is an
/// error, never a silent 443.
fn explicit_port(
    authority: &tokio_tungstenite::tungstenite::http::uri::Authority,
) -> Result<Option<u16>, String> {
    let Some(rest) = authority.as_str().strip_prefix(authority.host()) else {
        return Err("has no host".to_owned());
    };
    let Some(text) = rest.strip_prefix(':') else {
        return if rest.is_empty() {
            Ok(None)
        } else {
            Err(format!("has an unexpected {rest:?} after the host"))
        };
    };
    match text.parse::<u16>() {
        Ok(port) if port != 0 && text.bytes().all(|byte| byte.is_ascii_digit()) => Ok(Some(port)),
        _ => Err(format!("port {text:?} is not a TCP port (1-65535)")),
    }
}

/// Whether the resolver would read `host` as an IPv4 address rather than look it up: its last
/// label is a number, decimal, octal or `0x` hex (`127.1`, `0x7f000001`, `192.0.2.1.`).
/// inet_aton, and so getaddrinfo, accepts these forms; the WHATWG URL parser uses the same
/// rule ("ends in a number"). rustls takes them for DNS names, so they are refused here.
fn ends_in_a_number(host: &str) -> bool {
    let host = host.strip_suffix('.').unwrap_or(host);
    let last = host.rsplit('.').next().unwrap_or(host);
    let all = |text: &str, radix: u32| text.chars().all(|c| c.is_digit(radix));
    (!last.is_empty() && all(last, 10))
        || last
            .strip_prefix("0x")
            .or_else(|| last.strip_prefix("0X"))
            .is_some_and(|hex| all(hex, 16))
}

/// Sets the agent's options on a connected TCP socket: no Nagle delay (MQTT packets are small;
/// Nagle would delay each by up to 40 ms, TPM PoC) and kernel keepalive probes
/// ([`TCP_KEEPALIVE_TIME`]), so a connection whose peer vanished without a word (a NAT entry
/// dropped, a cable pulled) ends even while MQTT has nothing to say.
///
/// # Errors
///
/// The error of the socket option that could not be set.
pub fn configure_tcp(stream: &TcpStream) -> io::Result<()> {
    stream.set_nodelay(true)?;
    let keepalive = TcpKeepalive::new()
        .with_time(TCP_KEEPALIVE_TIME)
        .with_interval(TCP_KEEPALIVE_INTERVAL)
        .with_retries(TCP_KEEPALIVE_RETRIES);
    SockRef::from(stream).set_tcp_keepalive(&keepalive)
}

/// Opens the gateway's connection to the broker. Reuses its two TLS configurations across
/// connections, which keeps their resumption caches.
#[derive(Clone)]
pub struct WssConnector {
    url: CloudUrl,
    outer: Arc<ClientConfig>,
    inner: Arc<ClientConfig>,
    broker: ServerName<'static>,
}

impl std::fmt::Debug for WssConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WssConnector")
            .field("url", &self.url.as_str())
            .field("broker", &self.broker)
            .finish_non_exhaustive()
    }
}

/// The stream MQTT runs over: inner TLS, in a WebSocket, in outer TLS, over TCP.
pub type BrokerStream = TlsStream<WsStream<TlsStream<TcpStream>>>;

impl WssConnector {
    /// A connector to `url` whose inner TLS expects the broker certificate for `broker`.
    pub fn new(
        url: CloudUrl,
        outer: Arc<ClientConfig>,
        inner: Arc<ClientConfig>,
        broker: ServerName<'static>,
    ) -> Self {
        Self {
            url,
            outer,
            inner,
            broker,
        }
    }

    async fn tcp(&self) -> Result<TcpStream, ConnectError> {
        let addresses: Vec<SocketAddr> =
            tokio::net::lookup_host((self.url.host(), self.url.port()))
                .await
                .map_err(|error| ConnectError::new(ConnectStage::Resolve, error))?
                .collect();
        let stream = first_connection(addresses, TCP_CONNECT_TIMEOUT, TcpStream::connect)
            .await
            .map_err(|error| ConnectError::new(ConnectStage::Tcp, error))?;
        configure_tcp(&stream).map_err(|error| ConnectError::new(ConnectStage::Tcp, error))?;
        Ok(stream)
    }
}

/// Tries `addresses` in order, each for at most `limit`, and returns the first connection; when
/// none connects, the error of the last one.
async fn first_connection<A, S, F, Fut>(
    addresses: impl IntoIterator<Item = A>,
    limit: Duration,
    connect: F,
) -> io::Result<S>
where
    A: std::fmt::Display + Copy,
    F: Fn(A) -> Fut,
    Fut: Future<Output = io::Result<S>>,
{
    let mut last_error = io::Error::new(io::ErrorKind::NotFound, "the host has no address");
    for address in addresses {
        match tokio::time::timeout(limit, connect(address)).await {
            Ok(Ok(stream)) => return Ok(stream),
            Ok(Err(error)) => last_error = error,
            Err(_) => {
                last_error = io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("connecting to {address} timed out"),
                );
            }
        }
    }
    Err(last_error)
}

impl Connector for WssConnector {
    type Stream = BrokerStream;

    async fn connect(&self) -> Result<BrokerStream, ConnectError> {
        let tcp = self.tcp().await?;
        let outer_name = ServerName::try_from(self.url.host().to_owned())
            .map_err(|error| ConnectError::new(ConnectStage::OuterTls, error))?;
        let outer = TlsConnector::from(Arc::clone(&self.outer))
            .connect(outer_name, tcp)
            .await
            .map_err(|error| ConnectError::new(ConnectStage::OuterTls, error))?;
        let mut request = self
            .url
            .as_str()
            .into_client_request()
            .map_err(|error| ConnectError::new(ConnectStage::WebSocket, error))?;
        request.headers_mut().insert(
            "Sec-WebSocket-Protocol",
            HeaderValue::from_static(SUBPROTOCOL),
        );
        // tungstenite refuses a response without our subprotocol or with another one.
        let (websocket, _response) =
            tokio_tungstenite::client_async_with_config(request, outer, Some(websocket_config()))
                .await
                .map_err(|error| {
                    ConnectError::new(ConnectStage::WebSocket, describe_ws_error(error))
                })?;
        let inner = TlsConnector::from(Arc::clone(&self.inner))
            .connect(self.broker.clone(), WsStream::new(websocket))
            .await
            .map_err(|error| ConnectError::new(ConnectStage::InnerTls, error))?;
        if let Some(suite) = inner.get_ref().1.negotiated_cipher_suite() {
            tracing::debug!(cipher_suite = ?suite.suite(), "inner TLS established");
        }
        Ok(inner)
    }
}

/// A WebSocket error with the HTTP status of a refused upgrade, which says the most (403
/// from the ingress, 502 from a relay without a broker, ...).
fn describe_ws_error(error: WsError) -> io::Error {
    match error {
        WsError::Http(response) => io::Error::other(format!(
            "the upgrade was answered with HTTP {}",
            response.status()
        )),
        WsError::Io(error) => error,
        other => io::Error::other(other),
    }
}

/// A WebSocket as a byte stream: every write is one binary message, reads return the bytes of
/// binary messages in order. Text messages break the protocol and fail the read; a close ends
/// the stream.
pub struct WsStream<S> {
    websocket: WebSocketStream<S>,
    unread: Bytes,
}

impl<S> WsStream<S> {
    /// Wraps an upgraded WebSocket.
    pub fn new(websocket: WebSocketStream<S>) -> Self {
        Self {
            websocket,
            unread: Bytes::new(),
        }
    }
}

impl<S> std::fmt::Debug for WsStream<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WsStream")
            .field("unread", &self.unread.len())
            .finish_non_exhaustive()
    }
}

fn io_error(error: WsError) -> io::Error {
    match error {
        WsError::Io(error) => error,
        WsError::ConnectionClosed | WsError::AlreadyClosed => {
            io::Error::new(io::ErrorKind::BrokenPipe, "the WebSocket is closed")
        }
        other => io::Error::other(other),
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncRead for WsStream<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        loop {
            if !this.unread.is_empty() {
                let len = this.unread.len().min(buf.remaining());
                buf.put_slice(&this.unread[..len]);
                this.unread.advance(len);
                return Poll::Ready(Ok(()));
            }
            match ready!(Pin::new(&mut this.websocket).poll_next(cx)) {
                Some(Ok(Message::Binary(data))) => this.unread = data,
                // Pings are answered by tungstenite on the next read or write.
                Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => {}
                Some(Ok(Message::Close(_))) | None => return Poll::Ready(Ok(())),
                Some(Ok(Message::Text(_))) => {
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "a text message on the MQTT WebSocket",
                    )));
                }
                Some(Err(WsError::ConnectionClosed | WsError::AlreadyClosed)) => {
                    return Poll::Ready(Ok(()));
                }
                Some(Err(error)) => return Poll::Ready(Err(io_error(error))),
            }
        }
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncWrite for WsStream<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        let this = self.get_mut();
        ready!(Pin::new(&mut this.websocket).poll_ready(cx)).map_err(io_error)?;
        let len = buf.len().min(MAX_OUTGOING_MESSAGE);
        Pin::new(&mut this.websocket)
            .start_send(Message::Binary(Bytes::copy_from_slice(&buf[..len])))
            .map_err(io_error)?;
        Poll::Ready(Ok(len))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().websocket)
            .poll_flush(cx)
            .map_err(io_error)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().websocket)
            .poll_close(cx)
            .map_err(io_error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use futures_util::{SinkExt, StreamExt};
    use testkit::clock::bounded;
    use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
    use tokio::time::Instant;
    use tokio_tungstenite::tungstenite::protocol::Role;

    /// A stand-in for one address: 1 never answers (a dropped SYN), 2 refuses, others connect.
    async fn connect_to(address: u16) -> io::Result<u16> {
        match address {
            1 => std::future::pending().await,
            2 => Err(io::Error::from(io::ErrorKind::ConnectionRefused)),
            _ => Ok(address),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn each_resolved_address_gets_its_own_ten_seconds() {
        assert_eq!(TCP_CONNECT_TIMEOUT, Duration::from_secs(10));
        let started = Instant::now();
        let connected = bounded(
            "the third address",
            first_connection([1, 2, 3], TCP_CONNECT_TIMEOUT, connect_to),
        )
        .await;
        assert_eq!(connected.unwrap(), 3);
        assert_eq!(started.elapsed(), TCP_CONNECT_TIMEOUT);

        // None connects: the last address's error, after each address had its time.
        let started = Instant::now();
        let error = bounded(
            "two silent addresses",
            first_connection([1, 1], TCP_CONNECT_TIMEOUT, connect_to),
        )
        .await
        .unwrap_err();
        assert_eq!(started.elapsed(), 2 * TCP_CONNECT_TIMEOUT);
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(error.to_string(), "connecting to 1 timed out");
        let error = first_connection([1, 2], TCP_CONNECT_TIMEOUT, connect_to)
            .await
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::ConnectionRefused);
        let error = first_connection(Vec::<u16>::new(), TCP_CONNECT_TIMEOUT, connect_to)
            .await
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }

    /// The agent's side of an in-memory WebSocket, and the relay's side.
    async fn websocket_pair() -> (WsStream<DuplexStream>, WebSocketStream<DuplexStream>) {
        let (agent, relay) = tokio::io::duplex(256 * 1024);
        let agent =
            WebSocketStream::from_raw_socket(agent, Role::Client, Some(websocket_config())).await;
        let relay = WebSocketStream::from_raw_socket(relay, Role::Server, None).await;
        (WsStream::new(agent), relay)
    }

    #[tokio::test]
    async fn bytes_travel_in_binary_messages_of_at_most_32_kib_and_pings_are_answered() {
        let (mut agent, mut relay) = websocket_pair().await;
        let sent: Vec<u8> = (0..100_000_u32)
            .map(|index| u8::try_from(index % 251).unwrap())
            .collect();
        let expected = sent.len();
        let writer = async {
            agent.write_all(&sent).await.unwrap();
            agent.flush().await.unwrap();
        };
        let reader = async {
            let mut received = Vec::new();
            while received.len() < expected {
                match relay.next().await.unwrap().unwrap() {
                    Message::Binary(data) => {
                        assert!(data.len() <= MAX_OUTGOING_MESSAGE, "{} bytes", data.len());
                        received.extend_from_slice(&data);
                    }
                    other => panic!("expected a binary message, got {other:?}"),
                }
            }
            received
        };
        let ((), received) = bounded("the messages", async { tokio::join!(writer, reader) }).await;
        assert_eq!(received, sent);

        // The relay's binary messages come out as bytes, in order; a ping between them is
        // answered.
        relay
            .send(Message::Binary(Bytes::from_static(b"abc")))
            .await
            .unwrap();
        relay
            .send(Message::Ping(Bytes::from_static(b"p")))
            .await
            .unwrap();
        relay
            .send(Message::Binary(Bytes::from_static(b"def")))
            .await
            .unwrap();
        let mut bytes = [0_u8; 6];
        bounded("the bytes", agent.read_exact(&mut bytes))
            .await
            .unwrap();
        assert_eq!(&bytes, b"abcdef");
        agent.write_all(b"g").await.unwrap();
        assert_eq!(
            bounded("the pong", relay.next()).await.unwrap().unwrap(),
            Message::Pong(Bytes::from_static(b"p"))
        );
    }

    #[tokio::test]
    async fn a_text_message_is_an_error_and_a_close_ends_the_stream() {
        let (mut agent, mut relay) = websocket_pair().await;
        relay.send(Message::Text("x".into())).await.unwrap();
        let error = bounded("the read", agent.read(&mut [0; 8]))
            .await
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("text message"), "{error}");

        let (mut agent, mut relay) = websocket_pair().await;
        relay
            .send(Message::Binary(Bytes::from_static(b"last")))
            .await
            .unwrap();
        relay.close(None).await.unwrap();
        let mut rest = Vec::new();
        bounded("the end of the stream", agent.read_to_end(&mut rest))
            .await
            .unwrap();
        assert_eq!(rest, b"last");
    }

    #[test]
    fn only_wss_urls_on_our_own_domain_with_the_mqtt_path_are_accepted() {
        let url = CloudUrl::parse("wss://gw.trustiot.example/mqtt").unwrap();
        assert_eq!(
            (url.host(), url.port(), url.as_str()),
            ("gw.trustiot.example", 443, "wss://gw.trustiot.example/mqtt")
        );
        let explicit = CloudUrl::parse("WSS://GW.Trustiot.Example:443/mqtt").unwrap();
        assert_eq!(explicit.as_str(), "wss://gw.trustiot.example/mqtt");
        assert_eq!(
            CloudUrl::parse("wss://gw.trustiot.example:65535/mqtt")
                .unwrap()
                .port(),
            65535
        );
        let test_port = CloudUrl::parse("wss://localhost:8443/mqtt").unwrap();
        assert_eq!(
            (test_port.port(), test_port.as_str()),
            (8443, "wss://localhost:8443/mqtt")
        );
        // Digits are fine anywhere but in the last label.
        for host in [
            "1gw.trustiot.example",
            "gw-0x1.trustiot.example",
            "gw.trustiot.example.",
        ] {
            assert!(
                CloudUrl::parse(&format!("wss://{host}/mqtt")).is_ok(),
                "{host}"
            );
        }

        let refused = [
            ("ws://gw.trustiot.example/mqtt", "wss scheme"),
            ("https://gw.trustiot.example/mqtt", "wss scheme"),
            ("mqtts://gw.trustiot.example:8883", "wss scheme"),
            ("wss://203.0.113.7/mqtt", "not an IP address"),
            ("wss://[2001:db8::1]/mqtt", "not an IP address"),
            // Forms that inet_aton (and so getaddrinfo) reads as IPv4 addresses.
            ("wss://0x7f000001/mqtt", "not an IP address"),
            ("wss://0x7f.1/mqtt", "not an IP address"),
            ("wss://0X7F.0.0.1/mqtt", "not an IP address"),
            ("wss://127.1/mqtt", "not an IP address"),
            ("wss://2130706433/mqtt", "not an IP address"),
            ("wss://017700000001/mqtt", "not an IP address"),
            ("wss://192.0.2.1./mqtt", "not an IP address"),
            ("wss://gw.trustiot.123/mqtt", "not an IP address"),
            ("wss://gw.trustiot.0x1f/mqtt", "not an IP address"),
            ("wss://user:pw@gw.trustiot.example/mqtt", "user information"),
            ("wss://gw.trustiot.example/", "path /mqtt"),
            ("wss://gw.trustiot.example/mqtt/x", "path /mqtt"),
            ("wss://gw.trustiot.example/mqtt?x=1", "no query"),
            ("wss://gw.trustiot.example/mqtt#x", "fragment"),
            // An explicit port must be 1-65535; only a missing one means 443.
            ("wss://gw.trustiot.example:0/mqtt", "not a TCP port"),
            ("wss://gw.trustiot.example:65536/mqtt", "not a TCP port"),
            ("wss://gw.trustiot.example:99999/mqtt", "not a TCP port"),
            ("wss://gw.trustiot.example:abc/mqtt", "port"),
            ("wss://gw.trustiot.example:/mqtt", "not a TCP port"),
            ("wss://gw.trustiot.example:+443/mqtt", "not a TCP port"),
            ("wss://gw.trustiot.example:-1/mqtt", "port"),
            ("wss:///mqtt", ""),
            ("gw.trustiot.example", "wss scheme"),
            ("", ""),
        ];
        for (url, expected) in refused {
            let error = CloudUrl::parse(url).expect_err(url);
            assert!(error.contains(expected), "{url}: {error}");
        }
    }

    #[tokio::test]
    async fn connections_use_nodelay_and_tcp_keepalive() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let stream = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        configure_tcp(&stream).unwrap();
        let socket = SockRef::from(&stream);
        assert!(stream.nodelay().unwrap());
        assert!(socket.keepalive().unwrap());
        assert_eq!(
            (
                socket.tcp_keepalive_time().unwrap(),
                socket.tcp_keepalive_interval().unwrap(),
                socket.tcp_keepalive_retries().unwrap(),
            ),
            (
                TCP_KEEPALIVE_TIME,
                TCP_KEEPALIVE_INTERVAL,
                TCP_KEEPALIVE_RETRIES
            )
        );
    }

    #[test]
    fn websocket_buffers_are_bounded() {
        let config = websocket_config();
        assert_eq!(config.max_message_size, Some(MAX_INCOMING_MESSAGE));
        assert_eq!(config.max_frame_size, Some(MAX_INCOMING_MESSAGE));
        assert_eq!(config.read_buffer_size, READ_BUFFER);
        assert!(config.max_write_buffer_size > MAX_OUTGOING_MESSAGE);
        assert!(config.max_write_buffer_size < 64 * 1024);
    }
}
