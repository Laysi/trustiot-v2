//! A fake SMTP relay for tests/login.rs: TLS with a certificate for 127.0.0.1 from a test CA,
//! either STARTTLS or TLS from the first byte, `AUTH PLAIN` with an account made at run time, and
//! a record of every message it accepted. It can also misbehave as a relay or someone in the
//! path might: never offer STARTTLS, or send bytes right behind its answer to STARTTLS. No real
//! mailbox is ever involved.
//!
//! It runs on a thread with its own runtime, so a test that blocks its own threads (the HTTP
//! helpers in `support` are blocking) never stalls it.

use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls;
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};

use super::postgres::TestCa;
use super::scratch_dir;

/// How the relay talks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Plain greeting, `STARTTLS`, then everything over TLS (port 587).
    StartTls,
    /// TLS from the first byte (port 465).
    Tls,
    /// A plain connection that never offers `STARTTLS`, as when someone in the path strips it.
    /// Every mail command is answered 530 and recorded.
    NoStartTls,
    /// Offers `STARTTLS`, but sends a second reply right behind its 220 in the same write: the
    /// STARTTLS command injection of CVE-2011-0411 seen from the client.
    InjectAfterStartTls,
}

impl Mode {
    /// The value of `TRUSTIOT_SMTP_SECURITY` for this relay.
    pub const fn security(self) -> &'static str {
        match self {
            Self::Tls => "tls",
            Self::StartTls | Self::NoStartTls | Self::InjectAfterStartTls => "starttls",
        }
    }
}

/// One message the relay accepted.
#[derive(Clone, Debug)]
pub struct Delivery {
    /// The decoded `AUTH PLAIN` response (`\0user\0password`), when the client logged in.
    pub auth: Option<String>,
    /// The `MAIL FROM` address.
    pub mail_from: String,
    /// The `RCPT TO` addresses.
    pub rcpt_to: Vec<String>,
    /// The message as sent after `DATA`, with dot-stuffing undone and without the final `.`.
    pub data: String,
}

impl Delivery {
    /// The value of the first header called `name` (any case), unfolded.
    pub fn header(&self, name: &str) -> Option<String> {
        let (head, _) = self.data.split_once("\r\n\r\n")?;
        let mut value: Option<String> = None;
        for line in head.split("\r\n") {
            if let Some(current) = value.as_mut() {
                if line.starts_with([' ', '\t']) {
                    current.push_str(line);
                    continue;
                }
                break;
            }
            if let Some((key, rest)) = line.split_once(':')
                && key.eq_ignore_ascii_case(name)
            {
                value = Some(rest.trim().to_owned());
            }
        }
        value
    }

    /// The body, decoded from base64 (the service sends text/plain in base64).
    pub fn body_text(&self) -> String {
        let (_, body) = self.data.split_once("\r\n\r\n").expect("a message body");
        let joined: String = body.split("\r\n").collect();
        let bytes = STANDARD.decode(joined.trim()).expect("a base64 body");
        String::from_utf8(bytes).expect("a UTF-8 body")
    }
}

#[derive(Default)]
struct Record {
    deliveries: Vec<Delivery>,
    /// Every command line received before TLS.
    plain_commands: Vec<String>,
    /// Connections accepted.
    connections: usize,
}

type Shared = Arc<(Mutex<Record>, Condvar)>;

/// The account the relay accepts.
#[derive(Clone)]
pub struct Account {
    pub username: String,
    pub password: String,
}

impl Account {
    /// A throwaway account with a random password, made at run time.
    pub fn generate() -> Self {
        let mut bytes = [0_u8; 16];
        getrandom::fill(&mut bytes).expect("operating system randomness");
        let password: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
        Self {
            username: "login-test-relay".to_owned(),
            password,
        }
    }
}

/// The running relay.
pub struct FakeSmtp {
    pub port: u16,
    pub mode: Mode,
    pub account: Account,
    /// The test CA in PEM, for `TRUSTIOT_SMTP_CA_FILE`.
    pub ca_file: PathBuf,
    shared: Shared,
}

impl FakeSmtp {
    /// Starts a relay on a free local port.
    pub fn start(mode: Mode) -> Self {
        let ca = TestCa::new("identity-login-test-smtp-ca");
        let dir = scratch_dir("smtp");
        let ca_file = dir.join("smtp-ca.pem");
        std::fs::write(&ca_file, &ca.pem).expect("write the relay's CA file");
        let (certificate, key) = ca.server_certificate(&["127.0.0.1"]);
        let certificates =
            vec![CertificateDer::from_pem_slice(certificate.as_bytes()).expect("certificate")];
        let key = PrivateKeyDer::from_pem_slice(key.as_bytes()).expect("key");
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .expect("protocol versions")
        .with_no_client_auth()
        .with_single_cert(certificates, key)
        .expect("server config");
        let acceptor = TlsAcceptor::from(Arc::new(config));
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind the relay");
        listener
            .set_nonblocking(true)
            .expect("non-blocking listener");
        let port = listener.local_addr().expect("relay address").port();
        let account = Account::generate();
        let shared: Shared = Arc::default();
        let (state, expected) = (Arc::clone(&shared), account.clone());
        thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("the relay's runtime");
            runtime.block_on(async move {
                let listener = TcpListener::from_std(listener).expect("tokio listener");
                loop {
                    let Ok((stream, _)) = listener.accept().await else {
                        continue;
                    };
                    state.0.lock().expect("relay lock").connections += 1;
                    let (acceptor, state, expected) =
                        (acceptor.clone(), Arc::clone(&state), expected.clone());
                    tokio::spawn(async move {
                        let _ = serve(stream, mode, &acceptor, &state, &expected).await;
                    });
                }
            });
        });
        Self {
            port,
            mode,
            account,
            ca_file,
            shared,
        }
    }

    /// The platform settings that point the identity service at this relay, as the chart sets
    /// them, with `TRUSTIOT_SMTP_CA_FILE` for the test CA.
    pub fn env(&self) -> Vec<(String, String)> {
        vec![
            ("TRUSTIOT_SMTP_HOST".into(), "127.0.0.1".into()),
            ("TRUSTIOT_SMTP_PORT".into(), self.port.to_string()),
            ("TRUSTIOT_SMTP_SECURITY".into(), self.mode.security().into()),
            (
                "TRUSTIOT_SMTP_FROM".into(),
                "TrustIoT <noreply@example.com>".into(),
            ),
            (
                "TRUSTIOT_SMTP_CA_FILE".into(),
                self.ca_file.display().to_string(),
            ),
            (
                "TRUSTIOT_SMTP_USERNAME".into(),
                self.account.username.clone(),
            ),
            (
                "TRUSTIOT_SMTP_PASSWORD".into(),
                self.account.password.clone(),
            ),
        ]
    }

    /// Every message accepted so far.
    pub fn deliveries(&self) -> Vec<Delivery> {
        self.shared.0.lock().expect("relay lock").deliveries.clone()
    }

    /// Every command line received before TLS.
    pub fn plain_commands(&self) -> Vec<String> {
        self.shared
            .0
            .lock()
            .expect("relay lock")
            .plain_commands
            .clone()
    }

    /// Connections accepted so far.
    pub fn connections(&self) -> usize {
        self.shared.0.lock().expect("relay lock").connections
    }

    /// Waits until at least `count` messages have been accepted and returns them all.
    pub fn wait_for_deliveries(&self, count: usize, timeout: Duration) -> Vec<Delivery> {
        let deadline = Instant::now() + timeout;
        let (lock, changed) = &*self.shared;
        let mut record = lock.lock().expect("relay lock");
        while record.deliveries.len() < count {
            let now = Instant::now();
            assert!(
                now < deadline,
                "the relay received {} of {count} messages within {timeout:?}",
                record.deliveries.len()
            );
            record = changed
                .wait_timeout(record, deadline - now)
                .expect("relay lock")
                .0;
        }
        record.deliveries.clone()
    }
}

/// The relay's longest line, as RFC 5321 allows for text lines.
const MAX_LINE: u64 = 1000;

/// One line without its CRLF; `None` at the end of the stream.
async fn read_line<S: AsyncRead + Unpin>(stream: &mut BufReader<S>) -> io::Result<Option<String>> {
    let mut line = Vec::new();
    let read = (&mut *stream)
        .take(MAX_LINE)
        .read_until(b'\n', &mut line)
        .await?;
    if read == 0 {
        return Ok(None);
    }
    while line
        .last()
        .is_some_and(|byte| *byte == b'\n' || *byte == b'\r')
    {
        line.pop();
    }
    Ok(Some(String::from_utf8_lossy(&line).into_owned()))
}

async fn write<S: AsyncWrite + Unpin>(stream: &mut S, text: &str) -> io::Result<()> {
    stream.write_all(text.as_bytes()).await?;
    stream.flush().await
}

/// The plain part of a connection, and then the TLS session.
async fn serve(
    stream: TcpStream,
    mode: Mode,
    acceptor: &TlsAcceptor,
    shared: &Shared,
    account: &Account,
) -> io::Result<()> {
    if mode == Mode::Tls {
        let tls = acceptor.accept(stream).await?;
        return session(tls, true, shared, account).await;
    }
    let mut plain = BufReader::new(stream);
    write(
        plain.get_mut(),
        "220 relay.example.com ESMTP fake relay\r\n",
    )
    .await?;
    loop {
        let Some(line) = read_line(&mut plain).await? else {
            return Ok(());
        };
        shared
            .0
            .lock()
            .expect("relay lock")
            .plain_commands
            .push(line.clone());
        let verb = line.to_ascii_uppercase();
        if verb.starts_with("EHLO ") {
            let starttls = if mode == Mode::NoStartTls {
                ""
            } else {
                "250-STARTTLS\r\n"
            };
            write(
                plain.get_mut(),
                &format!("250-relay.example.com\r\n{starttls}250 8BITMIME\r\n"),
            )
            .await?;
        } else if verb == "STARTTLS" && mode != Mode::NoStartTls {
            if mode == Mode::InjectAfterStartTls {
                write(
                    plain.get_mut(),
                    "220 2.0.0 Ready to start TLS\r\n250-relay.example.com\r\n250 AUTH PLAIN\r\n",
                )
                .await?;
            } else {
                write(plain.get_mut(), "220 2.0.0 Ready to start TLS\r\n").await?;
            }
            let tls = acceptor.accept(plain.into_inner()).await?;
            return session(tls, false, shared, account).await;
        } else if verb == "QUIT" {
            write(plain.get_mut(), "221 2.0.0 Bye\r\n").await?;
            return Ok(());
        } else {
            write(
                plain.get_mut(),
                "530 5.7.0 Must issue a STARTTLS command first\r\n",
            )
            .await?;
        }
    }
}

/// The session over TLS: `EHLO`, `AUTH PLAIN`, one or more transactions, `QUIT`.
async fn session<S: AsyncRead + AsyncWrite + Unpin>(
    stream: S,
    greet: bool,
    shared: &Shared,
    account: &Account,
) -> io::Result<()> {
    let mut stream = BufReader::new(stream);
    if greet {
        write(
            stream.get_mut(),
            "220 relay.example.com ESMTP fake relay\r\n",
        )
        .await?;
    }
    let expected = format!("\0{}\0{}", account.username, account.password);
    let mut auth: Option<String> = None;
    let mut mail_from: Option<String> = None;
    let mut rcpt_to = Vec::new();
    loop {
        let Some(line) = read_line(&mut stream).await? else {
            return Ok(());
        };
        let verb = line.to_ascii_uppercase();
        let reply = if verb.starts_with("EHLO ") {
            "250-relay.example.com\r\n250-AUTH LOGIN PLAIN\r\n250 8BITMIME\r\n".to_owned()
        } else if let Some(response) = line
            .get(..11)
            .filter(|start| start.eq_ignore_ascii_case("AUTH PLAIN "))
            .and_then(|_| line.get(11..))
        {
            let decoded = STANDARD
                .decode(response.trim())
                .ok()
                .and_then(|bytes| String::from_utf8(bytes).ok());
            if decoded.as_deref() == Some(expected.as_str()) {
                auth = decoded;
                "235 2.7.0 Authentication successful\r\n".to_owned()
            } else {
                "535 5.7.8 Authentication credentials invalid\r\n".to_owned()
            }
        } else if verb.starts_with("MAIL FROM:") {
            if auth.is_none() {
                "530 5.7.0 Authentication required\r\n".to_owned()
            } else {
                mail_from = Some(angle_address(&line));
                rcpt_to.clear();
                "250 2.1.0 Ok\r\n".to_owned()
            }
        } else if verb.starts_with("RCPT TO:") {
            if mail_from.is_none() {
                "503 5.5.1 Need MAIL first\r\n".to_owned()
            } else {
                rcpt_to.push(angle_address(&line));
                "250 2.1.5 Ok\r\n".to_owned()
            }
        } else if verb == "DATA" {
            if rcpt_to.is_empty() {
                "503 5.5.1 Need RCPT first\r\n".to_owned()
            } else {
                write(stream.get_mut(), "354 End data with <CR><LF>.<CR><LF>\r\n").await?;
                let data = read_data(&mut stream).await?;
                let delivery = Delivery {
                    auth: auth.clone(),
                    mail_from: mail_from.take().unwrap_or_default(),
                    rcpt_to: std::mem::take(&mut rcpt_to),
                    data,
                };
                let (lock, changed) = &**shared;
                lock.lock().expect("relay lock").deliveries.push(delivery);
                changed.notify_all();
                "250 2.0.0 Ok: queued\r\n".to_owned()
            }
        } else if verb == "RSET" {
            mail_from = None;
            rcpt_to.clear();
            "250 2.0.0 Ok\r\n".to_owned()
        } else if verb == "QUIT" {
            write(stream.get_mut(), "221 2.0.0 Bye\r\n").await?;
            return Ok(());
        } else {
            "502 5.5.2 Command not recognized\r\n".to_owned()
        };
        write(stream.get_mut(), &reply).await?;
    }
}

/// The address between `<` and `>` of a `MAIL FROM` or `RCPT TO` line.
fn angle_address(line: &str) -> String {
    line.split_once('<')
        .and_then(|(_, rest)| rest.split_once('>'))
        .map(|(address, _)| address.to_owned())
        .unwrap_or_default()
}

/// The message after `DATA`, up to the line with a single `.`, dot-stuffing undone, lines joined
/// with CRLF.
async fn read_data<S: AsyncRead + Unpin>(stream: &mut BufReader<S>) -> io::Result<String> {
    let mut lines = Vec::new();
    loop {
        let Some(line) = read_line(stream).await? else {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "the client closed the connection inside DATA",
            ));
        };
        if line == "." {
            return Ok(lines.join("\r\n"));
        }
        let line = match line.strip_prefix('.') {
            Some(rest) => rest.to_owned(),
            None => line,
        };
        lines.push(line);
    }
}
