//! The production workstation tool (`factory/workstation`) against the real certs service and a
//! throwaway Postgres (Laysi/trustiot-v2#615): the tool enrolls its workstation certificate and
//! registers gateways through a stand-in for the ingress, which checks the session cookie and
//! swaps it for a platform administrator's internal token, as the ingress does. The stand-in can
//! drop an answer after the service has committed it, the case the tool's retries exist for.
//!
//! A registration, its rerun, and a run whose first answer is lost leave exactly one
//! registration row, one gateway certificate and one `certs.gateway_registered` event per
//! hardware ID; the tool records each registration once, with the service's serial.
//!
//! Skipped without podman or docker unless `CI` is set.

mod support;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use certs::pem;
use contract::certs::GatewayRegistration;
use rcgen::{
    CertificateParams, KeyPair, PKCS_ECDSA_P256_SHA256, PKCS_ECDSA_P384_SHA384, PublicKeyData as _,
};
use sha2::{Digest as _, Sha256};
use support::{Service, TestCa, now, temp_dir, wait_for};
use testkit::product::{ProductPostgres, product_postgres};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use workstation::config::Origin;
use workstation::http::{Client, ExtraHeaders};
use workstation::identity::{self, WorkstationDirectory};
use workstation::journal::{Journal, State};
use workstation::register::{self, Backoff, Gateway, Registrar, Workstation};
use workstation::secret::SessionCookie;

/// The CRL schedule of `tests/registration.rs`: the first CRL is out within seconds, so an
/// issuing CA can sign.
const FAST_CRLS: [(&str, &str); 4] = [
    ("TRUSTIOT__CRL__CHECK_INTERVAL", "1s"),
    ("TRUSTIOT__CRL__VALIDITY", "1h"),
    ("TRUSTIOT__CRL__RENEW_BEFORE", "3590s"),
    ("TRUSTIOT__CRL__ALERT_BEFORE", "3500s"),
];

/// The session cookie's name, as the identity service sets it.
const SESSION_COOKIE: &str = "__Host-trustiot-session";

/// A stand-in for the ingress on loopback: it accepts requests that carry the session cookie,
/// forwards them to the certs service with a platform administrator's internal token instead,
/// and relays the answer, unless it was told to drop the next ones.
struct Ingress {
    url: String,
    shared: Arc<IngressState>,
    task: tokio::task::JoinHandle<()>,
}

struct IngressState {
    cookie: String,
    upstream: String,
    token: String,
    /// How many of the next answers to drop after the service has given them.
    drop_next: AtomicU32,
    /// `(path, status from the service, relayed)` of every forwarded request.
    forwarded: Mutex<Vec<(String, u16, bool)>>,
}

impl Drop for Ingress {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Ingress {
    async fn start(service: &Service, cookie: &str) -> Self {
        let shared = Arc::new(IngressState {
            cookie: cookie.to_owned(),
            upstream: service.base_url.clone(),
            token: service.admin_token(),
            drop_next: AtomicU32::new(0),
            forwarded: Mutex::new(Vec::new()),
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bound");
        let url = format!("http://{}", listener.local_addr().expect("an address"));
        let serving = Arc::clone(&shared);
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let shared = Arc::clone(&serving);
                tokio::spawn(async move { relay(&shared, stream).await });
            }
        });
        Self { url, shared, task }
    }

    fn drop_next(&self, count: u32) {
        self.shared.drop_next.store(count, Ordering::SeqCst);
    }

    /// `(status from the service, relayed)` of the requests to `path`.
    fn forwarded(&self, path: &str) -> Vec<(u16, bool)> {
        self.shared
            .forwarded
            .lock()
            .expect("unpoisoned")
            .iter()
            .filter(|(forwarded, _, _)| forwarded == path)
            .map(|(_, status, relayed)| (*status, *relayed))
            .collect()
    }
}

/// Reads one HTTP/1.1 request: method, path, headers (lower-case names) and body.
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

async fn respond(stream: &mut TcpStream, status: u16, date: Option<&str>, body: &[u8]) {
    let mut head = format!(
        "HTTP/1.1 {status} Status\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
         connection: close\r\n",
        body.len()
    );
    if let Some(date) = date {
        head.push_str(&format!("date: {date}\r\n"));
    }
    head.push_str("\r\n");
    let _ = stream.write_all(head.as_bytes()).await;
    let _ = stream.write_all(body).await;
    let _ = stream.shutdown().await;
}

async fn relay(shared: &IngressState, mut stream: TcpStream) {
    let Some((method, path, headers, body)) = read_request(&mut stream).await else {
        return;
    };
    let expected = format!("{SESSION_COOKIE}={}", shared.cookie);
    if headers.get("cookie") != Some(&expected) {
        respond(
            &mut stream,
            401,
            None,
            br#"{"code":"unauthenticated","message_key":"error.unauthenticated"}"#,
        )
        .await;
        return;
    }
    let mut request = reqwest::Client::new()
        .request(
            method.parse().expect("a method"),
            format!("{}{path}", shared.upstream),
        )
        .timeout(Duration::from_secs(60))
        .header(contract::INTERNAL_IDENTITY_HEADER, &shared.token)
        .body(body);
    for (name, value) in &headers {
        if name == "content-type" || name.starts_with("trustiot-workstation-") {
            request = request.header(name.as_str(), value.as_str());
        }
    }
    let response = request.send().await.expect("the certs service answers");
    let status = response.status().as_u16();
    let date = response
        .headers()
        .get("date")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let answer = response.bytes().await.expect("the body");
    let dropped = shared
        .drop_next
        .try_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
            left.checked_sub(1)
        })
        .is_ok();
    shared
        .forwarded
        .lock()
        .expect("unpoisoned")
        .push((path, status, !dropped));
    if dropped {
        // The service has committed; the workstation gets nothing back.
        drop(stream);
        return;
    }
    respond(&mut stream, status, date.as_deref(), &answer).await;
}

/// A gateway's CSR for a new P-256 key and a P-384 endorsement key, both PEM.
fn gateway(hardware_id: &str) -> Gateway {
    let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).expect("a gateway key");
    let request = CertificateParams::new(Vec::new())
        .expect("params")
        .serialize_request(&key)
        .expect("a CSR")
        .pem()
        .expect("PEM");
    let endorsement = KeyPair::generate_for(&PKCS_ECDSA_P384_SHA384).expect("an endorsement key");
    let endorsement = pem::encode(pem::PUBLIC_KEY, &endorsement.subject_public_key_info());
    Gateway::new(hardware_id, request.as_bytes(), endorsement.as_bytes()).expect("a gateway")
}

/// The first column of the one row `query` returns for `hardware_id`.
async fn scalar<T>(product: &ProductPostgres, query: &'static str, hardware_id: &str) -> T
where
    T: for<'r> sqlx::Decode<'r, sqlx::Postgres> + sqlx::Type<sqlx::Postgres> + Send + Unpin,
{
    sqlx::query_scalar(query)
        .bind(hardware_id)
        .fetch_one(&mut product.superuser().await)
        .await
        .unwrap_or_else(|error| panic!("{query}: {error}"))
}

/// Registrations, gateway certificates and registration events of `hardware_id`.
async fn totals(product: &ProductPostgres, hardware_id: &str) -> (i64, i64, i64) {
    (
        scalar(
            product,
            "SELECT count(*) FROM certs.gateway_registrations WHERE hardware_id = $1",
            hardware_id,
        )
        .await,
        scalar(
            product,
            "SELECT count(*) FROM certs.issued_certificates \
             WHERE profile = 'gateway' AND subject = $1",
            hardware_id,
        )
        .await,
        scalar(
            product,
            "SELECT count(*) FROM certs.outbox_event \
             WHERE event_type = 'certs.gateway_registered' AND aggregate_id = $1",
            hardware_id,
        )
        .await,
    )
}

/// Registers `gateway` with the library function the tool's `register` command runs.
async fn run(registrar: &Registrar<'_>, gateway: &Gateway) -> (u16, u32, String) {
    let registered = register::register(registrar, gateway, now)
        .await
        .unwrap_or_else(|error| panic!("registered: {error}"));
    (
        registered.status.as_u16(),
        registered.attempts,
        registered.registration.serial,
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn the_tool_registers_each_gateway_once_through_reruns_and_lost_answers() {
    let Some(product) = product_postgres(&["certs_app", "certs_platform"]).await else {
        return;
    };
    let ca = TestCa::new(&[1]);
    let service = Service::start(&product, &ca, &FAST_CRLS).await;
    wait_for("the published CRLs", || async {
        let (status, body) = service.get(certs::http::CRL_PATH).await;
        (status == 200).then_some(body)
    })
    .await;

    // A made-up session cookie, new on every run.
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes).expect("random");
    let cookie_value = URL_SAFE_NO_PAD.encode(bytes);
    let ingress = Ingress::start(&service, &cookie_value).await;
    let client = Client::new(
        Origin::parse(&ingress.url).expect("a loopback origin"),
        None,
        ExtraHeaders::none(),
        Duration::from_secs(60),
    )
    .expect("a client");
    let cookie = SessionCookie::parse(&cookie_value).expect("a cookie");

    // The workstation's key, and its certificate from the real service (`enroll`).
    let directory = WorkstationDirectory::new(temp_dir("certs-workstation-tool").join("ws"));
    identity::init(&directory).expect("init");
    let enrolled = identity::enroll(&client, &cookie, &directory, "line-d-ws-01", now)
        .await
        .expect("enrolled");
    let workstation = Workstation::load(&directory, now()).expect("the workstation");
    assert_eq!(workstation.serial(), enrolled.installed.serial);
    let journal = Journal::new(directory.path());
    let registrar = Registrar {
        client: &client,
        cookie: &cookie,
        workstation: &workstation,
        journal: &journal,
        backoff: Backoff {
            initial: Duration::from_millis(5),
            cap: Duration::from_millis(50),
            max_attempts: 5,
            max_total: Duration::from_secs(600),
        },
        root_sha256: Some(Sha256::digest(ca.root_der()).into()),
    };
    let path = register::REGISTRATIONS_PATH;

    // A new hardware ID, then the same command again.
    let first = gateway("tool-0001");
    let (status, attempts, serial) = run(&registrar, &first).await;
    assert_eq!((status, attempts), (201, 1));
    assert_eq!(totals(&product, "tool-0001").await, (1, 1, 1));
    let rerun = run(&registrar, &first).await;
    assert_eq!(rerun, (200, 1, serial.clone()));
    assert_eq!(totals(&product, "tool-0001").await, (1, 1, 1));

    // The service answers, the answer is lost, and the tool sends the request again.
    ingress.drop_next(1);
    let lost = run(&registrar, &first).await;
    assert_eq!(lost, (200, 2, serial.clone()));
    assert_eq!(totals(&product, "tool-0001").await, (1, 1, 1));

    // The first answer for a new hardware ID is lost: the service committed it, and the repeat
    // gets that registration.
    let second = gateway("tool-0002");
    ingress.drop_next(1);
    let (status, attempts, second_serial) = run(&registrar, &second).await;
    assert_eq!((status, attempts), (200, 2));
    assert_eq!(totals(&product, "tool-0002").await, (1, 1, 1));

    assert_eq!(
        ingress.forwarded(path),
        [
            (201, true),
            (200, true),
            (200, false),
            (200, true),
            (201, false),
            (200, true),
        ]
    );
    for (hardware_id, serial) in [("tool-0001", &serial), ("tool-0002", &second_serial)] {
        let record = journal
            .load(hardware_id)
            .expect("readable")
            .expect("a record");
        assert_eq!(record.state, State::Registered);
        let registration: GatewayRegistration = record.registration.expect("the registration");
        assert_eq!(&registration.serial, serial, "{hardware_id}");
        let stored: String = scalar(
            &product,
            "SELECT encode(certificate_serial, 'hex') FROM certs.gateway_registrations \
             WHERE hardware_id = $1",
            hardware_id,
        )
        .await;
        assert_eq!(
            stored.trim_start_matches('0'),
            serial.trim_start_matches('0'),
            "{hardware_id}: the record holds the service's certificate"
        );
    }
    println!(
        "evidence: forwarded registrations {:?}; tool-0001 and tool-0002 each (1 registration, 1 \
         gateway certificate, 1 certs.gateway_registered event)",
        ingress.forwarded(path)
    );
}
