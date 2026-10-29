//! The HTTP client of every command: HTTP/1.1 over hyper, TLS with rustls and ring, one new
//! connection per request.
//!
//! - **Never a second attempt.** Each request opens its own connection and is sent once; there
//!   is no pool (a pooled client may resend a request on a connection that turned out closed)
//!   and no retry. Whether to try again is the operator's decision: a `send_code` that got no
//!   answer may still have mailed a code and used up the address's sending quota.
//! - **Trust**: `https://` verifies the server against the system's roots
//!   (`rustls-native-certs`: `SSL_CERT_FILE` or `SSL_CERT_DIR` when set, otherwise the system
//!   bundle) or only the certificates in `--ca-file`. Verification cannot be turned off. Plain
//!   `http://` is accepted only for loopback ([`crate::config::Origin`]).
//! - No proxy, no redirects (a 3xx is an error), at most [`MAX_BODY_BYTES`] of response body.
//! - Requests carry neither `Origin` nor `Sec-Fetch-Site`: the identity service's cross-site check
//!   lets such a request through (services/identity/README.md, "跨站請求").
//! - The extra headers of `--headers-file` (a Cloudflare Access service token) go on every request.
//! - Secrets (the cookie, those headers) are sensitive header values; log lines name the method,
//!   the path and the status, never a header or a body.

use std::fmt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use contract::ErrorBody;
use http::header::{ACCEPT, CONTENT_TYPE, COOKIE, DATE, HOST, HeaderName, SET_COOKIE, USER_AGENT};
use http::{HeaderValue, Method, Request, StatusCode};
use http_body_util::{BodyExt as _, Full, Limited};
use hyper::client::conn::http1;
use hyper_util::rt::TokioIo;
use rustls::RootCertStore;
use rustls::pki_types::pem::PemObject as _;
use rustls::pki_types::{CertificateDer, ServerName};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tracing::{debug, trace};
use zeroize::Zeroizing;

use crate::Error;
use crate::config::Origin;
use crate::files;
use crate::secret::{Secret, SessionCookie, SetCookie};

/// Larger response bodies are refused: the API answers with small JSON documents.
pub const MAX_BODY_BYTES: usize = 1024 * 1024;

/// Largest `--headers-file`.
const MAX_HEADERS_FILE_BYTES: usize = 8 * 1024;

/// The only headers `--headers-file` may set: a Cloudflare Access service token
/// (Laysi/trustiot-v2#619).
pub const EXTRA_HEADER_NAMES: [&str; 2] = ["cf-access-client-id", "cf-access-client-secret"];

/// Largest CA file `--ca-file` names.
const MAX_CA_FILE_BYTES: usize = 1024 * 1024;

/// The headers of `--headers-file`, sent on every request. Their values are secrets.
#[derive(Clone, Default)]
pub struct ExtraHeaders(Vec<(HeaderName, Secret)>);

impl ExtraHeaders {
    /// No extra headers.
    #[must_use]
    pub fn none() -> Self {
        Self::default()
    }

    /// Reads `path`: a file of mode 0600 or narrower with one `name: value` per line (blank
    /// lines and lines starting with `#` are skipped), naming exactly
    /// [`EXTRA_HEADER_NAMES`], each once. Values are visible ASCII without spaces.
    ///
    /// # Errors
    ///
    /// When the file is refused or a line is wrong; messages give the line number and, for a
    /// duplicate or a missing header, one of the two accepted names, never any text of the file
    /// (a value written where the name belongs would otherwise be shown).
    pub fn load(path: &Path) -> Result<Self, Error> {
        let bytes = files::read_secret(path, MAX_HEADERS_FILE_BYTES)?;
        Self::parse(&bytes).map_err(|reason| Error::Failed(format!("{}: {reason}", path.display())))
    }

    fn parse(bytes: &[u8]) -> Result<Self, String> {
        let text = std::str::from_utf8(bytes).map_err(|_| "is not UTF-8 text".to_owned())?;
        let mut headers: Vec<(HeaderName, Secret)> = Vec::new();
        for (index, line) in text.lines().enumerate() {
            let number = index + 1;
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((name, value)) = line.split_once(':') else {
                return Err(format!("line {number} is not `name: value`"));
            };
            let name = name.trim().to_ascii_lowercase();
            if !EXTRA_HEADER_NAMES.contains(&name.as_str()) {
                return Err(format!(
                    "line {number} names another header: only {} may be set",
                    EXTRA_HEADER_NAMES.join(" and ")
                ));
            }
            if headers.iter().any(|(known, _)| known.as_str() == name) {
                return Err(format!("line {number}: {name} is given twice"));
            }
            let value = value.trim();
            if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_graphic()) {
                return Err(format!(
                    "line {number}: the value of {name} must be visible ASCII without spaces"
                ));
            }
            let header = HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| format!("line {number}: {name} is not a header name"))?;
            headers.push((header, Secret::new(value.to_owned())));
        }
        for name in EXTRA_HEADER_NAMES {
            if !headers.iter().any(|(known, _)| known.as_str() == name) {
                return Err(format!("{name} is missing"));
            }
        }
        Ok(Self(headers))
    }

    /// The header names, for messages.
    #[must_use]
    pub fn names(&self) -> Vec<&str> {
        self.0.iter().map(|(name, _)| name.as_str()).collect()
    }
}

impl fmt::Debug for ExtraHeaders {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.names()).finish()
    }
}

/// Why a request got no usable response. Never contains a header, a cookie or a body.
#[derive(Debug, thiserror::Error)]
#[error("{method} {path} on {origin}: {problem}")]
pub struct RequestError {
    method: Method,
    path: &'static str,
    origin: String,
    problem: Problem,
}

impl RequestError {
    /// Whether the request may have reached the server: anything after the connection was
    /// established. Then the server may have acted on it, for example mailed a login code.
    #[must_use]
    pub fn may_have_arrived(&self) -> bool {
        !matches!(
            self.problem,
            Problem::Connect(_) | Problem::Tls(_) | Problem::Build(_)
        )
    }

    /// Whether another attempt may succeed: the connection could not be made, broke, or no
    /// complete response came within the timeout. A request that cannot be built and a TLS
    /// failure (an untrusted or wrong server certificate, most often) are not: trying again
    /// changes nothing.
    #[must_use]
    pub fn is_transient(&self) -> bool {
        matches!(
            self.problem,
            Problem::Connect(_) | Problem::Exchange(_) | Problem::Timeout(_)
        )
    }

    /// What went wrong, without the request: for log lines and records.
    #[must_use]
    pub fn problem_kind(&self) -> &'static str {
        match self.problem {
            Problem::Build(_) => "build",
            Problem::Connect(_) => "connect",
            Problem::Tls(_) => "tls",
            Problem::Exchange(_) => "exchange",
            Problem::Timeout(_) => "timeout",
        }
    }
}

/// What went wrong with a request.
#[derive(Debug, thiserror::Error)]
enum Problem {
    #[error("cannot build the request: {0}")]
    Build(String),
    #[error("cannot connect: {0}")]
    Connect(String),
    #[error("TLS: {0}")]
    Tls(String),
    #[error("{0}")]
    Exchange(String),
    #[error("no complete response within {0:?}")]
    Timeout(Duration),
}

/// A response: the status, the `Set-Cookie` values (wrapped as secrets), the `Date` header and
/// the body.
#[derive(Debug)]
pub struct Response {
    pub status: StatusCode,
    pub set_cookies: Vec<SetCookie>,
    pub date: Option<String>,
    pub body: Bytes,
}

/// Longest server-supplied text [`printable`] keeps, in characters.
const MAX_PRINTED_CHARS: usize = 120;

/// `text` from a server, made safe to print on a terminal: control characters (escape
/// sequences, carriage returns, line breaks) are written as `\u{..}` and the text is cut to
/// [`MAX_PRINTED_CHARS`] characters.
#[must_use]
pub fn printable(text: &str) -> String {
    let mut shown = String::with_capacity(text.len().min(MAX_PRINTED_CHARS));
    for (index, character) in text.chars().enumerate() {
        if index == MAX_PRINTED_CHARS {
            shown.push_str("...");
            break;
        }
        if character.is_control() {
            shown.extend(character.escape_unicode());
        } else {
            shown.push(character);
        }
    }
    shown
}

/// An error status from the platform, with the M1 error body's code, message key,
/// `details.reason` and the fields it names (each through [`printable`]); the body's other content
/// is never shown.
#[derive(Clone, Debug, thiserror::Error)]
pub struct ApiError {
    /// The request, as `METHOD /path`.
    pub request: String,
    pub status: StatusCode,
    /// The M1 `code`, when the body is an M1 error body.
    pub code: Option<String>,
    /// The M1 `message_key`, when the body is an M1 error body.
    pub message_key: Option<String>,
    /// The M1 body's `details.reason`, when it is a string (the certs service names why it
    /// refused a workstation with it).
    pub reason: Option<String>,
    /// `(field, message key)` of each field error.
    pub field_errors: Vec<(String, String)>,
}

impl ApiError {
    /// The error of `response` to `method path`.
    #[must_use]
    pub fn new(method: &Method, path: &str, response: &Response) -> Self {
        let parsed = serde_json::from_slice::<ErrorBody>(&response.body).ok();
        Self {
            request: format!("{method} {path}"),
            status: response.status,
            code: parsed
                .as_ref()
                .map(|body| printable(&body.code().to_string())),
            message_key: parsed
                .as_ref()
                .map(|body| printable(&body.message_key().to_string())),
            reason: parsed
                .as_ref()
                .and_then(|body| body.details().get("reason"))
                .and_then(serde_json::Value::as_str)
                .map(printable),
            field_errors: parsed
                .as_ref()
                .map(|body| {
                    body.field_errors()
                        .iter()
                        .map(|error| {
                            (
                                printable(&error.field),
                                printable(&error.message_key.to_string()),
                            )
                        })
                        .collect()
                })
                .unwrap_or_default(),
        }
    }

    /// Whether this is the platform saying the session is not valid: 401 with the M1 body's code
    /// `unauthenticated`. A bare 401, from a proxy in front of the platform for example, is not:
    /// the session may still be live, so it is never forgotten on that alone.
    #[must_use]
    pub fn is_session_rejected(&self) -> bool {
        self.status == StatusCode::UNAUTHORIZED && self.code.as_deref() == Some("unauthenticated")
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} answered {}", self.request, self.status.as_u16())?;
        match (&self.code, &self.message_key) {
            (Some(code), Some(key)) => write!(f, " {code} ({key})")?,
            _ => f.write_str(" without an M1 error body")?,
        }
        if let Some(reason) = &self.reason {
            write!(f, ", reason {reason}")?;
        }
        for (field, key) in &self.field_errors {
            write!(f, "; {field}: {key}")?;
        }
        Ok(())
    }
}

/// One request: the method, the path, a query, the session cookie, extra headers and a JSON body,
/// all optional but the first two.
pub struct Call<'a> {
    pub method: Method,
    pub path: &'static str,
    /// The query string without its `?`, already encoded (`url::form_urlencoded`). Never logged:
    /// log lines and errors name the path only.
    pub query: Option<&'a str>,
    pub cookie: Option<&'a SessionCookie>,
    /// Headers of this request alone (a registration's workstation signature). Their values are
    /// marked sensitive and never logged.
    pub headers: Vec<(HeaderName, HeaderValue)>,
    /// A JSON body; it may hold a secret (the login code), so it is wiped once sent.
    pub json: Option<Zeroizing<Vec<u8>>>,
}

/// Aborts the task that drives a connection when the request is done or abandoned.
struct AbortOnDrop(tokio::task::AbortHandle);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// A client for one platform origin.
pub struct Client {
    origin: Origin,
    tls: Option<TlsConnector>,
    headers: ExtraHeaders,
    timeout: Duration,
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Client")
            .field("origin", &self.origin)
            .field("tls", &self.tls.is_some())
            .field("headers", &self.headers)
            .field("timeout", &self.timeout)
            .finish()
    }
}

impl Client {
    /// A client for `origin`. For `https://` it trusts the certificates in `ca_file` only, or
    /// the system's roots when there is none.
    ///
    /// # Errors
    ///
    /// When the CA file or the system store yields no usable certificate, or `ca_file` is given
    /// for a plain `http://` origin.
    pub fn new(
        origin: Origin,
        ca_file: Option<&Path>,
        headers: ExtraHeaders,
        timeout: Duration,
    ) -> Result<Self, Error> {
        let tls = if origin.is_https() {
            let roots = match ca_file {
                Some(path) => roots_from_file(path)?,
                None => system_roots()?,
            };
            let config = rustls::ClientConfig::builder_with_provider(Arc::new(
                rustls::crypto::ring::default_provider(),
            ))
            .with_safe_default_protocol_versions()
            .map_err(|error| Error::Failed(format!("TLS configuration: {error}")))?
            .with_root_certificates(roots)
            .with_no_client_auth();
            Some(TlsConnector::from(Arc::new(config)))
        } else {
            if ca_file.is_some() {
                return Err(Error::Usage(
                    "--ca-file applies only to an https:// --url".to_owned(),
                ));
            }
            None
        };
        Ok(Self {
            origin,
            tls,
            headers,
            timeout,
        })
    }

    /// The origin requests go to.
    #[must_use]
    pub fn origin(&self) -> &Origin {
        &self.origin
    }

    /// Sends `call` once.
    ///
    /// # Errors
    ///
    /// When there is no complete response within the timeout; any status is a response.
    pub async fn send(&self, call: Call<'_>) -> Result<Response, RequestError> {
        let method = call.method.clone();
        let path = call.path;
        let failed = |problem| RequestError {
            method: method.clone(),
            path,
            origin: self.origin.to_string(),
            problem,
        };
        debug!(%method, path, origin = %self.origin, "sending a request");
        let request = self.request(call).map_err(failed)?;
        let response = tokio::time::timeout(self.timeout, self.exchange(request))
            .await
            .map_err(|_| failed(Problem::Timeout(self.timeout)))?
            .map_err(failed)?;
        debug!(%method, path, status = response.status.as_u16(), "received a response");
        Ok(response)
    }

    fn request(&self, call: Call<'_>) -> Result<Request<Full<Bytes>>, Problem> {
        let uri = match call.query {
            Some(query) => format!("{}?{query}", call.path),
            None => call.path.to_owned(),
        };
        let mut builder = Request::builder()
            .method(call.method)
            .uri(uri)
            .header(HOST, self.origin.authority())
            .header(ACCEPT, "application/json")
            .header(
                USER_AGENT,
                concat!("trustiot-workstation/", env!("CARGO_PKG_VERSION")),
            );
        if let Some(cookie) = call.cookie {
            builder = builder.header(COOKIE, sensitive(cookie.header_value().expose())?);
        }
        for (name, value) in &self.headers.0 {
            builder = builder.header(name, sensitive(value.expose())?);
        }
        for (name, mut value) in call.headers {
            value.set_sensitive(true);
            builder = builder.header(name, value);
        }
        let body = match call.json {
            Some(json) => {
                builder = builder.header(CONTENT_TYPE, "application/json");
                // hyper needs an owned buffer; this copy is not wiped (see the module docs of
                // `secret`). The caller's copy is, when `json` drops here.
                Bytes::copy_from_slice(&json)
            }
            None => Bytes::new(),
        };
        builder
            .body(Full::new(body))
            .map_err(|error| Problem::Build(error.to_string()))
    }

    async fn exchange(&self, request: Request<Full<Bytes>>) -> Result<Response, Problem> {
        let host = self.origin.host();
        let port = self.origin.port();
        let tcp = TcpStream::connect((host, port))
            .await
            .map_err(|error| Problem::Connect(error.to_string()))?;
        trace!(host, port, "connected");
        match &self.tls {
            Some(tls) => {
                let name = ServerName::try_from(host.to_owned())
                    .map_err(|_| Problem::Tls(format!("{host} is not a valid TLS server name")))?;
                let stream = tls
                    .connect(name, tcp)
                    .await
                    .map_err(|error| Problem::Tls(error.to_string()))?;
                trace!(host, "TLS established");
                round_trip(stream, request).await
            }
            None => round_trip(tcp, request).await,
        }
    }
}

fn sensitive(value: &str) -> Result<HeaderValue, Problem> {
    let mut header = HeaderValue::from_str(value).map_err(|_| {
        Problem::Build("a header value has characters a header cannot carry".into())
    })?;
    header.set_sensitive(true);
    Ok(header)
}

async fn round_trip<S>(stream: S, request: Request<Full<Bytes>>) -> Result<Response, Problem>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mut sender, connection) = http1::handshake(TokioIo::new(stream))
        .await
        .map_err(|error| Problem::Exchange(format!("HTTP handshake: {error}")))?;
    let driver = tokio::spawn(connection);
    let _abort = AbortOnDrop(driver.abort_handle());
    let response = sender
        .send_request(request)
        .await
        .map_err(|error| Problem::Exchange(format!("HTTP request: {error}")))?;
    trace!("received the response head");
    let status = response.status();
    let set_cookies = response
        .headers()
        .get_all(SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .map(|value| SetCookie::new(value.to_owned()))
        .collect();
    let date = response
        .headers()
        .get(DATE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let body = Limited::new(response.into_body(), MAX_BODY_BYTES)
        .collect()
        .await
        .map_err(|error| Problem::Exchange(format!("reading the response body: {error}")))?
        .to_bytes();
    Ok(Response {
        status,
        set_cookies,
        date,
        body,
    })
}

fn roots_from_file(path: &Path) -> Result<RootCertStore, Error> {
    let pem = files::read_public(path, MAX_CA_FILE_BYTES)?;
    let certificates: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(&pem)
        .collect::<Result<_, _>>()
        .map_err(|_| Error::BadFile {
            path: path.to_owned(),
            reason: "is not a PEM file of certificates",
        })?;
    let mut roots = RootCertStore::empty();
    let (added, _ignored) = roots.add_parsable_certificates(certificates);
    if added == 0 {
        return Err(Error::BadFile {
            path: path.to_owned(),
            reason: "holds no certificate the TLS library accepts",
        });
    }
    Ok(roots)
}

fn system_roots() -> Result<RootCertStore, Error> {
    let loaded = rustls_native_certs::load_native_certs();
    let mut roots = RootCertStore::empty();
    let (added, _unparsable) = roots.add_parsable_certificates(loaded.certs);
    if added == 0 {
        return Err(Error::Failed(format!(
            "no root certificate could be loaded from the system store ({} errors); install a \
             CA bundle, set SSL_CERT_FILE, or name the platform's CA with --ca-file",
            loaded.errors.len()
        )));
    }
    Ok(roots)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt as _;
    use std::time::{SystemTime, UNIX_EPOCH};

    use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, Issuer, KeyPair};
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::net::TcpListener;

    use super::*;

    /// Made-up Access values, built at run time (never secret-looking literals in the source).
    fn fake() -> (String, String) {
        let id = format!("{}.access", "x".repeat(32));
        let secret = format!(
            "{}-{}",
            ["not", "a", "real", "value"].join("-"),
            "f".repeat(24)
        );
        (id, secret)
    }

    fn headers_text(id: &str, secret: &str) -> String {
        format!("# service token\n\nCF-Access-Client-Id: {id}\ncf-access-client-secret:{secret}\n")
    }

    #[test]
    fn the_headers_file_names_exactly_the_access_headers() {
        let (id, secret) = fake();
        let headers = ExtraHeaders::parse(headers_text(&id, &secret).as_bytes()).expect("valid");
        assert_eq!(
            headers.names(),
            ["cf-access-client-id", "cf-access-client-secret"]
        );
        let shown = format!("{headers:?}");
        assert!(!shown.contains(&id) && !shown.contains(&secret), "{shown}");
        for wrong in [
            format!("cf-access-client-id: {id}\n"),
            format!("{}x-other: 1\n", headers_text(&id, &secret)),
            format!("{}cf-access-client-id: again\n", headers_text(&id, &secret)),
            format!("cf-access-client-id {id}\ncf-access-client-secret: {secret}\n"),
            format!("cf-access-client-id: has space\ncf-access-client-secret: {secret}\n"),
            format!("cf-access-client-id:\ncf-access-client-secret: {secret}\n"),
            format!("authorization: Bearer {secret}\n"),
            // A value pasted where the name belongs, and a line with only a value.
            format!("{secret}: {id}\n"),
            format!("{secret}\n"),
            format!("x-{id}: 1\n"),
        ] {
            let error = ExtraHeaders::parse(wrong.as_bytes()).expect_err(&wrong);
            assert!(
                !error.contains(&secret)
                    && !error.contains(&id)
                    && !error.contains("has space")
                    && !error.contains("authorization")
                    && !error.contains("x-other"),
                "{error}"
            );
            assert!(
                error.starts_with("line ") || error.ends_with(" is missing"),
                "{error}"
            );
        }
    }

    fn scratch() -> std::path::PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "workstation-http-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("after 1970")
                .as_nanos()
        ));
        files::ensure_directory(&directory).expect("a scratch directory");
        directory
    }

    #[test]
    fn a_headers_file_wider_than_0600_is_refused() {
        let (id, secret) = fake();
        let directory = scratch();
        let path = directory.join("access.headers");
        files::create_secret(&path, headers_text(&id, &secret).as_bytes()).expect("written");
        assert!(ExtraHeaders::load(&path).is_ok());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("chmod");
        let error = ExtraHeaders::load(&path).expect_err("refused");
        assert!(matches!(error, Error::TooOpen { .. }), "{error}");
        assert!(!error.to_string().contains(&secret));
        fs::remove_dir_all(directory).expect("cleaned up");
    }

    /// A test CA and a server certificate for 127.0.0.1 it signed.
    fn test_pki() -> (String, String, String) {
        let ca_key = KeyPair::generate().expect("a CA key");
        let mut ca_params = CertificateParams::new(Vec::new()).expect("params");
        ca_params
            .distinguished_name
            .push(DnType::CommonName, "workstation test CA");
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let ca_certificate = ca_params.self_signed(&ca_key).expect("a CA certificate");
        let issuer = Issuer::new(ca_params, ca_key);
        let server_key = KeyPair::generate().expect("a server key");
        let server = CertificateParams::new(vec!["127.0.0.1".to_owned()])
            .expect("params")
            .signed_by(&server_key, &issuer)
            .expect("a server certificate");
        (
            ca_certificate.pem(),
            server.pem(),
            server_key.serialize_pem(),
        )
    }

    /// Serves one TLS connection: answers 204 to the first request and records its head.
    async fn tls_server(
        certificate: &str,
        key: &str,
    ) -> (u16, tokio::task::JoinHandle<Option<String>>) {
        let certificates = CertificateDer::pem_slice_iter(certificate.as_bytes())
            .collect::<Result<Vec<_>, _>>()
            .expect("certificates");
        let key = rustls::pki_types::PrivateKeyDer::from_pem_slice(key.as_bytes()).expect("a key");
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .expect("versions")
        .with_no_client_auth()
        .with_single_cert(certificates, key)
        .expect("a server config");
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bound");
        let port = listener.local_addr().expect("an address").port();
        let task = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.ok()?;
            let mut stream = acceptor.accept(tcp).await.ok()?;
            let mut head = Vec::new();
            let mut buffer = [0_u8; 1024];
            while !head.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = stream.read(&mut buffer).await.ok()?;
                if read == 0 {
                    return None;
                }
                head.extend_from_slice(buffer.get(..read)?);
            }
            stream
                .write_all(b"HTTP/1.1 204 No Content\r\nconnection: close\r\n\r\n")
                .await
                .ok()?;
            stream.shutdown().await.ok()?;
            Some(String::from_utf8_lossy(&head).into_owned())
        });
        (port, task)
    }

    #[tokio::test]
    async fn https_trusts_only_the_ca_file_and_carries_the_extra_headers() {
        let (id, secret) = fake();
        let directory = scratch();
        let (ca, server_certificate, server_key) = test_pki();
        let ca_file = directory.join("ca.pem");
        fs::write(&ca_file, &ca).expect("written");
        let headers = ExtraHeaders::parse(headers_text(&id, &secret).as_bytes()).expect("valid");

        let (port, server) = tls_server(&server_certificate, &server_key).await;
        let origin = Origin::parse(&format!("https://127.0.0.1:{port}")).expect("an origin");
        let client = Client::new(origin, Some(&ca_file), headers, Duration::from_secs(60))
            .expect("a client");
        let response = client
            .send(Call {
                method: Method::DELETE,
                path: "/api/identity/v1/sessions/current",
                cookie: None,
                headers: Vec::new(),
                query: None,
                json: None,
            })
            .await
            .expect("a response");
        assert_eq!(response.status, StatusCode::NO_CONTENT);
        let head = server.await.expect("joined").expect("a request");
        let head = head.to_ascii_lowercase();
        assert!(
            head.starts_with("delete /api/identity/v1/sessions/current http/1.1"),
            "{head}"
        );
        assert!(
            head.contains(&format!("cf-access-client-id: {id}")),
            "{head}"
        );
        assert!(
            head.contains(&format!("cf-access-client-secret: {secret}")),
            "{head}"
        );
        assert!(
            !head.contains("origin:") && !head.contains("sec-fetch-site"),
            "{head}"
        );

        // A server whose certificate the CA file does not vouch for is refused before anything
        // is sent.
        let (_, other_certificate, other_key) = test_pki();
        let (port, server) = tls_server(&other_certificate, &other_key).await;
        let origin = Origin::parse(&format!("https://127.0.0.1:{port}")).expect("an origin");
        let client = Client::new(
            origin,
            Some(&ca_file),
            ExtraHeaders::none(),
            Duration::from_secs(60),
        )
        .expect("a client");
        let error = client
            .send(Call {
                method: Method::GET,
                path: "/",
                cookie: None,
                headers: Vec::new(),
                query: None,
                json: None,
            })
            .await
            .expect_err("an untrusted server");
        assert!(!error.may_have_arrived(), "{error}");
        assert!(error.to_string().contains("TLS"), "{error}");
        assert!(
            server.await.expect("joined").is_none(),
            "no request reached it"
        );
        fs::remove_dir_all(directory).expect("cleaned up");
    }

    #[test]
    fn plain_http_takes_no_ca_file() {
        let origin = Origin::parse("http://127.0.0.1:9").expect("an origin");
        let error = Client::new(
            origin,
            Some(Path::new("/nonexistent/ca.pem")),
            ExtraHeaders::none(),
            Duration::from_secs(1),
        )
        .expect_err("refused");
        assert!(matches!(error, Error::Usage(_)));
    }

    #[test]
    fn api_errors_show_only_m1_codes_keys_and_fields() {
        let response = Response {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            set_cookies: Vec::new(),
            date: None,
            body: Bytes::from_static(
                br#"{"code":"validation_failed","message_key":"workstation_certificate.name_invalid","field_errors":[{"field":"/w\u001b[2J\u001b]0;owned\u0007\r\nfake line","message_key":"workstation_certificate.name_invalid"}]}"#,
            ),
        };
        let shown = ApiError::new(&Method::POST, "/", &response).to_string();
        assert!(!shown.chars().any(char::is_control), "{shown:?}");
        assert!(
            shown.contains(r"/w\u{1b}[2J\u{1b}]0;owned\u{7}\u{d}\u{a}fake line"),
            "{shown}"
        );
        let long = "x".repeat(500);
        assert_eq!(printable(&long).chars().count(), MAX_PRINTED_CHARS + 3);
    }

    #[test]
    fn only_the_platforms_own_401_rejects_a_session() {
        let answer = |status: StatusCode, body: &'static [u8]| Response {
            status,
            set_cookies: Vec::new(),
            date: None,
            body: Bytes::from_static(body),
        };
        let platform = answer(
            StatusCode::UNAUTHORIZED,
            br#"{"code":"unauthenticated","message_key":"error.unauthenticated","field_errors":[]}"#,
        );
        assert!(ApiError::new(&Method::GET, "/", &platform).is_session_rejected());
        for other in [
            answer(StatusCode::UNAUTHORIZED, b"<html>Access denied</html>"),
            answer(StatusCode::UNAUTHORIZED, b""),
            answer(
                StatusCode::FORBIDDEN,
                br#"{"code":"permission_denied","message_key":"error.permission_denied","field_errors":[]}"#,
            ),
        ] {
            assert!(!ApiError::new(&Method::GET, "/", &other).is_session_rejected());
        }
    }
}
