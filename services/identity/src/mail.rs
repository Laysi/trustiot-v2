//! Email through the deployment's SMTP relay (the chart's `smtp.*`; decision 18): the login
//! codes of [`crate::login`]. On-prem this is the customer's relay; the SaaS uses the relay in
//! docs/architecture/secrets.md ("SMTP 帳密").
//!
//! A small SMTP client (RFC 5321) rather than a mail crate: the service sends one kind of
//! message, and the rules that matter here are few and tested below.
//!
//! - **One shape of message.** `multipart/alternative` (RFC 2046) with a `text/plain` part
//!   first and a `text/html` part last, both UTF-8 in base64. The boundary is random per message
//!   and starts with `=_`, which base64 never contains, so no line of a part can end it.
//! - **Always TLS.** `tls` speaks TLS from the first byte; `starttls` requires the relay to
//!   offer `STARTTLS` and upgrades before anything else is sent, and never falls back to plain
//!   text. Bytes the relay sends between accepting `STARTTLS` and the handshake are refused (the
//!   STARTTLS command injection of CVE-2011-0411). The relay's certificate must chain to the
//!   image's CA bundle ([`SYSTEM_CA_FILE`]), or to `TRUSTIOT_SMTP_CA_FILE` when it is set, and
//!   match `smtp.host`.
//! - **`AUTH PLAIN`** only when credentials are configured, and only over TLS (so always).
//! - **Nothing personal or secret in errors.** A refusal is reported by the step and the reply
//!   code (and the enhanced status code, digits only), never the relay's text, which may quote
//!   the recipient. The message and the password never reach a log.
//! - **Bounded.** Connecting and the TLS handshake take at most [`CONNECT_TIMEOUT`], each reply
//!   at most [`REPLY_TIMEOUT`]; a reply line is at most [`MAX_REPLY_LINE`] bytes and a reply at
//!   most [`MAX_REPLY_LINES`] lines. The caller bounds the whole delivery.

use std::fmt;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use rustls::RootCertStore;
use rustls::pki_types::pem::PemObject as _;
use rustls::pki_types::{CertificateDer, ServerName};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use zeroize::Zeroizing;

use crate::config::{Platform, SmtpPassword, SmtpSecurity};

/// The CA bundle of the identity image (distroless, Debian layout): the trust anchors of the
/// relay's certificate unless `TRUSTIOT_SMTP_CA_FILE` names others.
pub const SYSTEM_CA_FILE: &str = "/etc/ssl/certs/ca-certificates.crt";

/// How long the TCP connection and the TLS handshake may each take.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// How long the relay may take to answer one command.
pub const REPLY_TIMEOUT: Duration = Duration::from_secs(30);

/// The longest reply line read, in bytes. RFC 5321 allows 512; this leaves room for relays
/// that exceed it.
pub const MAX_REPLY_LINE: usize = 2048;

/// The most lines one reply may have.
pub const MAX_REPLY_LINES: usize = 100;

/// The address rules, shared with the login flow and the migration Job (`src/email.rs`).
pub use crate::email::{MAX_ADDRESS_LEN, is_address};

/// Bytes of a subject or name per RFC 2047 encoded word: a multiple of 3, so each word's base64
/// has no padding, and short enough that a word is at most 64 characters.
const ENCODED_WORD_BYTES: usize = 39;

/// Base64 characters per body line (RFC 2045 allows at most 76).
const BODY_LINE: usize = 76;

/// The sender, from `smtp.from`: `Name <address>`, `"Name" <address>`, `<address>` or a bare
/// address. The name may be any text without control characters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mailbox {
    name: Option<String>,
    address: String,
}

/// The longest sender name, in characters.
const MAX_NAME_CHARS: usize = 200;

impl Mailbox {
    /// Reads `smtp.from`; `None` when it is not one of the accepted forms.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        let (name, address) = match text.strip_suffix('>') {
            Some(rest) => {
                let (name, address) = rest.rsplit_once('<')?;
                let name = name.trim();
                let name = name
                    .strip_prefix('"')
                    .and_then(|name| name.strip_suffix('"'))
                    .unwrap_or(name);
                (Some(name).filter(|name| !name.is_empty()), address)
            }
            None => (None, text),
        };
        if !is_address(address) {
            return None;
        }
        if let Some(name) = name
            && (name.chars().count() > MAX_NAME_CHARS || name.chars().any(char::is_control))
        {
            return None;
        }
        Some(Self {
            name: name.map(str::to_owned),
            address: address.to_owned(),
        })
    }

    /// The address, for `MAIL FROM`.
    #[must_use]
    pub fn address(&self) -> &str {
        &self.address
    }

    /// The `From` header's value: the name as a quoted string when it is printable ASCII without
    /// `"` or `\`, otherwise as RFC 2047 encoded words.
    fn header(&self) -> String {
        match &self.name {
            None => format!("<{}>", self.address),
            Some(name)
                if name
                    .bytes()
                    .all(|byte| (0x20..0x7f).contains(&byte) && byte != b'"' && byte != b'\\') =>
            {
                format!("\"{name}\" <{}>", self.address)
            }
            Some(name) => format!("{} <{}>", encoded_words(name), self.address),
        }
    }

    /// The domain of the address, for the `Message-ID`.
    fn domain(&self) -> &str {
        self.address
            .rsplit_once('@')
            .map_or("localhost", |(_, domain)| domain)
    }
}

/// `text` as RFC 2047 `B` encoded words in UTF-8, split at character boundaries and folded onto
/// continuation lines.
fn encoded_words(text: &str) -> String {
    let mut words = Vec::new();
    let mut start = 0;
    let mut end = 0;
    for (index, c) in text.char_indices() {
        let next = index + c.len_utf8();
        if next - start > ENCODED_WORD_BYTES {
            words.push(text.get(start..end).unwrap_or_default());
            start = end;
        }
        end = next;
    }
    if start < text.len() {
        words.push(text.get(start..).unwrap_or_default());
    }
    words
        .iter()
        .map(|word| format!("=?UTF-8?B?{}?=", STANDARD.encode(word.as_bytes())))
        .collect::<Vec<_>>()
        .join("\r\n ")
}

/// One message to one recipient, as plain text and as HTML. `Debug` never prints it: both
/// bodies hold a login code. Wiped on drop.
pub struct Message {
    to: String,
    subject: String,
    text: Zeroizing<String>,
    html: Zeroizing<String>,
}

impl Message {
    /// A message to `to` (checked with [`is_address`] when it is sent) with the same content as
    /// plain `text` and as a complete `html` document; a client shows the one it prefers. The
    /// lines of both end in `\n`; they are sent as CRLF.
    #[must_use]
    pub fn new(
        to: String,
        subject: String,
        text: Zeroizing<String>,
        html: Zeroizing<String>,
    ) -> Self {
        Self {
            to,
            subject,
            text,
            html,
        }
    }
}

impl fmt::Debug for Message {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Message(<redacted>)")
    }
}

#[cfg(test)]
impl Message {
    pub(crate) fn to(&self) -> &str {
        &self.to
    }

    pub(crate) fn subject(&self) -> &str {
        &self.subject
    }

    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    pub(crate) fn html(&self) -> &str {
        &self.html
    }
}

/// The prefix of every MIME boundary. `_` is not a base64 character, so no line of a base64
/// part can start with `--=_` and end the part early (RFC 2046 section 5.1.1); `=` makes the
/// boundary a quoted parameter.
const BOUNDARY_PREFIX: &str = "=_trustiot_";

/// The pieces of the multipart body around the boundary and the parts, named so that
/// [`render`] sizes its buffer from the same text it writes.
const CRLF: &str = "\r\n";
const DELIMITER: &str = "--";
const PART_TYPE: &str = "\r\nContent-Type: ";
const PART_ENCODING: &str = "\r\nContent-Transfer-Encoding: base64\r\n\r\n";
const CLOSE: &str = "--\r\n";

/// `content` with every line end CRLF: a `\n` without a `\r` before it gets one. One pass into
/// a buffer sized for the worst case (every byte a `\n`), so the content, which holds the code,
/// is never in a reallocated (and unwiped) buffer.
fn crlf(content: &str) -> Zeroizing<String> {
    let mut out = Zeroizing::new(String::with_capacity(crlf_len_bound(content)));
    let capacity = out.capacity();
    let mut previous = '\0';
    for character in content.chars() {
        if character == '\n' && previous != '\r' {
            out.push('\r');
        }
        out.push(character);
        previous = character;
    }
    debug_assert_eq!(out.capacity(), capacity, "the CRLF buffer was reallocated");
    out
}

/// At least the length of [`crlf`]'s output: one more byte for each `\n`.
fn crlf_len_bound(content: &str) -> usize {
    content.len() + content.bytes().filter(|byte| *byte == b'\n').count()
}

/// At least the length that [`push_base64_part`] appends for `content`: base64 of the CRLF
/// content in lines of [`BODY_LINE`] characters, each ending in CRLF.
fn base64_part_len_bound(content: &str) -> usize {
    let encoded = crlf_len_bound(content).div_ceil(3) * 4;
    encoded + encoded.div_ceil(BODY_LINE) * 2
}

/// A part's body: CRLF line ends, then base64 in lines of [`BODY_LINE`] characters, each line
/// ending in CRLF.
fn push_base64_part(out: &mut String, content: &str) {
    let crlf = crlf(content);
    // `encode` allocates the exact encoded length once.
    let encoded = Zeroizing::new(STANDARD.encode(crlf.as_bytes()));
    for chunk in encoded.as_bytes().chunks(BODY_LINE) {
        // Base64 is ASCII, so every chunk is a str.
        out.push_str(std::str::from_utf8(chunk).unwrap_or_default());
        out.push_str("\r\n");
    }
}

/// The message as sent after `DATA`: headers, then a `multipart/alternative` body with the
/// plain text first and the HTML last (the last part is the preferred one), each in base64;
/// dot-stuffed, ending with CRLF (the caller adds the final `.`). `boundary` is random per
/// message ([`boundary`]).
fn render(
    from: &Mailbox,
    message: &Message,
    date: &str,
    message_id: &str,
    boundary: &str,
) -> Zeroizing<Vec<u8>> {
    let headers = [
        format!("Date: {date}"),
        format!("From: {}", from.header()),
        format!("To: <{}>", message.to),
        format!("Subject: {}", encoded_words(&message.subject)),
        format!("Message-ID: <{message_id}@{}>", from.domain()),
        "MIME-Version: 1.0".to_owned(),
        format!("Content-Type: multipart/alternative; boundary=\"{boundary}\""),
        // RFC 3834: no auto-replies to this message.
        "Auto-Submitted: auto-generated".to_owned(),
    ];
    let parts = [
        ("text/plain; charset=utf-8", &message.text),
        ("text/html; charset=utf-8", &message.html),
    ];
    // Sized for the whole message before anything is written, so the buffer that holds the code
    // is never reallocated (which would leave a copy behind).
    let size = headers
        .iter()
        .map(|header| header.len() + CRLF.len())
        .sum::<usize>()
        + CRLF.len()
        + parts
            .iter()
            .map(|(content_type, content)| {
                DELIMITER.len()
                    + boundary.len()
                    + PART_TYPE.len()
                    + content_type.len()
                    + PART_ENCODING.len()
                    + base64_part_len_bound(content)
            })
            .sum::<usize>()
        + DELIMITER.len()
        + boundary.len()
        + CLOSE.len();
    let mut text = Zeroizing::new(String::with_capacity(size));
    let capacity = text.capacity();
    for header in &headers {
        text.push_str(header);
        text.push_str(CRLF);
    }
    text.push_str(CRLF);
    for (content_type, content) in parts {
        text.push_str(DELIMITER);
        text.push_str(boundary);
        text.push_str(PART_TYPE);
        text.push_str(content_type);
        text.push_str(PART_ENCODING);
        push_base64_part(&mut text, content);
    }
    text.push_str(DELIMITER);
    text.push_str(boundary);
    text.push_str(CLOSE);
    debug_assert_eq!(
        text.capacity(),
        capacity,
        "the message buffer was reallocated"
    );
    dot_stuff(&text)
}

/// RFC 5321 section 4.5.2: a line that starts with `.` gets another one, so no line of the
/// message can end it early. The output is sized exactly before anything is written.
fn dot_stuff(text: &str) -> Zeroizing<Vec<u8>> {
    let dots = text
        .split_inclusive("\r\n")
        .filter(|line| line.starts_with('.'))
        .count();
    let mut out = Zeroizing::new(Vec::with_capacity(text.len() + dots));
    let capacity = out.capacity();
    for line in text.split_inclusive("\r\n") {
        if line.starts_with('.') {
            out.push(b'.');
        }
        out.extend_from_slice(line.as_bytes());
    }
    debug_assert_eq!(
        out.capacity(),
        capacity,
        "the dot-stuffed buffer was reallocated"
    );
    out
}

/// Why the relay settings cannot be used. The service still starts, logs this at ERROR and
/// runs without a relay: `send_code` answers 503 until the settings are fixed and the service
/// restarts. Never contains a setting's value, except the CA file's path.
#[derive(Debug, thiserror::Error)]
pub enum MailConfigError {
    /// A setting is missing or invalid (`Platform::validate_smtp`).
    #[error("{key} {message}")]
    Setting {
        /// The dotted key, for example `platform.smtp_from`.
        key: String,
        /// Why, without the value.
        message: String,
    },
    /// The CA file cannot be read or parsed.
    #[error("cannot read the SMTP CA file {path}: {message}")]
    CaFile {
        /// The file.
        path: String,
        /// Why.
        message: String,
    },
    /// The CA file holds no usable certificate.
    #[error("the SMTP CA file {path} holds no usable certificate")]
    NoCertificate {
        /// The file.
        path: String,
    },
    /// `smtp.host` is not a name or address TLS can verify.
    #[error("platform.smtp_host is not a valid TLS server name")]
    ServerName,
    /// The TLS client could not be built.
    #[error("cannot build the SMTP TLS client: {0}")]
    Tls(String),
    /// A setting is missing or invalid; [`MailConfigError::Setting`] reports it first, so this
    /// does not happen.
    #[error("the SMTP settings are incomplete")]
    Incomplete,
}

/// Why a message was not sent. The text names the step and codes only.
#[derive(Debug, thiserror::Error)]
pub enum SendError {
    /// The TCP connection failed.
    #[error("cannot connect to the SMTP relay: {0}")]
    Connect(std::io::Error),
    /// Reading or writing failed.
    #[error("the SMTP connection failed during {step}: {kind}")]
    Io {
        /// The step.
        step: &'static str,
        /// The I/O error kind.
        kind: std::io::ErrorKind,
    },
    /// The TLS handshake failed (for example an untrusted or mismatched certificate).
    #[error("TLS with the SMTP relay failed: {0}")]
    Tls(String),
    /// A step took longer than its timeout.
    #[error("the SMTP relay did not answer in time during {0}")]
    TimedOut(&'static str),
    /// The relay closed the connection.
    #[error("the SMTP relay closed the connection during {0}")]
    Closed(&'static str),
    /// The relay's reply is not SMTP, too long, or came when none was expected.
    #[error("the SMTP relay broke the protocol during {0}")]
    Protocol(&'static str),
    /// The relay refused a step.
    #[error("the SMTP relay refused {step} with {code}{}", enhanced_suffix(.enhanced.as_deref()))]
    Refused {
        /// The step.
        step: &'static str,
        /// The reply code.
        code: u16,
        /// The enhanced status code (RFC 3463), when the reply has one.
        enhanced: Option<String>,
    },
    /// `starttls` is configured but the relay does not offer it.
    #[error("the SMTP relay does not offer STARTTLS; plain SMTP is never used")]
    NoStartTls,
    /// Credentials are configured but the relay does not offer `AUTH PLAIN`.
    #[error("the SMTP relay does not offer AUTH PLAIN")]
    NoAuthPlain,
    /// The recipient is not an address [`is_address`] accepts.
    #[error("the recipient is not a valid address")]
    Recipient,
    /// No randomness for the `Message-ID` or the MIME boundary.
    #[error("no randomness for the Message-ID or the MIME boundary")]
    Random,
}

/// ` (5.1.1)` after a refusal's code, when the reply had an enhanced status code.
fn enhanced_suffix(enhanced: Option<&str>) -> String {
    enhanced
        .map(|code| format!(" ({code})"))
        .unwrap_or_default()
}

/// Sends through the configured relay. Cheap to clone.
#[derive(Clone)]
pub struct Mailer {
    inner: Arc<Relay>,
}

struct Relay {
    host: String,
    port: u16,
    security: SmtpSecurity,
    from: Mailbox,
    credentials: Option<(String, SmtpPassword)>,
    tls: TlsConnector,
    server_name: ServerName<'static>,
}

impl fmt::Debug for Mailer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Mailer")
            .field("port", &self.inner.port)
            .field("security", &self.inner.security)
            .finish_non_exhaustive()
    }
}

impl Mailer {
    /// The relay of the platform settings, or `None` when `smtp_host` is not set. Checks the
    /// other settings and reads the CA file now.
    ///
    /// # Errors
    ///
    /// When a setting is missing or invalid, or the CA file cannot be used.
    pub fn from_platform(platform: &Platform) -> Result<Option<Self>, MailConfigError> {
        let Some(host) = platform.smtp_host.clone() else {
            return Ok(None);
        };
        platform
            .validate_smtp()
            .map_err(|invalid| MailConfigError::Setting {
                key: invalid.key,
                message: invalid.message,
            })?;
        let (Some(port), Some(security), Some(from)) = (
            platform.smtp_port,
            platform.smtp_security,
            platform.smtp_from.as_deref().and_then(Mailbox::parse),
        ) else {
            return Err(MailConfigError::Incomplete);
        };
        let credentials = match (&platform.smtp_username, &platform.smtp_password) {
            (Some(username), Some(password)) => Some((username.clone(), password.clone())),
            (None, None) => None,
            _ => return Err(MailConfigError::Incomplete),
        };
        let ca_file = platform
            .smtp_ca_file
            .clone()
            .unwrap_or_else(|| PathBuf::from(SYSTEM_CA_FILE));
        let tls = connector(&ca_file)?;
        let server_name =
            ServerName::try_from(host.clone()).map_err(|_| MailConfigError::ServerName)?;
        Ok(Some(Self {
            inner: Arc::new(Relay {
                host,
                port,
                security,
                from,
                credentials,
                tls,
                server_name,
            }),
        }))
    }

    /// Delivers `message`: one SMTP session from connecting to `QUIT`.
    ///
    /// # Errors
    ///
    /// When any step fails; the message may then or may not have been delivered only when the
    /// failure came after the relay's answer to the end of the data, which is not reported as a
    /// failure.
    pub async fn send(&self, message: &Message) -> Result<(), SendError> {
        if !is_address(&message.to) {
            return Err(SendError::Recipient);
        }
        let relay = &self.inner;
        let date = chrono::Utc::now().to_rfc2822();
        let data = render(&relay.from, message, &date, &message_id()?, &boundary()?);
        let tcp = tokio::time::timeout(
            CONNECT_TIMEOUT,
            TcpStream::connect((relay.host.as_str(), relay.port)),
        )
        .await
        .map_err(|_| SendError::TimedOut("connect"))?
        .map_err(SendError::Connect)?;
        let hello = hello_name(&tcp);
        match relay.security {
            SmtpSecurity::Tls => {
                let tls = relay.handshake(tcp).await?;
                let mut connection = Connection::new(tls);
                connection.expect("greeting", &[220]).await?;
                let ehlo = connection
                    .command(&format!("EHLO {hello}"), "EHLO", &[250])
                    .await?;
                relay
                    .transact(&mut connection, &ehlo, &message.to, &data)
                    .await
            }
            SmtpSecurity::Starttls => {
                let mut connection = Connection::new(tcp);
                connection.expect("greeting", &[220]).await?;
                let ehlo = connection
                    .command(&format!("EHLO {hello}"), "EHLO", &[250])
                    .await?;
                if !ehlo.offers("STARTTLS") {
                    return Err(SendError::NoStartTls);
                }
                connection.command("STARTTLS", "STARTTLS", &[220]).await?;
                let tcp = connection.into_plain()?;
                let tls = relay.handshake(tcp).await?;
                let mut connection = Connection::new(tls);
                // RFC 3207: the client forgets what it learned before and says hello again.
                let ehlo = connection
                    .command(&format!("EHLO {hello}"), "EHLO", &[250])
                    .await?;
                relay
                    .transact(&mut connection, &ehlo, &message.to, &data)
                    .await
            }
        }
    }
}

impl Relay {
    async fn handshake(
        &self,
        tcp: TcpStream,
    ) -> Result<tokio_rustls::client::TlsStream<TcpStream>, SendError> {
        tokio::time::timeout(
            CONNECT_TIMEOUT,
            self.tls.connect(self.server_name.clone(), tcp),
        )
        .await
        .map_err(|_| SendError::TimedOut("the TLS handshake"))?
        .map_err(|error| SendError::Tls(error.to_string()))
    }

    /// Authentication and the mail transaction, over TLS.
    async fn transact<S: AsyncRead + AsyncWrite + Unpin>(
        &self,
        connection: &mut Connection<S>,
        ehlo: &Reply,
        recipient: &str,
        data: &[u8],
    ) -> Result<(), SendError> {
        if let Some((username, password)) = &self.credentials {
            if !ehlo.offers_auth("PLAIN") {
                return Err(SendError::NoAuthPlain);
            }
            let plain = Zeroizing::new(format!("\0{username}\0{}", password.expose()));
            let command =
                Zeroizing::new(format!("AUTH PLAIN {}", STANDARD.encode(plain.as_bytes())));
            connection.command(&command, "AUTH", &[235]).await?;
        }
        connection
            .command(
                &format!("MAIL FROM:<{}>", self.from.address()),
                "MAIL FROM",
                &[250],
            )
            .await?;
        connection
            .command(&format!("RCPT TO:<{recipient}>"), "RCPT TO", &[250, 251])
            .await?;
        connection.command("DATA", "DATA", &[354]).await?;
        connection.write(data, "the message").await?;
        connection.write(b".\r\n", "the message").await?;
        connection.expect("the end of the message", &[250]).await?;
        // Delivered. A failing QUIT changes nothing.
        let _quit = connection.command("QUIT", "QUIT", &[221]).await;
        Ok(())
    }
}

/// A TLS client that trusts only the certificates in `ca_file`.
fn connector(ca_file: &Path) -> Result<TlsConnector, MailConfigError> {
    let path = ca_file.display().to_string();
    let certificates: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(ca_file)
        .and_then(Iterator::collect)
        .map_err(|error| MailConfigError::CaFile {
            path: path.clone(),
            message: error.to_string(),
        })?;
    let mut roots = RootCertStore::empty();
    let (added, _ignored) = roots.add_parsable_certificates(certificates);
    if added == 0 {
        return Err(MailConfigError::NoCertificate { path });
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|error| MailConfigError::Tls(error.to_string()))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(TlsConnector::from(Arc::new(config)))
}

/// The `EHLO` argument: the local address as an address literal (RFC 5321 section 4.1.3), since
/// a pod has no name the relay could look up.
fn hello_name(tcp: &TcpStream) -> String {
    match tcp.local_addr().map(|address| address.ip()) {
        Ok(IpAddr::V4(ip)) => format!("[{ip}]"),
        Ok(IpAddr::V6(ip)) => format!("[IPv6:{ip}]"),
        Err(_) => "[127.0.0.1]".to_owned(),
    }
}

/// 128 random bits in hex.
fn random_hex() -> Result<String, SendError> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(|_| SendError::Random)?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// A random `Message-ID` local part: 128 bits in hex.
fn message_id() -> Result<String, SendError> {
    random_hex()
}

/// A random MIME boundary: [`BOUNDARY_PREFIX`] and 128 bits in hex, 43 characters (RFC 2046
/// allows 70).
fn boundary() -> Result<String, SendError> {
    Ok(format!("{BOUNDARY_PREFIX}{}", random_hex()?))
}

/// One reply: its code and the text of each line (kept only to read `EHLO` extensions; never
/// logged).
#[derive(Debug)]
struct Reply {
    code: u16,
    lines: Vec<String>,
}

impl Reply {
    /// Whether an `EHLO` reply lists `keyword` (any case) as an extension.
    fn offers(&self, keyword: &str) -> bool {
        self.lines.iter().skip(1).any(|line| {
            line.split_ascii_whitespace()
                .next()
                .is_some_and(|word| word.eq_ignore_ascii_case(keyword))
        })
    }

    /// Whether an `EHLO` reply lists `mechanism` under `AUTH`.
    fn offers_auth(&self, mechanism: &str) -> bool {
        self.lines.iter().skip(1).any(|line| {
            let mut words = line.split_ascii_whitespace();
            words
                .next()
                .is_some_and(|word| word.eq_ignore_ascii_case("AUTH"))
                && words.any(|word| word.eq_ignore_ascii_case(mechanism))
        })
    }

    /// The RFC 3463 enhanced status code at the start of the first line, when there is one:
    /// digits and dots only, so it may be logged.
    fn enhanced(&self) -> Option<String> {
        let first = self.lines.first()?.split_ascii_whitespace().next()?;
        let parts: Vec<&str> = first.split('.').collect();
        let valid = parts.len() == 3
            && parts.iter().all(|part| {
                (1..=3).contains(&part.len()) && part.bytes().all(|byte| byte.is_ascii_digit())
            });
        valid.then(|| first.to_owned())
    }
}

/// One SMTP connection, plain or TLS.
struct Connection<S> {
    stream: BufReader<S>,
}

impl<S: AsyncRead + AsyncWrite + Unpin> Connection<S> {
    fn new(stream: S) -> Self {
        Self {
            stream: BufReader::new(stream),
        }
    }

    /// The plain connection again, to start TLS on it. Refused when the relay already sent
    /// more than the reply to `STARTTLS`: those bytes would otherwise be read as if they came
    /// over TLS.
    fn into_plain(self) -> Result<S, SendError> {
        if !self.stream.buffer().is_empty() {
            return Err(SendError::Protocol("STARTTLS"));
        }
        Ok(self.stream.into_inner())
    }

    async fn write(&mut self, bytes: &[u8], step: &'static str) -> Result<(), SendError> {
        let io = |error: std::io::Error| SendError::Io {
            step,
            kind: error.kind(),
        };
        let stream = self.stream.get_mut();
        stream.write_all(bytes).await.map_err(io)?;
        stream.flush().await.map_err(io)
    }

    /// Sends `line` and CRLF, then reads the reply, which must have one of the `expected`
    /// codes.
    async fn command(
        &mut self,
        line: &str,
        step: &'static str,
        expected: &[u16],
    ) -> Result<Reply, SendError> {
        let mut bytes = Zeroizing::new(Vec::with_capacity(line.len() + 2));
        bytes.extend_from_slice(line.as_bytes());
        bytes.extend_from_slice(b"\r\n");
        self.write(&bytes, step).await?;
        self.expect(step, expected).await
    }

    /// Reads a reply, which must have one of the `expected` codes.
    async fn expect(&mut self, step: &'static str, expected: &[u16]) -> Result<Reply, SendError> {
        let reply = tokio::time::timeout(REPLY_TIMEOUT, self.reply(step))
            .await
            .map_err(|_| SendError::TimedOut(step))??;
        if expected.contains(&reply.code) {
            Ok(reply)
        } else {
            Err(SendError::Refused {
                step,
                code: reply.code,
                enhanced: reply.enhanced(),
            })
        }
    }

    /// Reads one reply: lines `ddd-text` up to one `ddd text` (or `ddd`), all with the same
    /// code.
    async fn reply(&mut self, step: &'static str) -> Result<Reply, SendError> {
        let mut code = None;
        let mut lines = Vec::new();
        loop {
            if lines.len() == MAX_REPLY_LINES {
                return Err(SendError::Protocol(step));
            }
            let mut line = Vec::new();
            let limit = u64::try_from(MAX_REPLY_LINE).unwrap_or(u64::MAX);
            let read = (&mut self.stream)
                .take(limit)
                .read_until(b'\n', &mut line)
                .await
                .map_err(|error| SendError::Io {
                    step,
                    kind: error.kind(),
                })?;
            if read == 0 {
                return Err(SendError::Closed(step));
            }
            let Some(line) = line.strip_suffix(b"\n") else {
                return Err(SendError::Protocol(step));
            };
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            let (this_code, last, text) =
                parse_reply_line(line).ok_or(SendError::Protocol(step))?;
            if *code.get_or_insert(this_code) != this_code {
                return Err(SendError::Protocol(step));
            }
            lines.push(text);
            if last {
                return Ok(Reply {
                    code: this_code,
                    lines,
                });
            }
        }
    }
}

/// One reply line: the three-digit code, whether it is the last line, and the text.
fn parse_reply_line(line: &[u8]) -> Option<(u16, bool, String)> {
    let digits = line.get(..3)?;
    if !digits.iter().all(u8::is_ascii_digit) || !(b'2'..=b'5').contains(&digits[0]) {
        return None;
    }
    let code = std::str::from_utf8(digits).ok()?.parse().ok()?;
    let (last, text) = match line.get(3) {
        None => (true, &[][..]),
        Some(b' ') => (true, line.get(4..).unwrap_or_default()),
        Some(b'-') => (false, line.get(4..).unwrap_or_default()),
        Some(_) => return None,
    };
    Some((code, last, String::from_utf8_lossy(text).into_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    // The address rules themselves are tested in src/email.rs.

    #[test]
    fn the_sender_is_read_in_its_usual_forms() {
        let named = |name: &str| Mailbox {
            name: Some(name.to_owned()),
            address: "noreply@example.com".to_owned(),
        };
        assert_eq!(
            Mailbox::parse("TrustIoT <noreply@example.com>"),
            Some(named("TrustIoT"))
        );
        assert_eq!(
            Mailbox::parse("\"TrustIoT Alerts\" <noreply@example.com>"),
            Some(named("TrustIoT Alerts"))
        );
        assert_eq!(
            Mailbox::parse(" 智慧工廠 <noreply@example.com> "),
            Some(named("智慧工廠"))
        );
        let bare = Mailbox {
            name: None,
            address: "noreply@example.com".to_owned(),
        };
        assert_eq!(Mailbox::parse("noreply@example.com"), Some(bare.clone()));
        assert_eq!(Mailbox::parse("<noreply@example.com>"), Some(bare));
        for invalid in [
            "",
            "TrustIoT",
            "TrustIoT <noreply@example.com",
            "TrustIoT noreply@example.com>",
            "TrustIoT <not an address>",
            "Trust\rIoT <noreply@example.com>",
            "TrustIoT <noreply@example.com>\nBcc: x@example.org",
            "TrustIoT <noreply@example.com> extra",
        ] {
            assert_eq!(Mailbox::parse(invalid), None, "{invalid:?}");
        }
    }

    #[test]
    fn sender_names_are_quoted_or_encoded() {
        let header = |text: &str| Mailbox::parse(text).expect("a sender").header();
        assert_eq!(
            header("TrustIoT <noreply@example.com>"),
            "\"TrustIoT\" <noreply@example.com>"
        );
        assert_eq!(header("noreply@example.com"), "<noreply@example.com>");
        // A quote inside the name, or non-ASCII, is encoded rather than escaped.
        let encoded = header("智慧工廠 <noreply@example.com>");
        assert_eq!(
            encoded,
            format!(
                "=?UTF-8?B?{}?= <noreply@example.com>",
                STANDARD.encode("智慧工廠")
            )
        );
    }

    #[test]
    fn long_text_becomes_several_encoded_words_split_at_characters() {
        let text = "登入驗證碼".repeat(10);
        let encoded = encoded_words(&text);
        let words: Vec<&str> = encoded.split("\r\n ").collect();
        assert!(words.len() > 1, "{encoded}");
        let mut decoded = Vec::new();
        for word in &words {
            assert!(word.len() <= 75, "{word} is longer than RFC 2047 allows");
            let inner = word
                .strip_prefix("=?UTF-8?B?")
                .and_then(|word| word.strip_suffix("?="))
                .expect("an encoded word");
            let bytes = STANDARD.decode(inner).expect("base64");
            // Each word is whole characters on its own.
            assert!(String::from_utf8(bytes.clone()).is_ok(), "{word}");
            decoded.extend(bytes);
        }
        assert_eq!(String::from_utf8(decoded).expect("utf-8"), text);
        assert_eq!(encoded_words(""), "");
    }

    /// Splits a rendered body at `boundary`: each part's headers and decoded content, after
    /// checking the delimiters, the closing delimiter and the base64 line rules.
    fn parts(body: &str, boundary: &str) -> Vec<(String, String)> {
        let delimiter = format!("--{boundary}\r\n");
        let close = format!("--{boundary}--\r\n");
        assert!(
            body.ends_with(&close),
            "the closing delimiter ends the body"
        );
        let inner = body.strip_suffix(&close).expect("the closing delimiter");
        assert!(
            inner.starts_with(&delimiter),
            "no preamble before the first part"
        );
        inner
            .split(&delimiter)
            .skip(1)
            .map(|part| {
                let (head, encoded) = part.split_once("\r\n\r\n").expect("part headers");
                for line in encoded.split("\r\n").filter(|line| !line.is_empty()) {
                    assert!(line.len() <= BODY_LINE, "{line}");
                    assert!(
                        line.bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || b"+/=".contains(&byte)),
                        "only base64 in a part: {line}"
                    );
                }
                let decoded = STANDARD
                    .decode(encoded.replace("\r\n", ""))
                    .expect("base64");
                (head.to_owned(), String::from_utf8(decoded).expect("utf-8"))
            })
            .collect()
    }

    #[test]
    fn a_rendered_message_is_multipart_alternative_with_base64_parts() {
        let from = Mailbox::parse("TrustIoT <noreply@example.com>").expect("a sender");
        let plain = "第一行\n.second line starts with a dot\nthird\n".repeat(20);
        let html = "<!DOCTYPE html>\n<p>段落</p>\n.dot\n".repeat(30);
        let message = Message::new(
            "user@example.org".to_owned(),
            "TrustIoT 登入驗證碼".to_owned(),
            Zeroizing::new(plain.clone()),
            Zeroizing::new(html.clone()),
        );
        let boundary = format!("{BOUNDARY_PREFIX}0123abcd");
        let data = render(
            &from,
            &message,
            "Wed, 21 Oct 2026 08:00:00 +0000",
            "abc",
            &boundary,
        );
        let text = String::from_utf8(data.to_vec()).expect("ascii");
        assert!(text.is_ascii());
        assert!(text.ends_with("\r\n"));
        assert!(!text.replace("\r\n", "").contains('\n'), "a bare LF");
        assert!(!text.replace("\r\n", "").contains('\r'), "a bare CR");
        let (headers, body) = text.split_once("\r\n\r\n").expect("a blank line");
        for expected in [
            "Date: Wed, 21 Oct 2026 08:00:00 +0000",
            "From: \"TrustIoT\" <noreply@example.com>",
            "To: <user@example.org>",
            "Message-ID: <abc@example.com>",
            "MIME-Version: 1.0",
            "Content-Type: multipart/alternative; boundary=\"=_trustiot_0123abcd\"",
            "Auto-Submitted: auto-generated",
        ] {
            assert!(headers.lines().any(|line| line == expected), "{expected}");
        }
        assert!(!headers.contains("登入"), "the subject is encoded");
        assert!(
            !headers.contains("Content-Transfer-Encoding"),
            "a multipart message has no transfer encoding of its own"
        );
        for line in body.split("\r\n") {
            assert!(!line.starts_with('.'), "{line}");
        }
        let parts = parts(body, &boundary);
        let [(plain_head, plain_text), (html_head, html_text)] = parts.as_slice() else {
            panic!("two parts: {parts:?}");
        };
        // Plain text first, HTML last: a client shows the last part it can display.
        assert_eq!(
            plain_head,
            "Content-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: base64"
        );
        assert_eq!(
            html_head,
            "Content-Type: text/html; charset=utf-8\r\nContent-Transfer-Encoding: base64"
        );
        assert_eq!(plain_text, &plain.replace('\n', "\r\n"));
        assert_eq!(html_text, &html.replace('\n', "\r\n"));
    }

    #[test]
    fn boundaries_are_random_and_cannot_occur_in_a_part() {
        let first = boundary().expect("randomness");
        let second = boundary().expect("randomness");
        assert_ne!(first, second);
        for value in [&first, &second] {
            assert!(value.starts_with(BOUNDARY_PREFIX), "{value}");
            assert_eq!(value.len(), BOUNDARY_PREFIX.len() + 32, "{value}");
            // RFC 2046: at most 70 characters, from the allowed set.
            assert!(value.len() <= 70);
            assert!(
                value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"=_".contains(&byte)),
                "{value}"
            );
        }
        // `_` is outside the base64 alphabet, so a base64 line never holds a boundary.
        assert!(!STANDARD.encode([0xff_u8; 300]).contains('_'));
        assert!(BOUNDARY_PREFIX.contains('_'));

        // In a message whose content holds the boundary-like text, the boundary is found only
        // in the header and the three delimiter lines.
        let from = Mailbox::parse("noreply@example.com").expect("a sender");
        let hostile = format!("--{first}\n--{first}--\n");
        let message = Message::new(
            "user@example.org".to_owned(),
            "subject".to_owned(),
            Zeroizing::new(hostile.clone()),
            Zeroizing::new(hostile),
        );
        let data = render(&from, &message, "date", "id", &first);
        let text = String::from_utf8(data.to_vec()).expect("ascii");
        assert_eq!(text.matches(first.as_str()).count(), 4, "{text}");
    }

    /// The bodies hold the code, so no buffer they pass through may grow: a reallocation frees
    /// the old buffer without wiping it. `render`, `crlf` and `dot_stuff` assert that their
    /// buffer kept its capacity (debug builds, which tests are); this test feeds them the worst
    /// cases of every bound, and the real login email.
    #[test]
    fn rendering_never_grows_a_buffer_that_holds_the_body() {
        // One pass gives what replacing CRLF with LF, then LF with CRLF, gave.
        for (content, expected) in [
            ("", ""),
            ("a\nb\n", "a\r\nb\r\n"),
            ("a\r\nb", "a\r\nb"),
            ("\r\r\n\n", "\r\r\n\r\n"),
            ("\r", "\r"),
            ("é\n驗證碼\n", "é\r\n驗證碼\r\n"),
        ] {
            assert_eq!(crlf(content).as_str(), expected, "{content:?}");
            assert_eq!(
                content.replace("\r\n", "\n").replace('\n', "\r\n"),
                expected
            );
        }
        let from = Mailbox::parse("TrustIoT <noreply@example.com>").expect("a sender");
        let login = crate::login_email::code_message(
            &crate::login::EmailAddress::parse("user@example.com").expect("an address"),
            &crate::login::LoginCode::parse("908172").expect("a code"),
        );
        let mut cases = vec![
            ("\n".repeat(1000), "\n".repeat(999)),
            (String::new(), String::new()),
            (".\n".repeat(500), "\r\n".repeat(301)),
            ("x".to_owned(), "é".repeat(77)),
            (login.text().to_owned(), login.html().to_owned()),
        ];
        cases.extend((0..6).map(|length| ("\n".repeat(length), ".".repeat(length))));
        for (plain, html) in cases {
            for content in [&plain, &html] {
                let mut part = String::new();
                push_base64_part(&mut part, content);
                assert!(
                    part.len() <= base64_part_len_bound(content),
                    "{} > {} for {content:?}",
                    part.len(),
                    base64_part_len_bound(content)
                );
            }
            let message = Message::new(
                "user@example.org".to_owned(),
                "TrustIoT 登入驗證碼".to_owned(),
                Zeroizing::new(plain),
                Zeroizing::new(html),
            );
            let data = render(
                &from,
                &message,
                "Wed, 21 Oct 2026 08:00:00 +0000",
                "abc",
                &format!("{BOUNDARY_PREFIX}0123abcd"),
            );
            assert!(data.ends_with(b"--\r\n"));
        }
    }

    #[test]
    fn lines_that_start_with_a_dot_are_doubled() {
        assert_eq!(
            dot_stuff(".\r\nabc\r\n..x\r\ny.\r\n").as_slice(),
            b"..\r\nabc\r\n...x\r\ny.\r\n"
        );
    }

    #[test]
    fn reply_lines_are_parsed_strictly() {
        assert_eq!(
            parse_reply_line(b"250-mail.example.com"),
            Some((250, false, "mail.example.com".to_owned()))
        );
        assert_eq!(
            parse_reply_line(b"250 STARTTLS"),
            Some((250, true, "STARTTLS".to_owned()))
        );
        assert_eq!(parse_reply_line(b"354"), Some((354, true, String::new())));
        for invalid in [
            &b""[..],
            b"25",
            b"2500 x",
            b"abc ok",
            b"650 x",
            b"150 x",
            b"250_x",
        ] {
            assert_eq!(parse_reply_line(invalid), None, "{invalid:?}");
        }
    }

    #[test]
    fn ehlo_extensions_and_enhanced_codes_are_read_from_the_reply() {
        let reply = Reply {
            code: 250,
            lines: vec![
                "mail.example.com greets you".to_owned(),
                "PIPELINING".to_owned(),
                "starttls".to_owned(),
                "AUTH LOGIN PLAIN".to_owned(),
            ],
        };
        assert!(reply.offers("STARTTLS"));
        assert!(!reply.offers("SMTPUTF8"));
        assert!(reply.offers_auth("PLAIN"));
        assert!(!reply.offers_auth("CRAM-MD5"));
        // The greeting line is not an extension.
        let greeting_only = Reply {
            code: 250,
            lines: vec!["STARTTLS".to_owned()],
        };
        assert!(!greeting_only.offers("STARTTLS"));

        let refused = |text: &str| Reply {
            code: 550,
            lines: vec![text.to_owned()],
        };
        assert_eq!(
            refused("5.1.1 <user@example.org>: no such user").enhanced(),
            Some("5.1.1".to_owned())
        );
        assert_eq!(refused("no such user <user@example.org>").enhanced(), None);
        assert_eq!(refused("5.1 x").enhanced(), None);
        assert_eq!(refused("5.1.1234 x").enhanced(), None);
    }

    #[test]
    fn errors_name_the_step_and_codes_only() {
        let error = SendError::Refused {
            step: "RCPT TO",
            code: 550,
            enhanced: Some("5.1.1".to_owned()),
        };
        assert_eq!(
            error.to_string(),
            "the SMTP relay refused RCPT TO with 550 (5.1.1)"
        );
        let error = SendError::Refused {
            step: "AUTH",
            code: 535,
            enhanced: None,
        };
        assert_eq!(error.to_string(), "the SMTP relay refused AUTH with 535");
    }

    #[test]
    fn debug_output_never_shows_the_message() {
        let message = Message::new(
            "user@example.org".to_owned(),
            "subject".to_owned(),
            Zeroizing::new("code 123456".to_owned()),
            Zeroizing::new("<p>code 654321</p>".to_owned()),
        );
        let shown = format!("{message:?}");
        assert_eq!(shown, "Message(<redacted>)");
        assert!(!shown.contains("123456") && !shown.contains("654321"));
    }
}
