//! A stand-in for the certs service's renewal endpoint (`POST /certs/renew`,
//! services/certs/README.md, 「換發」) over HTTPS with the front's certificate, and the agent
//! wired with its renewal as the binary wires it, pointed at the stand-in.
//!
//! The stand-in follows the wire format of `protocol::renewal`: a fresh nonce per challenge, the
//! signature checked against the gateway's registered public key, a new certificate for that key
//! from the issuing CA it was given. Refusals (429, 503, ...) can be scripted; it records every
//! request.

#![allow(
    clippy::unwrap_used,
    reason = "test helpers unwrap like the tests themselves (clippy.toml allows it in tests)"
)]

use std::collections::{HashMap, VecDeque};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use agent::link::{self, Subscriptions};
use agent::renewal::HttpsEndpoint;
use agent::transport::CloudUrl;
use base64::Engine as _;
use bytes::Bytes;
use http_body_util::{BodyExt as _, Full, Limited};
use hyper::body::Incoming;
use hyper::header::{CONTENT_TYPE, HOST, RETRY_AFTER};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use protocol::UnixMillis;
use protocol::renewal::{
    Challenge, MAX_REQUEST_BYTES, Purpose, RENEWAL_PATH, RenewalRequest, RenewedCertificate,
};
use rcgen::KeyPair;
use ring::signature::{ECDSA_P256_SHA256_ASN1, UnparsedPublicKey};
use rustls_pki_types::CertificateDer;
use rustls_pki_types::pem::PemObject;
use testkit::clock::bounded;
use testkit::mqtt::{Authority, FRONT_HOST, TestPki};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_rustls::TlsAcceptor;
use tokio_util::sync::CancellationToken;

use super::wss::{Agent, Files};

/// One request the stand-in answered.
#[derive(Clone, Debug)]
pub struct Seen {
    /// When it arrived.
    pub at: Instant,
    /// `challenge` or `renew`.
    pub step: &'static str,
    /// The status it was answered with.
    pub status: u16,
    /// Its `Host` header.
    pub host: String,
}

struct State {
    issuing: Authority,
    root: String,
    key: KeyPair,
    validity: Duration,
    challenges: Mutex<HashMap<String, Challenge>>,
    script: Mutex<VecDeque<(u16, Option<u64>)>>,
    seen: Mutex<Vec<Seen>>,
    issued: Mutex<Vec<String>>,
    nonces: AtomicU64,
    /// While `false`, requests wait before they are answered.
    open: watch::Sender<bool>,
}

/// The stand-in renewal endpoint.
pub struct RenewalServer {
    address: SocketAddr,
    state: Arc<State>,
    task: JoinHandle<()>,
}

impl RenewalServer {
    /// Serves on the loopback interface with the front's certificate, renewing the gateway whose
    /// public key is that of `key_pem` with certificates of `issuing` (a CA under
    /// `pki.broker_ca`), valid from five minutes back (as the certs service issues them) to
    /// `validity` from now.
    pub async fn start(
        pki: &TestPki,
        issuing: Authority,
        key_pem: &str,
        validity: Duration,
    ) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let state = Arc::new(State {
            issuing,
            root: pki.broker_ca.pem.clone(),
            key: KeyPair::from_pem(key_pem).unwrap(),
            validity,
            challenges: Mutex::new(HashMap::new()),
            script: Mutex::new(VecDeque::new()),
            seen: Mutex::new(Vec::new()),
            issued: Mutex::new(Vec::new()),
            nonces: AtomicU64::new(0),
            open: watch::Sender::new(true),
        });
        let acceptor = TlsAcceptor::from(pki.front_tls());
        let task = {
            let state = Arc::clone(&state);
            tokio::spawn(async move {
                while let Ok((tcp, _)) = listener.accept().await {
                    let acceptor = acceptor.clone();
                    let state = Arc::clone(&state);
                    tokio::spawn(async move {
                        let Ok(tls) = acceptor.accept(tcp).await else {
                            return;
                        };
                        let service = service_fn(move |request| {
                            let state = Arc::clone(&state);
                            async move { Ok::<_, Infallible>(handle(&state, request).await) }
                        });
                        let _ = http1::Builder::new()
                            .serve_connection(TokioIo::new(tls), service)
                            .await;
                    });
                }
            })
        };
        Self {
            address,
            state,
            task,
        }
    }

    pub fn address(&self) -> SocketAddr {
        self.address
    }

    /// Answers the next request with `status` (and `Retry-After: retry_after`) instead.
    pub fn refuse_next(&self, status: u16, retry_after: Option<u64>) {
        self.state
            .script
            .lock()
            .unwrap()
            .push_back((status, retry_after));
    }

    /// Holds every request unanswered until [`Self::release`] (a held request may time out at
    /// the agent, which then retries).
    pub fn hold(&self) {
        self.state.open.send_replace(false);
    }

    /// Answers the held requests and those that follow.
    pub fn release(&self) {
        self.state.open.send_replace(true);
    }

    pub fn seen(&self) -> Vec<Seen> {
        self.state.seen.lock().unwrap().clone()
    }

    /// The certificates issued so far, in PEM.
    pub fn issued(&self) -> Vec<String> {
        self.state.issued.lock().unwrap().clone()
    }

    /// Waits for the `count`th certificate and returns it in DER.
    pub async fn wait_issued(&self, count: usize) -> CertificateDer<'static> {
        bounded(
            format!("certificate {count} from the renewal endpoint"),
            async {
                loop {
                    if let Some(pem) = self.issued().get(count - 1) {
                        return CertificateDer::from_pem_slice(pem.as_bytes()).unwrap();
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            },
        )
        .await
    }
}

impl Drop for RenewalServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn reply(status: u16, retry_after: Option<u64>, body: Vec<u8>) -> Response<Full<Bytes>> {
    let mut response = Response::builder()
        .status(StatusCode::from_u16(status).unwrap())
        .header(CONTENT_TYPE, "application/json");
    if let Some(seconds) = retry_after {
        response = response.header(RETRY_AFTER, seconds.to_string());
    }
    response.body(Full::new(Bytes::from(body))).unwrap()
}

fn json(value: &impl serde::Serialize) -> Vec<u8> {
    serde_json::to_vec(value).unwrap()
}

async fn handle(state: &State, request: Request<Incoming>) -> Response<Full<Bytes>> {
    let host = request
        .headers()
        .get(HOST)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_owned();
    if request.method() != Method::POST || request.uri().path() != RENEWAL_PATH {
        return reply(404, None, b"{}".to_vec());
    }
    let is_json = request
        .headers()
        .get(CONTENT_TYPE)
        .is_some_and(|value| value.as_bytes() == b"application/json");
    if !is_json {
        return reply(400, None, b"{}".to_vec());
    }
    let Ok(body) = Limited::new(request.into_body(), MAX_REQUEST_BYTES)
        .collect()
        .await
    else {
        return reply(400, None, b"{}".to_vec());
    };
    let Ok(parsed) = serde_json::from_slice::<RenewalRequest>(&body.to_bytes()) else {
        return reply(422, None, b"{}".to_vec());
    };
    let step = match parsed {
        RenewalRequest::Challenge(_) => "challenge",
        RenewalRequest::Renew(_) => "renew",
    };
    // The sender lives as long as `state`, so the wait ends only when the gate opens.
    let _ = state.open.subscribe().wait_for(|open| *open).await;
    let scripted = state.script.lock().unwrap().pop_front();
    let (status, retry_after, body) = match scripted {
        Some((status, retry_after)) => (status, retry_after, b"{}".to_vec()),
        None => {
            let (status, body) = state.answer(parsed);
            (status, None, body)
        }
    };
    state.seen.lock().unwrap().push(Seen {
        at: Instant::now(),
        step,
        status,
        host,
    });
    reply(status, retry_after, body)
}

impl State {
    fn answer(&self, request: RenewalRequest) -> (u16, Vec<u8>) {
        match request {
            RenewalRequest::Challenge(request) => {
                let mut nonce = [0_u8; 32];
                nonce[..8]
                    .copy_from_slice(&self.nonces.fetch_add(1, Ordering::Relaxed).to_be_bytes());
                let challenge = Challenge {
                    hardware_id: request.hardware_id,
                    purpose: Purpose::CertRenewal,
                    nonce: base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(nonce),
                    expires: 4_000_000_000,
                };
                self.challenges
                    .lock()
                    .unwrap()
                    .insert(challenge.nonce.clone(), challenge.clone());
                (200, json(&challenge))
            }
            RenewalRequest::Renew(request) => {
                let Some(challenge) = self.challenges.lock().unwrap().remove(&request.nonce) else {
                    return (409, b"{}".to_vec());
                };
                if challenge.hardware_id != request.hardware_id {
                    return (403, b"{}".to_vec());
                }
                let Ok(signature) =
                    base64::engine::general_purpose::STANDARD.decode(&request.signature)
                else {
                    return (422, b"{}".to_vec());
                };
                let verified =
                    UnparsedPublicKey::new(&ECDSA_P256_SHA256_ASN1, self.key.public_key_raw())
                        .verify(&challenge.signing_input(), &signature);
                if verified.is_err() {
                    return (401, b"{}".to_vec());
                }
                let now = SystemTime::now();
                let certificate = self.issuing.client_for_key(
                    request.hardware_id.as_str(),
                    &self.key,
                    now - Duration::from_secs(300),
                    now + self.validity,
                );
                self.issued.lock().unwrap().push(certificate.clone());
                let renewed = RenewedCertificate {
                    hardware_id: request.hardware_id,
                    serial: "01".to_owned(),
                    ca_key_id: "00".to_owned(),
                    not_before: String::new(),
                    not_after: String::new(),
                    certificate_chain: format!("{certificate}{}", self.issuing.pem),
                    certificate,
                    root_certificate: self.root.clone(),
                };
                (201, json(&renewed))
            }
        }
    }
}

/// The agent with its renewal running, as the binary starts them.
pub struct Renewing {
    pub agent: Agent,
    /// The credentials' version: 1 after the first renewal.
    pub version: watch::Receiver<u64>,
    /// The expiry heartbeats report.
    pub not_after: watch::Receiver<UnixMillis>,
    pub renewal: JoinHandle<()>,
}

impl Renewing {
    /// Starts the agent of `files`, subscribed to `subscriptions`, renewing through `server`.
    pub fn start(files: &Files, server: &RenewalServer, subscriptions: Subscriptions) -> Self {
        let mut setup = files.setup().unwrap();
        setup.link.subscriptions = subscriptions;
        let outer =
            agent::tls::outer_config(&[PathBuf::from(files.path("public-ca.pem"))]).unwrap();
        let url = CloudUrl::parse(&format!(
            "wss://{FRONT_HOST}:{}/mqtt",
            server.address().port()
        ))
        .unwrap();
        let renewal = setup.renewal.with_endpoint(HttpsEndpoint::new(&url, outer));
        let version = renewal.credentials();
        let not_after = renewal.certificate_not_after();
        let shutdown = CancellationToken::new();
        let (link, incoming, supervisor) =
            link::link(setup.link, setup.connector, shutdown.clone());
        let renewal =
            tokio::spawn(renewal.run(link.state(), link.certificate_refused(), shutdown.clone()));
        Self {
            agent: Agent {
                link,
                incoming,
                shutdown,
                supervisor: tokio::spawn(supervisor.run()),
            },
            version,
            not_after,
            renewal,
        }
    }

    /// Waits for the credentials' version `wanted`.
    pub async fn renewed(&mut self, wanted: u64) {
        bounded(
            format!("credentials version {wanted}"),
            self.version.wait_for(|version| *version >= wanted),
        )
        .await
        .unwrap();
    }

    pub async fn stop(self) {
        self.agent.stop().await;
        bounded("the renewal to stop", self.renewal).await.unwrap();
    }
}
