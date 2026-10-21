//! Shared test support: fixtures, a byte source that counts reads, and a scripted HTTP/1.1
//! server on loopback.

#![allow(
    dead_code,
    reason = "each test binary uses a different part of the support module"
)]

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ai_clients::{ApiKey, ByteSource, ChunkFuture, EndpointConfig, Timeouts};
use bytes::Bytes;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;

/// How long a test waits for something it expects to happen soon (a closed connection, a
/// cancelled task). Only a bound: tests never assert elapsed time.
pub const WAIT: Duration = Duration::from_secs(30);

/// Time limits no test reaches unless it means to.
pub fn generous_timeouts() -> Timeouts {
    Timeouts {
        connect: Duration::from_secs(30),
        first_token: Duration::from_secs(120),
        stream_idle: Duration::from_secs(120),
        response: Duration::from_secs(120),
    }
}

pub fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

pub fn fixture(name: &str) -> String {
    std::fs::read_to_string(fixture_path(name))
        .unwrap_or_else(|error| panic!("read fixture {name}: {error}"))
}

pub fn fixture_json(name: &str) -> serde_json::Value {
    serde_json::from_str(&fixture(name))
        .unwrap_or_else(|error| panic!("parse fixture {name}: {error}"))
}

/// A key made at run time, so no key-like literal sits in the repository.
pub fn key_text() -> String {
    ["test", "only", &std::process::id().to_string()].join("-")
}

pub fn test_key() -> ApiKey {
    ApiKey::new(key_text()).expect("a valid test key")
}

pub fn endpoint(addr: SocketAddr, max_concurrent: usize) -> EndpointConfig {
    EndpointConfig {
        max_concurrent,
        timeouts: generous_timeouts(),
        ..EndpointConfig::new("test", format!("http://{addr}"))
    }
}

/// Feeds fixed chunks and counts every read; after the last chunk it ends the body, or never
/// answers when `hang` is set.
pub struct CountingSource {
    chunks: VecDeque<Bytes>,
    pub reads: Arc<AtomicUsize>,
    hang: bool,
}

impl CountingSource {
    pub fn new(chunks: Vec<Vec<u8>>, hang: bool) -> Self {
        Self {
            chunks: chunks.into_iter().map(Bytes::from).collect(),
            reads: Arc::new(AtomicUsize::new(0)),
            hang,
        }
    }

    /// The text cut into pieces of `size` bytes.
    pub fn split(text: &str, size: usize, hang: bool) -> Self {
        Self::new(
            text.as_bytes().chunks(size).map(<[u8]>::to_vec).collect(),
            hang,
        )
    }
}

impl ByteSource for CountingSource {
    fn next_chunk(&mut self) -> ChunkFuture<'_> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        let next = self.chunks.pop_front();
        let hang = self.hang;
        Box::pin(async move {
            match next {
                Some(chunk) => Ok(Some(chunk)),
                None if hang => std::future::pending().await,
                None => Ok(None),
            }
        })
    }
}

/// What the server does on one connection, after reading the request.
#[derive(Debug, Clone)]
pub enum Step {
    Send(Vec<u8>),
    /// Keep the connection open until the client closes it.
    Hang,
}

/// A request as the server received it.
#[derive(Debug, Clone)]
pub struct Recorded {
    pub head: String,
    pub body: Vec<u8>,
}

impl Recorded {
    pub fn header(&self, name: &str) -> Option<String> {
        self.head.lines().skip(1).find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.trim()
                .eq_ignore_ascii_case(name)
                .then(|| value.trim().to_owned())
        })
    }

    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).expect("the request body is JSON")
    }
}

/// A loopback HTTP/1.1 server. Connection N gets script N (the last script repeats).
pub struct Server {
    pub addr: SocketAddr,
    pub requests: Arc<Mutex<Vec<Recorded>>>,
    /// How many connections the client has closed while the server held them open.
    pub client_closes: watch::Receiver<usize>,
}

impl Server {
    pub async fn start(scripts: Vec<Vec<Step>>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a loopback port");
        let addr = listener.local_addr().expect("the listener address");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let (closes_tx, client_closes) = watch::channel(0usize);
        let closes_tx = Arc::new(closes_tx);
        let recorded = Arc::clone(&requests);
        tokio::spawn(async move {
            let mut connection = 0usize;
            loop {
                let Ok((socket, _)) = listener.accept().await else {
                    return;
                };
                let script = scripts
                    .get(connection)
                    .or(scripts.last())
                    .cloned()
                    .unwrap_or_default();
                connection += 1;
                let recorded = Arc::clone(&recorded);
                let closes_tx = Arc::clone(&closes_tx);
                tokio::spawn(serve(socket, script, recorded, closes_tx));
            }
        });
        Self {
            addr,
            requests,
            client_closes,
        }
    }

    pub fn request_count(&self) -> usize {
        self.requests.lock().expect("the request log lock").len()
    }

    pub fn request(&self, index: usize) -> Recorded {
        self.requests.lock().expect("the request log lock")[index].clone()
    }

    /// Waits until the client has closed `count` held connections.
    pub async fn wait_for_client_closes(&mut self, count: usize) {
        tokio::time::timeout(WAIT, self.client_closes.wait_for(|closed| *closed >= count))
            .await
            .unwrap_or_else(|_| {
                panic!("the client did not close {count} connection(s) within {WAIT:?}")
            })
            .expect("the server is running");
    }

    /// Waits until the server has received `count` requests.
    pub async fn wait_for_requests(&self, count: usize) {
        tokio::time::timeout(WAIT, async {
            while self.request_count() < count {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!("the server did not receive {count} request(s) within {WAIT:?}")
        });
    }
}

async fn serve(
    mut socket: TcpStream,
    script: Vec<Step>,
    recorded: Arc<Mutex<Vec<Recorded>>>,
    closes: Arc<watch::Sender<usize>>,
) {
    let mut buffer = Vec::new();
    let head_end = loop {
        if let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break end + 4;
        }
        let mut chunk = [0u8; 4096];
        match socket.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(n) => buffer.extend_from_slice(&chunk[..n]),
        }
    };
    let head = String::from_utf8_lossy(&buffer[..head_end]).into_owned();
    let length = head
        .lines()
        .find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.trim()
                .eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())?
        })
        .unwrap_or(0);
    let mut body = buffer[head_end..].to_vec();
    while body.len() < length {
        let mut chunk = [0u8; 4096];
        match socket.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(n) => body.extend_from_slice(&chunk[..n]),
        }
    }
    recorded
        .lock()
        .expect("the request log lock")
        .push(Recorded { head, body });
    for step in script {
        match step {
            Step::Send(bytes) => {
                if socket.write_all(&bytes).await.is_err() {
                    return;
                }
                let _ = socket.flush().await;
            }
            Step::Hang => {
                let mut chunk = [0u8; 1024];
                loop {
                    match socket.read(&mut chunk).await {
                        Ok(0) | Err(_) => {
                            closes.send_modify(|closed| *closed += 1);
                            return;
                        }
                        Ok(_) => {}
                    }
                }
            }
        }
    }
    let _ = socket.shutdown().await;
}

/// A complete response with a body and `content-length`.
pub fn http_response(status: u16, headers: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
    let mut out = format!(
        "HTTP/1.1 {status} Status\r\nconnection: close\r\ncontent-length: {}\r\n",
        body.len()
    );
    for (name, value) in headers {
        out.push_str(&format!("{name}: {value}\r\n"));
    }
    out.push_str("\r\n");
    let mut out = out.into_bytes();
    out.extend_from_slice(body);
    out
}

/// The head of a streaming response; the body runs until the connection closes.
pub fn sse_head() -> Vec<u8> {
    b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream; charset=utf-8\r\nrequest-id: req_test_stream\r\nconnection: close\r\n\r\n".to_vec()
}

/// The events of an SSE fixture up to (not including) the first event of type `stop_before`.
pub fn sse_prefix(text: &str, stop_before: &str) -> String {
    let marker = format!("event: {stop_before}\n");
    match text.find(&marker) {
        Some(end) => text.get(..end).expect("a char boundary").to_owned(),
        None => text.to_owned(),
    }
}
