//! The gateway host's HTTPS endpoints the agent calls besides the WebSocket, over the outer TLS
//! settings (`cloud.trust`), on the host and port of `cloud.url` only, one HTTP/1.1 request per
//! connection: the renewal endpoint (`POST` [`RENEWAL_PATH`]) and the root CRL (`GET`
//! [`ROOT_CRL_PATH`], Laysi/trustiot-v2#563, [`crate::revocation`]).

use std::io;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt as _, Full, LengthLimitError, Limited};
use hyper::client::conn::http1;
use hyper::header::{ACCEPT, CONTENT_TYPE, HOST, HeaderMap, RETRY_AFTER};
use hyper::{Method, Request};
use hyper_util::rt::TokioIo;
use protocol::renewal::{RENEWAL_PATH, RenewalRequest};
use protocol::root_crl::{ROOT_CRL_CONTENT_TYPE, ROOT_CRL_PATH};
use rustls::ClientConfig;
use rustls_pki_types::ServerName;
use tokio_rustls::TlsConnector;

use super::{Answer, Endpoint, MAX_ANSWER_BYTES, REQUEST_TIMEOUT};
use crate::revocation::{Fetched, Source};
use crate::transport::{CloudUrl, connect_tcp};

/// The endpoints on the gateway host.
#[derive(Clone)]
pub struct HttpsEndpoint {
    host: String,
    port: u16,
    outer: Arc<ClientConfig>,
}

impl std::fmt::Debug for HttpsEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpsEndpoint")
            .field("host", &self.host)
            .field("port", &self.port)
            .finish_non_exhaustive()
    }
}

/// One request: its method, path, `Accept` and body (JSON, for a `POST`).
struct Call {
    method: Method,
    path: &'static str,
    accept: &'static str,
    json: Option<Vec<u8>>,
}

impl HttpsEndpoint {
    /// The endpoints on the host and port of `url`, with the outer TLS settings `outer`.
    pub fn new(url: &CloudUrl, outer: Arc<ClientConfig>) -> Self {
        Self {
            host: url.host().to_owned(),
            port: url.port(),
            outer,
        }
    }

    /// The URL renewal requests go to.
    pub fn url(&self) -> String {
        self.url_of(RENEWAL_PATH)
    }

    fn url_of(&self, path: &str) -> String {
        format!("https://{}{path}", self.authority())
    }

    /// `host`, or `host:port` for a port other than 443: the `Host` header.
    fn authority(&self) -> String {
        if self.port == 443 {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }

    /// `call`, answered within [`REQUEST_TIMEOUT`].
    async fn timed(&self, call: Call) -> io::Result<Answer> {
        let url = self.url_of(call.path);
        tokio::time::timeout(REQUEST_TIMEOUT, self.round_trip(call))
            .await
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!(
                        "no answer from {url} within {} s",
                        REQUEST_TIMEOUT.as_secs()
                    ),
                )
            })?
    }

    async fn round_trip(&self, call: Call) -> io::Result<Answer> {
        let tcp = connect_tcp(&self.host, self.port)
            .await
            .map_err(io::Error::other)?;
        let name = ServerName::try_from(self.host.clone()).map_err(io::Error::other)?;
        let tls = TlsConnector::from(Arc::clone(&self.outer))
            .connect(name, tcp)
            .await?;
        let (mut sender, connection) = http1::handshake(TokioIo::new(tls))
            .await
            .map_err(io::Error::other)?;
        let driver = tokio::spawn(connection);
        let _abort = AbortOnDrop(driver.abort_handle());
        let mut request = Request::builder()
            .method(call.method)
            .uri(call.path)
            .header(HOST, self.authority())
            .header(ACCEPT, call.accept);
        if call.json.is_some() {
            request = request.header(CONTENT_TYPE, "application/json");
        }
        let request = request
            .body(Full::new(Bytes::from(call.json.unwrap_or_default())))
            .map_err(io::Error::other)?;
        let response = sender
            .send_request(request)
            .await
            .map_err(io::Error::other)?;
        let status = response.status().as_u16();
        let retry_after = retry_after(response.headers());
        let body = match Limited::new(response.into_body(), MAX_ANSWER_BYTES)
            .collect()
            .await
        {
            Ok(collected) => collected.to_bytes().to_vec(),
            Err(error) if error.downcast_ref::<LengthLimitError>().is_some() => {
                return Err(io::Error::other(format!(
                    "the answer is larger than {MAX_ANSWER_BYTES} bytes"
                )));
            }
            Err(error) => return Err(io::Error::other(error)),
        };
        Ok(Answer {
            status,
            retry_after,
            body,
        })
    }
}

impl Endpoint for HttpsEndpoint {
    async fn post(&self, request: &RenewalRequest) -> io::Result<Answer> {
        let body = serde_json::to_vec(request).map_err(io::Error::other)?;
        self.timed(Call {
            method: Method::POST,
            path: RENEWAL_PATH,
            accept: "application/json",
            json: Some(body),
        })
        .await
    }
}

// A root CRL is at most `root_crl::MAX_BYTES`, within the size limit of every answer here.
const _: () = assert!(MAX_ANSWER_BYTES as u64 >= root_crl::MAX_BYTES);

impl Source for HttpsEndpoint {
    async fn fetch(&self) -> io::Result<Fetched> {
        let answer = self
            .timed(Call {
                method: Method::GET,
                path: ROOT_CRL_PATH,
                accept: ROOT_CRL_CONTENT_TYPE,
                json: None,
            })
            .await?;
        Ok(match answer.status {
            200 => Fetched::Crl(answer.body),
            404 => Fetched::NotPublished,
            status => Fetched::Status(status),
        })
    }

    fn describe(&self) -> String {
        self.url_of(ROOT_CRL_PATH)
    }
}

/// `Retry-After` as delay-seconds (the certs service sends only that form); an HTTP date or
/// anything else counts as none.
pub(super) fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    let text = headers.get(RETRY_AFTER)?.to_str().ok()?.trim();
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    // More digits than any sensible delay saturate; the caller caps it anyway.
    Some(Duration::from_secs(text.parse().unwrap_or(u64::MAX)))
}

/// Stops the HTTP connection's task when the request is done or dropped.
struct AbortOnDrop(tokio::task::AbortHandle);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}
