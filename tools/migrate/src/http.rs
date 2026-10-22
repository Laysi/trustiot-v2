//! The HTTP client of the Kubernetes API and ClickHouse calls: HTTP/1.1 over hyper, with rustls
//! and ring for `https://`, trusting only the CA the deployment names (no system store, no public
//! roots), like the Postgres connections (checklist M3).
//!
//! Not reqwest: its `rustls-no-provider` feature, which ring needs, would be unified into every
//! other workspace member that builds a reqwest client (crates/contract, crates/testkit), whose
//! clients then panic for want of a default crypto provider in `cargo test --workspace`.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use bytes::Bytes;
use http::header::{AUTHORIZATION, CONTENT_TYPE, HOST};
use http::uri::PathAndQuery;
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

/// Every request, from connecting to the last byte of the body, gives up after this; the callers
/// retry or fail the run.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Larger response bodies are refused: the Job reads one Job or Secret object or a short query
/// result.
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

/// The credentials of one request.
#[derive(Clone, Copy)]
pub enum Auth<'a> {
    /// No `Authorization` header: the certs service's public CA and CRL files, which the MQTT
    /// broker's trust sidecar reads inside the mesh (src/mqtt_trust.rs).
    None,
    /// `Authorization: Bearer <token>` (a Kubernetes ServiceAccount token).
    Bearer(&'a str),
    /// `Authorization: Basic ...` (a ClickHouse account).
    Basic {
        username: &'a str,
        password: &'a str,
    },
}

impl Auth<'_> {
    fn header(self) -> Result<Option<HeaderValue>, RequestError> {
        let text = match self {
            Self::None => return Ok(None),
            Self::Bearer(token) => format!("Bearer {token}"),
            Self::Basic { username, password } => format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode(format!("{username}:{password}"))
            ),
        };
        let mut value = HeaderValue::from_str(&text).map_err(|_| {
            RequestError("the credentials contain characters a header cannot carry".to_owned())
        })?;
        value.set_sensitive(true);
        Ok(Some(value))
    }
}

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
}

/// A client for `http://` URLs; an `https://` URL fails instead of reaching for the platform's
/// roots.
#[must_use]
pub fn plain() -> Client {
    Client { tls: None }
}

/// A client for `https://` URLs that trusts only the certificates in `ca_file` (PEM, one or
/// more). The file is read once, when the client is built.
///
/// # Errors
///
/// When the file cannot be read or holds no usable certificate.
pub fn trusting(ca_file: &Path) -> Result<Client, ClientError> {
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
    })
}

/// Aborts the task that drives a connection when the request is done or abandoned.
struct AbortOnDrop(tokio::task::AbortHandle);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl Client {
    /// `GET url`.
    ///
    /// # Errors
    ///
    /// When there is no complete response within [`REQUEST_TIMEOUT`]; any status is a response.
    pub async fn get(&self, url: &str, auth: Auth<'_>) -> Result<Response, RequestError> {
        self.send(Method::GET, url, &[], auth, None).await
    }

    /// `POST url?query` with `body` as plain text.
    ///
    /// # Errors
    ///
    /// When there is no complete response within [`REQUEST_TIMEOUT`]; any status is a response.
    pub async fn post(
        &self,
        url: &str,
        query: &[(&str, &str)],
        auth: Auth<'_>,
        body: String,
    ) -> Result<Response, RequestError> {
        self.send(
            Method::POST,
            url,
            query,
            auth,
            Some(("text/plain; charset=utf-8", body)),
        )
        .await
    }

    /// `PATCH url?query` with `body` as a JSON merge patch (RFC 7386), as the Kubernetes API
    /// takes it.
    ///
    /// # Errors
    ///
    /// When there is no complete response within [`REQUEST_TIMEOUT`]; any status is a response.
    pub async fn merge_patch(
        &self,
        url: &str,
        query: &[(&str, &str)],
        auth: Auth<'_>,
        body: String,
    ) -> Result<Response, RequestError> {
        self.send(
            Method::PATCH,
            url,
            query,
            auth,
            Some(("application/merge-patch+json", body)),
        )
        .await
    }

    async fn send(
        &self,
        method: Method,
        url: &str,
        query: &[(&str, &str)],
        auth: Auth<'_>,
        body: Option<(&'static str, String)>,
    ) -> Result<Response, RequestError> {
        tokio::time::timeout(
            REQUEST_TIMEOUT,
            self.exchange(method, url, query, auth, body),
        )
        .await
        .map_err(|_| RequestError(format!("no response within {REQUEST_TIMEOUT:?}")))?
    }

    async fn exchange(
        &self,
        method: Method,
        url: &str,
        query: &[(&str, &str)],
        auth: Auth<'_>,
        body: Option<(&'static str, String)>,
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
        let mut target = uri
            .path_and_query()
            .map_or("/", PathAndQuery::as_str)
            .to_owned();
        for (index, (name, value)) in query.iter().enumerate() {
            let separator = if index == 0 && !target.contains('?') {
                '?'
            } else {
                '&'
            };
            target.push(separator);
            target.extend(utf8_percent_encode(name, QUERY_VALUE));
            target.push('=');
            target.extend(utf8_percent_encode(value, QUERY_VALUE));
        }
        let mut builder = Request::builder()
            .method(method)
            .uri(target)
            .header(HOST, authority.as_str());
        if let Some(value) = auth.header()? {
            builder = builder.header(AUTHORIZATION, value);
        }
        let body = match body {
            Some((content_type, body)) => {
                builder = builder.header(CONTENT_TYPE, content_type);
                body
            }
            None => String::new(),
        };
        let request = builder
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
    async fn a_client_never_switches_scheme() {
        let error = plain()
            .get("https://example.invalid/", Auth::Bearer("t"))
            .await
            .expect_err("plain refuses https");
        assert!(error.to_string().contains("needs a CA file"), "{error}");
        let error = plain()
            .get("http://user:pw@example.invalid/", Auth::Bearer("t"))
            .await
            .expect_err("credentials in the URL are refused");
        assert!(!error.to_string().contains("pw"), "{error}");
    }

    #[test]
    fn credentials_are_sensitive_headers() {
        let header = Auth::Basic {
            username: "u",
            password: "p",
        }
        .header()
        .expect("a valid header")
        .expect("a header for basic credentials");
        assert!(header.is_sensitive());
        assert_eq!(header, "Basic dTpw");
    }

    #[test]
    fn public_files_are_fetched_without_credentials() {
        assert!(Auth::None.header().expect("no header to build").is_none());
    }
}
