//! A stand-in for the certs service's registration endpoint on loopback (Laysi/trustiot-v2#615):
//! plain HTTP/1.1 on raw TCP, so it can drop a connection after it has registered (a lost
//! answer) or hold one without answering (a timeout). It checks every request as the certs
//! service does: the session cookie, the workstation certificate, the time, a nonce it has not
//! seen, and the signature over `contract::certs::workstation_signing_input`.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use contract::certs::{
    WORKSTATION_CERTIFICATE_HEADER, WORKSTATION_CLOCK_SKEW_SECS, WORKSTATION_NONCE_HEADER,
    WORKSTATION_SIGNATURE_HEADER, WORKSTATION_TIMESTAMP_HEADER, workstation_signing_input,
};
use rcgen::{
    BasicConstraints, CertificateParams, CertificateRevocationListParams,
    CertificateSigningRequestParams, DnType, IsCa, Issuer, KeyIdMethod, KeyPair, KeyUsagePurpose,
    PublicKeyData as _, SerialNumber,
};
use sha2::{Digest as _, Sha256};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use x509_parser::asn1_rs::BitString;
use x509_parser::oid_registry::OID_SIG_ECDSA_WITH_SHA256;
use x509_parser::prelude::{FromDer as _, X509Certificate};
use x509_parser::x509::AlgorithmIdentifier;

/// The registration path.
pub const PATH: &str = "/api/certs/v1/gateway-registrations";

/// What to change in an answer that is otherwise right.
#[derive(Clone, Copy, Debug)]
pub enum Tamper {
    HardwareId,
    PublicKeySha256,
    EndorsementKeySha256,
    CommonName,
    CertificateKey,
    /// The same common name and key, signed by a CA the root never signed, which the chain
    /// carries instead of the issuing CA.
    ForeignIssuer,
    /// The same common name and key under a whole other hierarchy: certificate, chain and root
    /// certificate all consistent, but not the platform's root.
    ForeignRoot,
    /// A chain whose first certificate is not `certificate`.
    ChainLeaf,
    /// A chain of the certificate alone.
    ChainWithoutIssuer,
    /// A root CRL that another root signed (Laysi/trustiot-v2#832).
    ForeignRootCrl,
    /// A root CRL that is not PEM at all.
    RootCrlNotPem,
}

/// How the stand-in handles one registration request.
#[derive(Clone, Debug)]
pub enum Action {
    /// Checks and registers (201), or answers a repeat (200) or a different key (409).
    Answer,
    /// Registers like `Answer`, then closes the connection without a byte of the answer.
    DropAfterCommit,
    /// Never answers; the connection stays open until the stand-in is dropped.
    Hold,
    /// Answers `status` with an M1 body: code, message key, `details.reason`, field errors.
    Error {
        status: u16,
        code: &'static str,
        message_key: &'static str,
        reason: Option<&'static str>,
        fields: Vec<(&'static str, &'static str)>,
    },
    /// Registers like `Answer`, then changes the answer.
    Tamper(Tamper),
}

impl Action {
    pub fn error(status: u16, code: &'static str, message_key: &'static str) -> Self {
        Self::Error {
            status,
            code,
            message_key,
            reason: None,
            fields: Vec::new(),
        }
    }

    pub fn refused(reason: &'static str) -> Self {
        Self::Error {
            status: 403,
            code: "permission_denied",
            message_key: "gateway_registration.workstation_refused",
            reason: Some(reason),
            fields: Vec::new(),
        }
    }
}

/// One registration request as received.
#[derive(Clone, Debug)]
pub struct Seen {
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
    /// Whether every workstation check held (cookie, certificate, time, new nonce, signature).
    pub verified: bool,
    /// What it was answered: a status, `0` for a dropped or held connection.
    pub answered: u16,
}

impl Seen {
    pub fn header(&self, name: &str) -> &str {
        self.headers.get(name).map_or("", String::as_str)
    }
}

struct Registered {
    public_key_sha256: String,
    endorsement_key_sha256: String,
    answer: serde_json::Value,
}

struct Shared {
    cookie: String,
    workstation_certificate: Vec<u8>,
    planted_set_cookie: String,
    /// The root, which signs the issuing CA and the root CRLs.
    root: Issuer<'static, KeyPair>,
    /// The root CRL every answer carries from now on, as the certs service's current one
    /// (Laysi/trustiot-v2#832); none until [`Registry::adopt_root_crl`].
    root_crl: Mutex<Option<String>>,
    /// The issuing CA that signs gateway certificates, under `root_pem`.
    ca: Issuer<'static, KeyPair>,
    ca_pem: String,
    root_pem: String,
    script: Mutex<VecDeque<Action>>,
    seen: Mutex<Vec<Seen>>,
    nonces: Mutex<HashSet<String>>,
    registrations: Mutex<HashMap<String, Registered>>,
    held: Mutex<Vec<TcpStream>>,
    logouts: Mutex<u32>,
    /// The platform-wide event log, oldest first: a `certs.gateway_registered` event for each new
    /// registration, each after an event of another type (Laysi/trustiot-v2#616).
    events: Mutex<Vec<serde_json::Value>>,
    /// The query string of every event log request.
    event_log_queries: Mutex<Vec<String>>,
}

/// The stand-in on `http://127.0.0.1:<port>`.
pub struct Registry {
    pub url: String,
    shared: Arc<Shared>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Registry {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Registry {
    /// A stand-in that accepts the session `cookie` and the workstation certificate
    /// `workstation_certificate` (DER), answers each registration with the next action of
    /// `script` (then [`Action::Answer`]), and puts `planted_set_cookie` in a `Set-Cookie` of
    /// every answer.
    pub async fn start(
        cookie: &str,
        workstation_certificate: Vec<u8>,
        planted_set_cookie: &str,
        script: Vec<Action>,
    ) -> Self {
        let (root_params, root_key) = ca_params("TrustIoT test root");
        let root_pem = root_params.self_signed(&root_key).expect("a root").pem();
        let root = Issuer::new(root_params, root_key);
        let (ca, ca_pem) = issuing_ca("TrustIoT test issuing CA", Some(&root));
        let shared = Arc::new(Shared {
            cookie: cookie.to_owned(),
            workstation_certificate,
            planted_set_cookie: planted_set_cookie.to_owned(),
            root,
            root_crl: Mutex::new(None),
            ca,
            ca_pem,
            root_pem,
            script: Mutex::new(script.into()),
            seen: Mutex::new(Vec::new()),
            nonces: Mutex::new(HashSet::new()),
            registrations: Mutex::new(HashMap::new()),
            held: Mutex::new(Vec::new()),
            logouts: Mutex::new(0),
            events: Mutex::new(Vec::new()),
            event_log_queries: Mutex::new(Vec::new()),
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bound");
        let url = format!("http://{}", listener.local_addr().expect("an address"));
        let serving = Arc::clone(&shared);
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let shared = Arc::clone(&serving);
                tokio::spawn(async move { serve(shared, stream).await });
            }
        });
        Self { url, shared, task }
    }

    /// The root certificate every answer carries, PEM.
    pub fn root_pem(&self) -> &str {
        &self.shared.root_pem
    }

    /// Signs a root CRL numbered `number` with the root, which every answer from now on
    /// carries (repeats too, as the certs service answers with its current one), and gives its
    /// PEM.
    pub fn adopt_root_crl(&self, number: u64) -> String {
        let pem = root_crl(&self.shared.root, number);
        *self.shared.root_crl.lock().expect("unpoisoned") = Some(pem.clone());
        pem
    }

    /// Adds actions to the script.
    pub fn push(&self, actions: Vec<Action>) {
        self.shared
            .script
            .lock()
            .expect("unpoisoned")
            .extend(actions);
    }

    /// Every registration request so far.
    pub fn seen(&self) -> Vec<Seen> {
        self.shared.seen.lock().expect("unpoisoned").clone()
    }

    /// How many hardware IDs are registered.
    pub fn registrations(&self) -> usize {
        self.shared.registrations.lock().expect("unpoisoned").len()
    }

    /// How many logouts (`DELETE /api/identity/v1/sessions/current`) arrived.
    pub fn logouts(&self) -> u32 {
        *self.shared.logouts.lock().expect("unpoisoned")
    }

    /// The query string of every event log request so far.
    pub fn event_log_queries(&self) -> Vec<String> {
        self.shared
            .event_log_queries
            .lock()
            .expect("unpoisoned")
            .clone()
    }

    /// Adds `count` events of another type to the event log, newer than every event so far.
    pub fn add_other_events(&self, count: usize) {
        let mut events = self.shared.events.lock().expect("unpoisoned");
        for _ in 0..count {
            let number = events.len();
            events.push(other_event(number));
        }
    }
}

/// An event of another type than a registration.
fn other_event(number: usize) -> serde_json::Value {
    serde_json::json!({
        "event_id": format!("0199a000-0000-7000-8000-{number:012}"),
        "type": "identity.tenant_created",
        "version": 1,
        "producer": "identity",
        "tenant_id": null,
        "occurred_at": "2026-10-24T08:00:00Z",
        "recorded_at": "2026-10-24T08:00:01Z",
        "aggregate_type": "tenant",
        "aggregate_id": format!("tenant-{number}"),
        "aggregate_seq": 1,
        "actor": null,
        "payload": {},
    })
}

/// The `certs.gateway_registered` event of a new registration.
fn registered_event(number: usize, answer: &serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "event_id": format!("0199a000-0000-7000-9000-{number:012}"),
        "type": "certs.gateway_registered",
        "version": 1,
        "producer": "certs",
        "tenant_id": null,
        "occurred_at": answer["registered_at"],
        "recorded_at": answer["registered_at"],
        "aggregate_type": "gateway_registration",
        "aggregate_id": answer["hardware_id"],
        "aggregate_seq": 1,
        "actor": null,
        "payload": {
            "hardware_id": answer["hardware_id"],
            "public_key_sha256": answer["public_key_sha256"],
            "endorsement_key_sha256": answer["endorsement_key_sha256"],
            "certificate_serial": answer["serial"],
            "workstation": answer["workstation"],
        },
    })
}

/// One page of the event log, newest first: at most one event per page, so a reader has to
/// follow `next_cursor` (the index of the next event, counted from the newest).
fn event_log_page(shared: &Shared, query: &str) -> String {
    let events = shared.events.lock().expect("unpoisoned");
    let start: usize = query
        .split('&')
        .find_map(|pair| pair.strip_prefix("cursor="))
        .map_or(0, |cursor| {
            cursor.parse().expect("a cursor the stand-in gave")
        });
    let newest_first: Vec<&serde_json::Value> = events.iter().rev().collect();
    let page: Vec<&serde_json::Value> = newest_first.iter().skip(start).take(1).copied().collect();
    let next = (start + 1 < newest_first.len()).then(|| (start + 1).to_string());
    serde_json::json!({ "events": page, "next_cursor": next }).to_string()
}

/// The parameters and key of a CA named `name`.
fn ca_params(name: &str) -> (CertificateParams, KeyPair) {
    let key = KeyPair::generate().expect("a CA key");
    let mut params = CertificateParams::new(Vec::new()).expect("params");
    params.distinguished_name.push(DnType::CommonName, name);
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    (params, key)
}

/// A CA named `name` signed by `root`, or self-signed without one: the issuer and its PEM.
fn issuing_ca(
    name: &str,
    root: Option<&Issuer<'static, KeyPair>>,
) -> (Issuer<'static, KeyPair>, String) {
    let (params, key) = ca_params(name);
    let pem = match root {
        Some(root) => params.signed_by(&key, root),
        None => params.self_signed(&key),
    }
    .expect("a CA certificate")
    .pem();
    (Issuer::new(params, key), pem)
}

/// Reads one request: the request line, the headers (lower-case names) and the body.
async fn read_request(
    stream: &mut TcpStream,
) -> Option<(String, String, HashMap<String, String>, Vec<u8>)> {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 4096];
    let head_end = loop {
        if let Some(position) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break position;
        }
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(chunk.get(..read)?);
    };
    let head = String::from_utf8(buffer.get(..head_end)?.to_vec()).ok()?;
    let mut lines = head.split("\r\n");
    let mut request_line = lines.next()?.split(' ');
    let method = request_line.next()?.to_owned();
    let path = request_line.next()?.to_owned();
    let mut headers = HashMap::new();
    for line in lines {
        let (name, value) = line.split_once(':')?;
        headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_owned());
    }
    let length: usize = headers
        .get("content-length")
        .map_or(Some(0), |value| value.parse().ok())?;
    let mut body = buffer.get(head_end + 4..)?.to_vec();
    while body.len() < length {
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        body.extend_from_slice(chunk.get(..read)?);
    }
    Some((method, path, headers, body))
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        401 => "Unauthorized",
        403 => "Forbidden",
        409 => "Conflict",
        422 => "Unprocessable Entity",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "Status",
    }
}

async fn answer(shared: &Shared, stream: &mut TcpStream, status: u16, body: &str) {
    let date = chrono::DateTime::<chrono::Utc>::from_timestamp(now(), 0)
        .expect("a time")
        .format("%a, %d %b %Y %H:%M:%S GMT");
    let text = format!(
        "HTTP/1.1 {status} {}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
         date: {date}\r\nset-cookie: tracking={}; Path=/\r\nconnection: close\r\n\r\n{body}",
        reason_phrase(status),
        body.len(),
        shared.planted_set_cookie
    );
    let _ = stream.write_all(text.as_bytes()).await;
    let _ = stream.shutdown().await;
}

fn m1(code: &str, message_key: &str, reason: Option<&str>, fields: &[(&str, &str)]) -> String {
    let mut body = serde_json::json!({
        "code": code,
        "message_key": message_key,
        "field_errors": fields
            .iter()
            .map(|(field, key)| serde_json::json!({"field": field, "message_key": key}))
            .collect::<Vec<_>>(),
    });
    if let Some(reason) = reason {
        body["details"] = serde_json::json!({ "reason": reason });
    }
    body.to_string()
}

fn now() -> i64 {
    let since = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("after 1970");
    i64::try_from(since.as_secs()).expect("in range")
}

/// Whether the request's workstation headers hold, as the certs service checks them.
fn verify(shared: &Shared, headers: &HashMap<String, String>, body: &[u8]) -> bool {
    let get = |name: &str| headers.get(name).map(String::as_str);
    let (Some(certificate), Some(timestamp), Some(nonce), Some(signature)) = (
        get(WORKSTATION_CERTIFICATE_HEADER),
        get(WORKSTATION_TIMESTAMP_HEADER),
        get(WORKSTATION_NONCE_HEADER),
        get(WORKSTATION_SIGNATURE_HEADER),
    ) else {
        return false;
    };
    if STANDARD.decode(certificate).ok().as_deref() != Some(&shared.workstation_certificate[..]) {
        return false;
    }
    let Ok(timestamp) = timestamp.parse::<i64>() else {
        return false;
    };
    if now().abs_diff(timestamp) > WORKSTATION_CLOCK_SKEW_SECS.unsigned_abs() {
        return false;
    }
    if !shared
        .nonces
        .lock()
        .expect("unpoisoned")
        .insert(nonce.to_owned())
    {
        return false;
    }
    let Ok(signature) = STANDARD.decode(signature) else {
        return false;
    };
    let Ok((_, parsed)) = X509Certificate::from_der(&shared.workstation_certificate) else {
        return false;
    };
    let digest: [u8; 32] = Sha256::digest(body).into();
    let input = workstation_signing_input("POST", PATH, timestamp, nonce, &digest);
    x509_parser::verify::verify_signature(
        parsed.public_key(),
        &AlgorithmIdentifier::new(OID_SIG_ECDSA_WITH_SHA256, None),
        &BitString::new(0, &signature),
        &input,
    )
    .is_ok()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Registers the body's gateway: the status and the answer.
fn register(shared: &Shared, body: &[u8]) -> (u16, String) {
    let json: serde_json::Value = serde_json::from_slice(body).expect("a JSON body");
    let id = json["hardware_id"]
        .as_str()
        .expect("a hardware ID")
        .to_owned();
    let request = CertificateSigningRequestParams::from_pem(
        json["certificate_request"].as_str().expect("a CSR"),
    )
    .expect("a valid CSR");
    let ek_text = json["endorsement_key"]
        .as_str()
        .expect("an endorsement key");
    let ek = pem_der(ek_text);
    let public_key_sha256 = hex(&Sha256::digest(
        request.public_key.subject_public_key_info(),
    ));
    let endorsement_key_sha256 = hex(&Sha256::digest(&ek));
    let mut registrations = shared.registrations.lock().expect("unpoisoned");
    if let Some(existing) = registrations.get(&id) {
        if existing.public_key_sha256 != public_key_sha256 {
            return (
                409,
                m1(
                    "conflict",
                    "gateway_registration.public_key_mismatch",
                    None,
                    &[],
                ),
            );
        }
        if existing.endorsement_key_sha256 != endorsement_key_sha256 {
            return (
                409,
                m1(
                    "conflict",
                    "gateway_registration.endorsement_key_mismatch",
                    None,
                    &[],
                ),
            );
        }
        let mut answer = existing.answer.clone();
        with_root_crl(shared, &mut answer);
        return (200, answer.to_string());
    }
    let mut params = CertificateParams::new(Vec::new()).expect("params");
    params
        .distinguished_name
        .push(DnType::CommonName, id.as_str());
    let certificate = params
        .signed_by(&request.public_key, &shared.ca)
        .expect("a gateway certificate")
        .pem();
    let mut answer = serde_json::json!({
        "hardware_id": id,
        "public_key_sha256": public_key_sha256,
        "endorsement_key_sha256": endorsement_key_sha256,
        "workstation": "line-a-ws-01",
        "registered_at": "2026-10-24T08:00:00Z",
        "serial": "4f3a9c1e2b7d6058a1c3e5f708192a3b",
        "ca_key_id": "0".repeat(64),
        "not_before": "2026-10-24T07:55:00Z",
        "not_after": "2027-01-22T07:55:00Z",
        "certificate": certificate,
        "certificate_chain": format!("{certificate}{}", shared.ca_pem),
        "root_certificate": shared.root_pem,
    });
    with_root_crl(shared, &mut answer);
    {
        let mut events = shared.events.lock().expect("unpoisoned");
        let number = events.len();
        events.push(other_event(number));
        events.push(registered_event(number + 1, &answer));
    }
    registrations.insert(
        id,
        Registered {
            public_key_sha256,
            endorsement_key_sha256,
            answer: answer.clone(),
        },
    );
    (201, answer.to_string())
}

/// Puts the current root CRL, if any, in `answer`; without one the field is absent, as the
/// certs service answers.
fn with_root_crl(shared: &Shared, answer: &mut serde_json::Value) {
    match shared.root_crl.lock().expect("unpoisoned").as_ref() {
        Some(pem) => answer["root_crl"] = pem.clone().into(),
        None => {
            if let Some(object) = answer.as_object_mut() {
                object.remove("root_crl");
            }
        }
    }
}

/// A root CRL numbered `number` that `root` signs, revoking nothing, PEM.
pub fn root_crl(root: &Issuer<'static, KeyPair>, number: u64) -> String {
    CertificateRevocationListParams {
        this_update: rcgen::date_time_ymd(2020, 1, 1),
        next_update: rcgen::date_time_ymd(2099, 1, 1),
        crl_number: SerialNumber::from(number),
        issuing_distribution_point: None,
        revoked_certs: Vec::new(),
        key_identifier_method: KeyIdMethod::Sha256,
    }
    .signed_by(root)
    .expect("a root CRL")
    .pem()
    .expect("its PEM")
}

/// The DER of the one PEM block in `text`.
pub fn pem_der(text: &str) -> Vec<u8> {
    let body: String = text
        .lines()
        .filter(|line| !line.starts_with("-----"))
        .collect();
    STANDARD.decode(body.trim()).expect("base64")
}

fn tamper(shared: &Shared, answer: &str, how: Tamper) -> String {
    let mut json: serde_json::Value = serde_json::from_str(answer).expect("an answer");
    let certificate = json["certificate"]
        .as_str()
        .expect("a certificate")
        .to_owned();
    let original_key = || {
        let der = pem_der(&certificate);
        let (_, parsed) = X509Certificate::from_der(&der).expect("a parsed certificate");
        rcgen::SubjectPublicKeyInfo::from_der(parsed.public_key().raw).expect("an SPKI")
    };
    match how {
        Tamper::ForeignIssuer | Tamper::ForeignRoot => {
            let (root, root_pem) = issuing_ca("TrustIoT test root", None);
            let (foreign, foreign_pem) = if matches!(how, Tamper::ForeignRoot) {
                issuing_ca("TrustIoT test issuing CA", Some(&root))
            } else {
                issuing_ca("TrustIoT test issuing CA", None)
            };
            let id = json["hardware_id"].as_str().expect("an id").to_owned();
            let mut params = CertificateParams::new(Vec::new()).expect("params");
            params.distinguished_name.push(DnType::CommonName, id);
            let forged = params
                .signed_by(&original_key(), &foreign)
                .expect("a certificate")
                .pem();
            json["certificate_chain"] = format!("{forged}{foreign_pem}").into();
            json["certificate"] = forged.into();
            if matches!(how, Tamper::ForeignRoot) {
                json["root_certificate"] = root_pem.into();
            }
        }
        Tamper::ChainLeaf => {
            let other = KeyPair::generate().expect("another key");
            let mut params = CertificateParams::new(Vec::new()).expect("params");
            params
                .distinguished_name
                .push(DnType::CommonName, "gw-someone-else");
            let leaf = params
                .signed_by(&other, &shared.ca)
                .expect("a certificate")
                .pem();
            json["certificate_chain"] = format!("{leaf}{}", shared.ca_pem).into();
        }
        Tamper::ChainWithoutIssuer => json["certificate_chain"] = certificate.clone().into(),
        Tamper::ForeignRootCrl => {
            let (other_params, other_key) = ca_params("TrustIoT test root");
            let other = Issuer::new(other_params, other_key);
            json["root_crl"] = root_crl(&other, 7).into();
        }
        Tamper::RootCrlNotPem => json["root_crl"] = "not a CRL".into(),
        Tamper::HardwareId => json["hardware_id"] = "gw-someone-else".into(),
        Tamper::PublicKeySha256 => json["public_key_sha256"] = "1".repeat(64).into(),
        Tamper::EndorsementKeySha256 => json["endorsement_key_sha256"] = "2".repeat(64).into(),
        Tamper::CommonName | Tamper::CertificateKey => {
            let key = KeyPair::generate().expect("another key");
            let id = json["hardware_id"].as_str().expect("an id").to_owned();
            let name = if matches!(how, Tamper::CommonName) {
                format!("{id}-other")
            } else {
                id
            };
            let mut params = CertificateParams::new(Vec::new()).expect("params");
            params.distinguished_name.push(DnType::CommonName, name);
            let public_key = if matches!(how, Tamper::CommonName) {
                // The right key: the CSR's, as the original certificate has it.
                original_key()
            } else {
                rcgen::SubjectPublicKeyInfo::from_der(&key.subject_public_key_info())
                    .expect("an SPKI")
            };
            json["certificate"] = params
                .signed_by(&public_key, &shared.ca)
                .expect("a certificate")
                .pem()
                .into();
        }
    }
    json.to_string()
}

async fn serve(shared: Arc<Shared>, mut stream: TcpStream) {
    let Some((method, path, headers, body)) = read_request(&mut stream).await else {
        return;
    };
    let cookie_ok = headers.get("cookie").map(String::as_str)
        == Some(format!("__Host-trustiot-session={}", shared.cookie).as_str());
    if method == "DELETE" && path == "/api/identity/v1/sessions/current" {
        *shared.logouts.lock().expect("unpoisoned") += 1;
        answer(&shared, &mut stream, 204, "").await;
        return;
    }
    if method == "GET"
        && let Some(query) = path.strip_prefix("/api/identity/v1/platform/event-log?")
    {
        shared
            .event_log_queries
            .lock()
            .expect("unpoisoned")
            .push(query.to_owned());
        if !cookie_ok {
            let body = m1("unauthenticated", "error.unauthenticated", None, &[]);
            answer(&shared, &mut stream, 401, &body).await;
            return;
        }
        let page = event_log_page(&shared, query);
        answer(&shared, &mut stream, 200, &page).await;
        return;
    }
    if method != "POST" || path != PATH {
        answer(
            &shared,
            &mut stream,
            404,
            &m1("not_found", "error.not_found", None, &[]),
        )
        .await;
        return;
    }
    let verified = cookie_ok && verify(&shared, &headers, &body);
    let action = shared
        .script
        .lock()
        .expect("unpoisoned")
        .pop_front()
        .unwrap_or(Action::Answer);
    let record = |answered: u16| Seen {
        headers: headers.clone(),
        body: body.clone(),
        verified,
        answered,
    };
    if !verified {
        shared.seen.lock().expect("unpoisoned").push(record(403));
        answer(
            &shared,
            &mut stream,
            403,
            &m1(
                "permission_denied",
                "gateway_registration.workstation_refused",
                Some("signature_invalid"),
                &[],
            ),
        )
        .await;
        return;
    }
    match action {
        Action::Answer => {
            let (status, text) = register(&shared, &body);
            shared.seen.lock().expect("unpoisoned").push(record(status));
            answer(&shared, &mut stream, status, &text).await;
        }
        Action::DropAfterCommit => {
            let _ = register(&shared, &body);
            shared.seen.lock().expect("unpoisoned").push(record(0));
            // Closed before any byte of the answer.
            drop(stream);
        }
        Action::Hold => {
            shared.seen.lock().expect("unpoisoned").push(record(0));
            shared.held.lock().expect("unpoisoned").push(stream);
        }
        Action::Error {
            status,
            code,
            message_key,
            reason,
            fields,
        } => {
            shared.seen.lock().expect("unpoisoned").push(record(status));
            answer(
                &shared,
                &mut stream,
                status,
                &m1(code, message_key, reason, &fields),
            )
            .await;
        }
        Action::Tamper(how) => {
            let (status, text) = register(&shared, &body);
            shared.seen.lock().expect("unpoisoned").push(record(status));
            let text = tamper(&shared, &text, how);
            answer(&shared, &mut stream, status, &text).await;
        }
    }
}
