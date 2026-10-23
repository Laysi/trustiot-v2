//! Test support: the real `mqtt-bridge` binary, stand-ins for the MQTT broker (bytes, MQTT, TLS),
//! a proxy that cuts idle connections like Cloudflare, a gateway's WebSocket client, and a fake
//! OTLP collector.
//!
//! Time limits follow CLAUDE.md ("Test time limits"): waits are for a condition, bounded by
//! [`LIMIT`], and fail with a message naming what was awaited.

#![allow(
    dead_code,
    reason = "each test file uses a different part of the support module"
)]

use std::collections::HashMap;
use std::io::{BufRead as _, BufReader as StdBufReader, Read as _, Write as _};
use std::net::{SocketAddr, TcpListener as StdTcpListener, TcpStream as StdTcpStream};
use std::pin::Pin;
use std::process::Stdio;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll, ready};
use std::time::Duration;

use futures_util::{Sink, SinkExt as _, Stream, StreamExt as _};
use tokio::io::{
    AsyncBufReadExt as _, AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _, BufReader,
    ReadBuf,
};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::{Child, Command};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tokio_tungstenite::tungstenite::http::{HeaderName, HeaderValue, StatusCode};
use tokio_tungstenite::tungstenite::{Bytes, Error as WsError, Message};

pub const BINARY: &str = env!("CARGO_BIN_EXE_mqtt-bridge");

/// The bound of every wait on a loaded machine (CLAUDE.md): reaching it means the awaited thing
/// never happened, not that it was slow.
pub const LIMIT: Duration = Duration::from_secs(120);

/// The trusted source of the SaaS deployment.
pub const CF: &str = "cf-connecting-ip";

/// Waits until `check` returns `Some`, polling; panics naming `what` after [`LIMIT`].
pub async fn wait_until<T>(what: &str, mut check: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + LIMIT;
    loop {
        if let Some(value) = check() {
            return value;
        }
        assert!(Instant::now() < deadline, "waited {LIMIT:?} for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Runs `future`, panicking naming `what` after [`LIMIT`].
pub async fn within<T>(what: &str, future: impl Future<Output = T>) -> T {
    tokio::time::timeout(LIMIT, future)
        .await
        .unwrap_or_else(|_| panic!("waited {LIMIT:?} for {what}"))
}

/// The bridge binary, its JSON log lines collected as they come.
pub struct Bridge {
    child: Child,
    lines: Arc<Mutex<Vec<serde_json::Value>>>,
    reader: JoinHandle<()>,
    pub address: SocketAddr,
}

impl Bridge {
    /// Starts the bridge in front of `broker` with the SaaS source and `settings` on top, and
    /// waits until it listens.
    pub async fn start(broker: SocketAddr, settings: &[(&str, &str)]) -> Self {
        let broker = broker.to_string();
        let mut vars = vec![
            ("TRUSTIOT_CLIENT_IP_SOURCE", CF),
            ("TRUSTIOT__BROKER__ADDRESS", broker.as_str()),
        ];
        vars.extend_from_slice(settings);
        let mut bridge = Self::spawn(&vars);
        let listening = bridge.wait_for_line("listening").await;
        bridge.address = listening["fields"]["address"]
            .as_str()
            .expect("the API address")
            .parse()
            .expect("a socket address");
        bridge
    }

    /// Starts the binary with exactly `vars` (and free local ports) in its environment.
    pub fn spawn(vars: &[(&str, &str)]) -> Self {
        let mut command = Command::new(BINARY);
        command
            .env_clear()
            .env("TRUSTIOT__HTTP__LISTEN_ADDR", "127.0.0.1:0")
            .env("TRUSTIOT__HTTP__HEALTH_LISTEN_ADDR", "127.0.0.1:0");
        for (name, value) in vars {
            command.env(name, value);
        }
        let mut child = command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .expect("start the mqtt-bridge binary");
        let stdout = child.stdout.take().expect("piped stdout");
        let lines = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&lines);
        let reader = tokio::spawn(async move {
            let mut stdout = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = stdout.next_line().await {
                let value: serde_json::Value = serde_json::from_str(&line)
                    .unwrap_or_else(|_| panic!("not a JSON log line: {line}"));
                sink.lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(value);
            }
        });
        Self {
            child,
            lines,
            reader,
            address: SocketAddr::from(([127, 0, 0, 1], 0)),
        }
    }

    /// Every log line so far.
    pub fn lines(&self) -> Vec<serde_json::Value> {
        self.lines
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The log lines with `message`.
    pub fn messages(&self, message: &str) -> Vec<serde_json::Value> {
        self.lines()
            .into_iter()
            .filter(|line| line["message"] == message)
            .collect()
    }

    /// Waits for the first log line with `message`.
    pub async fn wait_for_line(&mut self, message: &str) -> serde_json::Value {
        self.wait_for_lines(message, 1).await.remove(0)
    }

    /// Waits until `count` log lines have `message`; returns them.
    pub async fn wait_for_lines(&mut self, message: &str, count: usize) -> Vec<serde_json::Value> {
        let deadline = Instant::now() + LIMIT;
        loop {
            let found = self.messages(message);
            if found.len() >= count {
                return found;
            }
            if let Ok(Some(status)) = self.child.try_wait() {
                // Let the reader take the last lines before reporting them.
                let _ = tokio::time::timeout(LIMIT, &mut self.reader).await;
                let found = self.messages(message);
                if found.len() >= count {
                    return found;
                }
                panic!(
                    "the bridge exited ({status}) before logging `{message}` {count} times:\n{}",
                    self.dump()
                );
            }
            assert!(
                Instant::now() < deadline,
                "waited {LIMIT:?} for `{message}` {count} times:\n{}",
                self.dump()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// The log lines, one per line, for failure messages.
    pub fn dump(&self) -> String {
        self.lines()
            .iter()
            .map(serde_json::Value::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Sends SIGTERM, as the kubelet does.
    pub fn terminate(&self) {
        let pid = self.child.id().expect("the bridge is running");
        let status = std::process::Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .status()
            .expect("run kill");
        assert!(status.success(), "kill -TERM {pid}");
    }

    /// Whether the process has exited.
    pub fn has_exited(&mut self) -> bool {
        self.child.try_wait().expect("check the bridge").is_some()
    }

    /// Waits for the process to exit and for its last log lines; returns its exit code.
    pub async fn wait_exit(&mut self) -> i32 {
        let status = within("the bridge to exit", self.child.wait())
            .await
            .expect("wait for the bridge");
        let _ = within("the bridge's last log lines", &mut self.reader).await;
        status.code().expect("an exit code, not a signal")
    }

    /// `ws://` URL of the bridge.
    pub fn url(&self) -> String {
        format!("ws://{}/mqtt", self.address)
    }
}

/// What a broker stand-in does with each connection.
#[derive(Clone)]
pub enum Behaviour {
    /// Records what it receives and sends it back.
    Echo,
    /// Answers MQTT 5 CONNECT with CONNACK and PINGREQ with PINGRESP; records what it receives.
    Mqtt,
    /// A TLS server that requires a client certificate, then echoes the plaintext it receives.
    Tls(Arc<rustls::ServerConfig>),
}

/// One connection the broker stand-in accepted.
#[derive(Clone, Debug, Default)]
pub struct BrokerConnection {
    /// The bridge's address as the broker saw it.
    pub peer: Option<SocketAddr>,
    /// Every byte received, in order (the plaintext for [`Behaviour::Tls`]).
    pub received: Vec<u8>,
    /// Whether the bridge closed the connection (read returned 0 or failed).
    pub closed: bool,
}

/// A broker stand-in on a free local port.
pub struct FakeBroker {
    pub address: SocketAddr,
    connections: Arc<Mutex<Vec<BrokerConnection>>>,
    task: JoinHandle<()>,
}

impl FakeBroker {
    pub async fn start(behaviour: Behaviour) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the broker");
        let address = listener.local_addr().expect("broker address");
        let connections = Arc::new(Mutex::new(Vec::new()));
        let record = Arc::clone(&connections);
        let task = tokio::spawn(async move {
            while let Ok((stream, peer)) = listener.accept().await {
                let index = {
                    let mut all = record.lock().unwrap_or_else(PoisonError::into_inner);
                    all.push(BrokerConnection {
                        peer: Some(peer),
                        ..BrokerConnection::default()
                    });
                    all.len() - 1
                };
                let record = Arc::clone(&record);
                let behaviour = behaviour.clone();
                tokio::spawn(async move {
                    serve_broker(stream, behaviour, &record, index).await;
                    record.lock().unwrap_or_else(PoisonError::into_inner)[index].closed = true;
                });
            }
        });
        Self {
            address,
            connections,
            task,
        }
    }

    pub fn connections(&self) -> Vec<BrokerConnection> {
        self.connections
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl Drop for FakeBroker {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn serve_broker(
    stream: TcpStream,
    behaviour: Behaviour,
    record: &Mutex<Vec<BrokerConnection>>,
    index: usize,
) {
    let received = |bytes: &[u8]| {
        record.lock().unwrap_or_else(PoisonError::into_inner)[index]
            .received
            .extend_from_slice(bytes);
    };
    match behaviour {
        Behaviour::Echo => echo(stream, received).await,
        Behaviour::Tls(config) => {
            let Ok(tls) = tokio_rustls::TlsAcceptor::from(config).accept(stream).await else {
                return;
            };
            echo(tls, received).await;
        }
        Behaviour::Mqtt => {
            let mut stream = stream;
            loop {
                let Some((packet, raw)) = read_mqtt_packet(&mut stream).await else {
                    return;
                };
                received(&raw);
                let answer: &[u8] = match packet >> 4 {
                    // CONNECT: CONNACK, MQTT 5, success, no properties.
                    1 => &[0x20, 0x03, 0x00, 0x00, 0x00],
                    // PINGREQ: PINGRESP.
                    12 => &[0xd0, 0x00],
                    // DISCONNECT.
                    14 => return,
                    _ => &[],
                };
                if stream.write_all(answer).await.is_err() {
                    return;
                }
            }
        }
    }
}

async fn echo<S: AsyncRead + AsyncWrite + Unpin>(mut stream: S, received: impl Fn(&[u8])) {
    let mut buffer = vec![0; 64 * 1024];
    loop {
        match stream.read(&mut buffer).await {
            Ok(0) | Err(_) => return,
            Ok(length) => {
                received(&buffer[..length]);
                if stream.write_all(&buffer[..length]).await.is_err() {
                    return;
                }
            }
        }
    }
}

/// Reads one MQTT packet: its first byte and every byte of it.
async fn read_mqtt_packet<S: AsyncRead + Unpin>(stream: &mut S) -> Option<(u8, Vec<u8>)> {
    let first = stream.read_u8().await.ok()?;
    let mut raw = vec![first];
    let mut length = 0usize;
    for shift in [0, 7, 14, 21] {
        let byte = stream.read_u8().await.ok()?;
        raw.push(byte);
        length |= usize::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            break;
        }
    }
    let mut rest = vec![0; length];
    stream.read_exact(&mut rest).await.ok()?;
    raw.extend_from_slice(&rest);
    Some((first, raw))
}

/// MQTT 5 CONNECT with keepalive 0 (no MQTT keepalive at all), clean start, client id `gw-test`.
pub const CONNECT_KEEPALIVE_0: [u8; 22] = [
    0x10, 0x14, // CONNECT, remaining length 20
    0x00, 0x04, b'M', b'Q', b'T', b'T', // protocol name
    0x05, // version 5
    0x02, // clean start
    0x00, 0x00, // keepalive 0
    0x00, // no properties
    0x00, 0x07, b'g', b'w', b'-', b't', b'e', b's', b't', // client id
];
pub const CONNACK: [u8; 5] = [0x20, 0x03, 0x00, 0x00, 0x00];
pub const PINGREQ: [u8; 2] = [0xc0, 0x00];
pub const PINGRESP: [u8; 2] = [0xd0, 0x00];

/// A TCP proxy that cuts a connection after it carried nothing, in either direction, for
/// `cutoff`: Cloudflare does this after 125 s (docs/feasibility/2026-10-10-edge-poc.md).
pub struct IdleCutProxy {
    pub address: SocketAddr,
    cuts: Arc<Mutex<u32>>,
    task: JoinHandle<()>,
}

impl IdleCutProxy {
    pub async fn start(upstream: SocketAddr, cutoff: Duration) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the proxy");
        let address = listener.local_addr().expect("proxy address");
        let cuts = Arc::new(Mutex::new(0));
        let counted = Arc::clone(&cuts);
        let task = tokio::spawn(async move {
            while let Ok((client, _)) = listener.accept().await {
                let counted = Arc::clone(&counted);
                tokio::spawn(async move {
                    let Ok(server) = TcpStream::connect(upstream).await else {
                        return;
                    };
                    if proxy_until_idle(client, server, cutoff).await {
                        *counted.lock().unwrap_or_else(PoisonError::into_inner) += 1;
                    }
                });
            }
        });
        Self {
            address,
            cuts,
            task,
        }
    }

    /// How many connections it cut for being idle.
    pub fn cuts(&self) -> u32 {
        *self.cuts.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Drop for IdleCutProxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Copies both ways until one side ends (false) or nothing moved for `cutoff` (true, and both
/// sides are dropped without a close, as Cloudflare does).
async fn proxy_until_idle(client: TcpStream, server: TcpStream, cutoff: Duration) -> bool {
    let (mut client_read, mut client_write) = client.into_split();
    let (mut server_read, mut server_write) = server.into_split();
    let mut up = vec![0; 16 * 1024];
    let mut down = vec![0; 16 * 1024];
    let mut last = Instant::now();
    loop {
        tokio::select! {
            read = client_read.read(&mut up) => match read {
                Ok(0) | Err(_) => return false,
                Ok(length) => {
                    if server_write.write_all(&up[..length]).await.is_err() {
                        return false;
                    }
                    last = Instant::now();
                }
            },
            read = server_read.read(&mut down) => match read {
                Ok(0) | Err(_) => return false,
                Ok(length) => {
                    if client_write.write_all(&down[..length]).await.is_err() {
                        return false;
                    }
                    last = Instant::now();
                }
            },
            () = tokio::time::sleep_until(last + cutoff) => return true,
        }
    }
}

/// The headers a request through Cloudflare carries, plus what a client may add itself.
pub type Headers<'a> = &'a [(&'a str, &'a str)];

/// Opens a gateway's WebSocket to `url` through a TCP connection to `address`, asking for the
/// `mqtt` subprotocol, with `headers`.
pub async fn connect_ws(
    address: SocketAddr,
    url: &str,
    headers: Headers<'_>,
) -> Result<WebSocketStream<TcpStream>, WsError> {
    connect_ws_with(address, url, headers, Some("mqtt")).await
}

/// [`connect_ws`] with a chosen subprotocol (or none).
pub async fn connect_ws_with(
    address: SocketAddr,
    url: &str,
    headers: Headers<'_>,
    subprotocol: Option<&str>,
) -> Result<WebSocketStream<TcpStream>, WsError> {
    let mut request = url.into_client_request()?;
    for (name, value) in headers {
        request.headers_mut().append(
            HeaderName::from_bytes(name.as_bytes()).expect("header name"),
            HeaderValue::from_str(value).expect("header value"),
        );
    }
    if let Some(subprotocol) = subprotocol {
        request.headers_mut().insert(
            "sec-websocket-protocol",
            HeaderValue::from_str(subprotocol).expect("subprotocol"),
        );
    }
    let tcp = TcpStream::connect(address).await.map_err(WsError::Io)?;
    let (websocket, response) = within(
        "the WebSocket handshake",
        tokio_tungstenite::client_async(request, tcp),
    )
    .await?;
    assert_eq!(
        response
            .headers()
            .get("sec-websocket-protocol")
            .map(HeaderValue::as_bytes),
        Some(&b"mqtt"[..]),
        "the bridge answers the mqtt subprotocol"
    );
    Ok(websocket)
}

/// The HTTP status of a refused upgrade.
pub fn refused_status(result: Result<WebSocketStream<TcpStream>, WsError>) -> StatusCode {
    match result {
        Err(WsError::Http(response)) => response.status(),
        Err(other) => panic!("expected an HTTP refusal, got {other}"),
        Ok(_) => panic!("expected an HTTP refusal, the upgrade succeeded"),
    }
}

/// The headers Cloudflare adds for a gateway at `ip`.
pub fn via_cloudflare(ip: &str) -> Vec<(&str, &str)> {
    vec![("cf-connecting-ip", ip)]
}

/// Reads the next data message (binary), skipping pings and pongs; panics on anything else.
pub async fn next_binary(websocket: &mut WebSocketStream<TcpStream>) -> Bytes {
    loop {
        match within("a message from the bridge", websocket.next()).await {
            Some(Ok(Message::Binary(data))) => return data,
            Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
            other => panic!("expected a binary message, got {other:?}"),
        }
    }
}

/// Reads until `wanted` bytes of binary messages arrived; returns them and each message's size.
pub async fn read_bytes(
    websocket: &mut WebSocketStream<TcpStream>,
    wanted: usize,
) -> (Vec<u8>, Vec<usize>) {
    let mut bytes = Vec::new();
    let mut sizes = Vec::new();
    while bytes.len() < wanted {
        let data = next_binary(websocket).await;
        sizes.push(data.len());
        bytes.extend_from_slice(&data);
    }
    (bytes, sizes)
}

/// Deterministic test bytes (a linear congruential generator): nothing a parser could take for a
/// protocol, including TLS-looking and zero bytes.
pub fn noise(seed: u64, length: usize) -> Vec<u8> {
    let mut state = seed;
    (0..length)
        .map(|_| {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (state >> 56) as u8
        })
        .collect()
}

/// A WebSocket as a byte stream for the test gateway's inner TLS: every write is one binary
/// message, reads return the bytes of binary messages in order, a close ends the stream.
pub struct WsBytes {
    websocket: WebSocketStream<TcpStream>,
    unread: Bytes,
}

impl WsBytes {
    pub fn new(websocket: WebSocketStream<TcpStream>) -> Self {
        Self {
            websocket,
            unread: Bytes::new(),
        }
    }
}

impl AsyncRead for WsBytes {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        loop {
            if !this.unread.is_empty() {
                let length = this.unread.len().min(buf.remaining());
                buf.put_slice(&this.unread.split_to(length));
                return Poll::Ready(Ok(()));
            }
            match ready!(Pin::new(&mut this.websocket).poll_next(cx)) {
                Some(Ok(Message::Binary(data))) => this.unread = data,
                Some(Ok(Message::Close(_))) | None => return Poll::Ready(Ok(())),
                Some(Ok(_)) => {}
                Some(Err(error)) => return Poll::Ready(Err(std::io::Error::other(error))),
            }
        }
    }
}

impl AsyncWrite for WsBytes {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        ready!(Pin::new(&mut this.websocket).poll_ready(cx)).map_err(std::io::Error::other)?;
        Pin::new(&mut this.websocket)
            .start_send(Message::Binary(Bytes::copy_from_slice(buf)))
            .map_err(std::io::Error::other)?;
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().websocket)
            .poll_flush(cx)
            .map_err(std::io::Error::other)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().websocket)
            .poll_close(cx)
            .map_err(std::io::Error::other)
    }
}

/// A fake OTLP/HTTP collector that keeps the bodies it receives, by path.
pub struct FakeCollector {
    address: SocketAddr,
    received: Arc<Mutex<Vec<Received>>>,
}

impl FakeCollector {
    pub fn start() -> Self {
        let listener = StdTcpListener::bind("127.0.0.1:0").expect("bind the fake collector");
        let address = listener.local_addr().expect("collector address");
        let received = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&received);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let sink = Arc::clone(&sink);
                std::thread::spawn(move || {
                    if let Some(request) = read_request(stream) {
                        sink.lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .push(request);
                    }
                });
            }
        });
        Self { address, received }
    }

    pub fn url(&self) -> String {
        format!("http://{}", self.address)
    }

    /// The bodies received on `path`, oldest first.
    pub fn bodies(&self, path: &str) -> Vec<Vec<u8>> {
        self.received
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|(received, _)| received == path)
            .map(|(_, body)| body.clone())
            .collect()
    }

    /// The last exported value of every sum data point, by metric name and attributes
    /// (`key=value` pairs, sorted, comma-separated).
    pub fn sums(&self) -> HashMap<(String, String), i64> {
        use opentelemetry_proto::tonic::collector::metrics::v1::ExportMetricsServiceRequest;
        use opentelemetry_proto::tonic::common::v1::any_value;
        use opentelemetry_proto::tonic::metrics::v1::{metric, number_data_point};
        use prost::Message as _;

        let mut latest = HashMap::new();
        for body in self.bodies("/v1/metrics") {
            let request =
                ExportMetricsServiceRequest::decode(body.as_slice()).expect("OTLP metrics");
            for resource in request.resource_metrics {
                for scope in resource.scope_metrics {
                    for metric in scope.metrics {
                        let Some(metric::Data::Sum(sum)) = metric.data else {
                            continue;
                        };
                        for point in sum.data_points {
                            let mut attributes: Vec<String> = point
                                .attributes
                                .iter()
                                .map(|attribute| {
                                    let value = match attribute
                                        .value
                                        .as_ref()
                                        .and_then(|value| value.value.as_ref())
                                    {
                                        Some(any_value::Value::StringValue(text)) => text.clone(),
                                        other => format!("{other:?}"),
                                    };
                                    format!("{}={value}", attribute.key)
                                })
                                .collect();
                            attributes.sort();
                            let value = match point.value {
                                Some(number_data_point::Value::AsInt(value)) => value,
                                Some(number_data_point::Value::AsDouble(value)) => {
                                    #[allow(
                                        clippy::cast_possible_truncation,
                                        reason = "the bridge's counters are integers"
                                    )]
                                    let value = value as i64;
                                    value
                                }
                                None => continue,
                            };
                            latest.insert((metric.name.clone(), attributes.join(",")), value);
                        }
                    }
                }
            }
        }
        latest
    }
}

type Received = (String, Vec<u8>);

/// Reads one request (Content-Length or chunked body), answers 200, and closes.
fn read_request(stream: StdTcpStream) -> Option<Received> {
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .ok()?;
    let mut reader = StdBufReader::new(stream.try_clone().ok()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line).ok()?;
    let path = request_line.split_whitespace().nth(1)?.to_owned();
    let mut length = 0;
    let mut chunked = false;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).ok()?;
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        let (name, value) = line.split_once(':')?;
        if name.eq_ignore_ascii_case("content-length") {
            length = value.trim().parse().ok()?;
        }
        if name.eq_ignore_ascii_case("transfer-encoding") && value.trim() == "chunked" {
            chunked = true;
        }
    }
    let mut body = Vec::new();
    if chunked {
        loop {
            let mut size = String::new();
            reader.read_line(&mut size).ok()?;
            let size = usize::from_str_radix(size.trim(), 16).ok()?;
            let mut chunk = vec![0; size + 2];
            reader.read_exact(&mut chunk).ok()?;
            if size == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..size]);
        }
    } else {
        body.resize(length, 0);
        reader.read_exact(&mut body).ok()?;
    }
    let mut stream = stream;
    stream
        .write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: application/x-protobuf\r\nContent-Length: 0\r\n\
              Connection: close\r\n\r\n",
        )
        .ok()?;
    Some((path, body))
}

/// Sends `bytes` as one binary message.
pub async fn send_binary(websocket: &mut WebSocketStream<TcpStream>, bytes: &[u8]) {
    within(
        "sending a binary message",
        websocket.send(Message::Binary(Bytes::copy_from_slice(bytes))),
    )
    .await
    .expect("send a binary message");
}

/// Connects through the bridge with MQTT CONNECT (keepalive 0) and waits for CONNACK.
pub async fn mqtt_session(address: SocketAddr, url: &str, ip: &str) -> WebSocketStream<TcpStream> {
    let mut websocket = connect_ws(address, url, &via_cloudflare(ip))
        .await
        .expect("open the WebSocket");
    send_binary(&mut websocket, &CONNECT_KEEPALIVE_0).await;
    let (answer, _) = read_bytes(&mut websocket, CONNACK.len()).await;
    assert_eq!(answer, CONNACK, "the broker's CONNACK through the bridge");
    websocket
}
