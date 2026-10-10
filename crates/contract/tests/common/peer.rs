//! A one-connection HTTP/1.1 peer on a local socket, for driving the internal client through
//! real transport failures.

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

/// What the peer does after reading the request.
pub enum Reply {
    /// Writes this raw HTTP response, then closes the connection.
    Respond(String),
    /// Never answers and keeps the connection open.
    Silent,
    /// Resets the connection (TCP RST), as ztunnel does when no AuthorizationPolicy allows
    /// the call.
    Reset,
    /// Closes the connection (TCP FIN) without answering.
    Close,
}

/// A complete HTTP/1.1 response that closes the connection.
pub fn http_response(status: u16, content_type: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status} X\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\n\
         connection: close\r\n\r\n{body}",
        body.len()
    )
}

/// Listens on a free local port and serves one connection with `reply`. Returns the base URL
/// and a task that yields the raw request the client sent.
pub async fn serve_once(reply: Reply) -> (String, JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let base_url = format!("http://{}", listener.local_addr().expect("address"));
    let task = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let request = read_request(&mut stream).await;
        match reply {
            Reply::Respond(response) => {
                stream.write_all(response.as_bytes()).await.expect("write");
                stream.shutdown().await.expect("shutdown");
            }
            Reply::Silent => tokio::time::sleep(Duration::from_secs(60)).await,
            Reply::Reset => {
                // SO_LINGER 0 makes close() send RST instead of FIN.
                socket2::SockRef::from(&stream)
                    .set_linger(Some(Duration::ZERO))
                    .expect("set SO_LINGER");
                drop(stream);
            }
            Reply::Close => drop(stream),
        }
        request
    });
    (base_url, task)
}

/// Reads one request: the head, then as many body bytes as `content-length` says.
async fn read_request(stream: &mut TcpStream) -> String {
    let mut received = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
        let read = stream.read(&mut buffer).await.expect("read");
        if read == 0 {
            break;
        }
        received.extend_from_slice(&buffer[..read]);
        let text = String::from_utf8_lossy(&received);
        if let Some((head, body)) = text.split_once("\r\n\r\n") {
            let length = head
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())?
                })
                .unwrap_or(0);
            if body.len() >= length {
                break;
            }
        }
    }
    String::from_utf8(received).expect("request is UTF-8")
}
