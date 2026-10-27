//! The HTTP client of the ClickHouse calls: HTTP/1.1 over hyper, with rustls and ring for
//! `https://`, trusting only the CA the deployment names (no system store, no public roots), like
//! the Postgres connections (checklist M3). One connection per request.
//!
//! A copy of the telemetry-query service's client (services/telemetry-query/src/transport.rs),
//! itself the migration Job's design (tools/migrate/src/http.rs) trimmed to one kind of request;
//! services share code only through crates, and moving the three copies into one crate is a
//! follow-up. Not reqwest: its `rustls-no-provider` feature, which ring needs, would be unified
//! into every other workspace member that builds a reqwest client (crates/contract,
//! crates/testkit), whose clients then panic for want of a default crypto provider in
//! `cargo test --workspace`.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use bytes::Bytes;
use http::header::{AUTHORIZATION, CONTENT_TYPE, HOST};
use http::{HeaderValue, Method, Request, StatusCode, Uri};
use http_body_util::{BodyExt as _, Full, Limited};
use hyper::client::conn::http1;
use hyper_util::rt::TokioIo;
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use rustls::RootCertStore;
use rustls::pki_types::pem::PemObject as _;
use rustls::pki_types::{CertificateDer, ServerName};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

/// Larger response bodies are refused. The service's statements answer a few bytes (readiness)
/// or a few rows (the telemetry path's high-water marks, Laysi/trustiot-v2#641), far below this.
const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;

/// Query values are percent-encoded except RFC 3986's unreserved characters.
const QUERY_VALUE: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// Why the client could not be built.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("cannot read the CA file {path}: {message}")]
    CaFile { path: String, message: String },
    #[error("the CA file {path} holds no certificate the TLS library accepts")]
    NoCertificate { path: String },
    #[error("cannot build the HTTP client: {0}")]
    Build(String),
}

/// Why a request got no response. Never contains the credentials it carried.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct RequestError(String);

/// A response: its status and its body as text (invalid UTF-8 replaced).
#[derive(Debug)]
pub struct Response {
    pub status: StatusCode,
    pub body: String,
}

/// An HTTP client for one kind of URL: `http://` only ([`plain`]) or `https://` only
/// ([`trusting`]), so a client never falls back from one to the other.
#[derive(Clone)]
pub struct Client {
    tls: Option<TlsConnector>,
    timeout: Duration,
}

/// A client for `http://` URLs whose requests give up after `timeout`; an `https://` URL fails
/// instead of reaching for the platform's roots.
#[must_use]
pub fn plain(timeout: Duration) -> Client {
    Client { tls: None, timeout }
}

/// A client for `https://` URLs that trusts only the certificates in `ca_file` (PEM, one or
/// more), whose requests give up after `timeout`. The file is read once, when the client is
/// built.
///
/// # Errors
///
/// When the file cannot be read or holds no usable certificate.
pub fn trusting(ca_file: &Path, timeout: Duration) -> Result<Client, ClientError> {
    let path = ca_file.display().to_string();
    let certificates: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(ca_file)
        .and_then(Iterator::collect)
        .map_err(|error| ClientError::CaFile {
            path: path.clone(),
            message: error.to_string(),
        })?;
    let mut roots = RootCertStore::empty();
    let (added, _ignored) = roots.add_parsable_certificates(certificates);
    if added == 0 {
        return Err(ClientError::NoCertificate { path });
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|error| ClientError::Build(error.to_string()))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(Client {
        tls: Some(TlsConnector::from(Arc::new(config))),
        timeout,
    })
}

/// Aborts the task that drives a connection when the request is done or abandoned; ClickHouse
/// then sees the connection close.
struct AbortOnDrop(tokio::task::AbortHandle);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// `Authorization: Basic ...` for a ClickHouse account, marked sensitive so it is never logged.
fn basic(username: &str, password: &str) -> Result<HeaderValue, RequestError> {
    let text = format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("{username}:{password}"))
    );
    let mut value = HeaderValue::from_str(&text).map_err(|_| {
        RequestError("the credentials contain characters a header cannot carry".to_owned())
    })?;
    value.set_sensitive(true);
    Ok(value)
}

impl Client {
    /// `POST url?query` with `body` as plain text, as `username`.
    ///
    /// # Errors
    ///
    /// When there is no complete response within the client's timeout; any status is a
    /// response.
    pub async fn post(
        &self,
        url: &str,
        query: &[(&str, &str)],
        (username, password): (&str, &str),
        body: String,
    ) -> Result<Response, RequestError> {
        tokio::time::timeout(
            self.timeout,
            self.exchange(url, query, basic(username, password)?, body),
        )
        .await
        .map_err(|_| RequestError(format!("no response within {:?}", self.timeout)))?
    }

    async fn exchange(
        &self,
        url: &str,
        query: &[(&str, &str)],
        authorization: HeaderValue,
        body: String,
    ) -> Result<Response, RequestError> {
        let uri: Uri = url
            .parse()
            .map_err(|error| RequestError(format!("invalid URL: {error}")))?;
        let https = match uri.scheme_str() {
            Some("https") => true,
            Some("http") => false,
            _ => {
                return Err(RequestError(
                    "the URL is not http:// or https://".to_owned(),
                ));
            }
        };
        match (&self.tls, https) {
            (Some(_), false) => {
                return Err(RequestError("this client only speaks https".to_owned()));
            }
            (None, true) => {
                return Err(RequestError(
                    "https needs a CA file, and none was configured".to_owned(),
                ));
            }
            _ => {}
        }
        let authority = uri
            .authority()
            .ok_or_else(|| RequestError("the URL has no host".to_owned()))?;
        if authority.as_str().contains('@') {
            return Err(RequestError(
                "a URL must not carry credentials (user:password@)".to_owned(),
            ));
        }
        let host = authority
            .host()
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_owned();
        // The scheme's port only when the URL names none; a port that is not a u16 is refused,
        // never replaced, so a request never goes to a port other than the configured one.
        let port = match authority.port() {
            None => {
                if https {
                    443
                } else {
                    80
                }
            }
            Some(port) => port
                .as_str()
                .parse::<u16>()
                .ok()
                .filter(|port| *port != 0)
                .ok_or_else(|| RequestError("the URL's port is not a valid port".to_owned()))?,
        };
        let mut target = uri.path().to_owned();
        for (index, (name, value)) in query.iter().enumerate() {
            target.push(if index == 0 { '?' } else { '&' });
            target.extend(utf8_percent_encode(name, QUERY_VALUE));
            target.push('=');
            target.extend(utf8_percent_encode(value, QUERY_VALUE));
        }
        let request = Request::builder()
            .method(Method::POST)
            .uri(target)
            .header(HOST, authority.as_str())
            .header(AUTHORIZATION, authorization)
            .header(CONTENT_TYPE, "text/plain; charset=utf-8")
            .body(Full::new(Bytes::from(body)))
            .map_err(|error| RequestError(format!("cannot build the request: {error}")))?;

        let tcp = TcpStream::connect((host.as_str(), port))
            .await
            .map_err(|error| RequestError(format!("cannot connect to {host}:{port}: {error}")))?;
        match &self.tls {
            Some(tls) => {
                let name = ServerName::try_from(host.clone())
                    .map_err(|_| RequestError(format!("{host} is not a valid TLS server name")))?;
                let stream = tls
                    .connect(name, tcp)
                    .await
                    .map_err(|error| RequestError(format!("TLS with {host}: {error}")))?;
                round_trip(stream, request).await
            }
            None => round_trip(tcp, request).await,
        }
    }
}

async fn round_trip<S>(stream: S, request: Request<Full<Bytes>>) -> Result<Response, RequestError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mut sender, connection) = http1::handshake(TokioIo::new(stream))
        .await
        .map_err(|error| RequestError(format!("HTTP handshake: {error}")))?;
    let driver = tokio::spawn(connection);
    let _abort = AbortOnDrop(driver.abort_handle());
    let response = sender
        .send_request(request)
        .await
        .map_err(|error| RequestError(format!("HTTP request: {error}")))?;
    let status = response.status();
    let body = Limited::new(response.into_body(), MAX_BODY_BYTES)
        .collect()
        .await
        .map_err(|error| RequestError(format!("reading the response body: {error}")))?
        .to_bytes();
    Ok(Response {
        status,
        body: String::from_utf8_lossy(&body).into_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_client_never_switches_scheme_or_leaks_credentials() {
        let client = plain(Duration::from_secs(5));
        let error = client
            .post("https://example.invalid/", &[], ("u", "p"), String::new())
            .await
            .expect_err("plain refuses https");
        assert!(error.to_string().contains("needs a CA file"), "{error}");
        let error = client
            .post(
                "http://user:secret-in-url@example.invalid/",
                &[],
                ("u", "p"),
                String::new(),
            )
            .await
            .expect_err("credentials in the URL are refused");
        assert!(!error.to_string().contains("secret-in-url"), "{error}");
    }

    #[test]
    fn credentials_are_sensitive_headers() {
        let header = basic("u", "p").expect("a valid header");
        assert!(header.is_sensitive());
        assert_eq!(header, "Basic dTpw");
    }
}
