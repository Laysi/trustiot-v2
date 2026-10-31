//! Renewal without a network: a scripted endpoint that issues certificates like the certs
//! service, the checks of a renewed certificate, the atomic file swap, and the schedule and
//! retries of [`Renewal::run`] on tokio's paused clock with a wall clock that follows it.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use protocol::GatewayId;
use protocol::renewal::{Challenge, RENEWAL_PATH};
use rcgen::KeyPair;
use ring::signature::{ECDSA_P256_SHA256_ASN1, UnparsedPublicKey};
use testkit::mqtt::{Authority, ScratchDir};

use super::*;
use crate::backoff::MIN_DELAY;
use crate::identity::KeySource;

const GATEWAY: &str = "tg-renewal-1";
const DAY: Duration = Duration::from_secs(86_400);

/// Changes a renew answer before the agent gets it.
type Tampering = Box<dyn Fn(&mut RenewedCertificate, &Pki) + Send>;

/// Something a scripted endpoint does instead of answering like the certs service.
enum Scripted {
    /// This answer, whatever the request.
    Answer(Answer),
    /// A renew answer changed by the function.
    Tamper(Tampering),
}

/// A root CA (the agent's `broker.ca`), an issuing CA under it, and the gateway's key.
struct Pki {
    root: Authority,
    issuing: Authority,
    key: KeyPair,
}

impl Pki {
    fn new() -> Self {
        let root = Authority::new("renewal test root");
        let issuing = root.issuing("renewal test issuing CA");
        Self {
            root,
            issuing,
            key: KeyPair::generate().unwrap(),
        }
    }

    /// The chain the certs service returns for `gateway`'s key, valid from `not_before` for
    /// `validity`.
    fn chain(&self, gateway: &str, not_before: SystemTime, validity: Duration) -> (String, String) {
        let certificate =
            self.issuing
                .client_for_key(gateway, &self.key, not_before, not_before + validity);
        let chain = format!("{certificate}{}", self.issuing.pem);
        (certificate, chain)
    }
}

/// Answers like the certs service: challenges with fresh nonces, signatures checked against the
/// registered key, certificates of `validity` from the clock's now. Records every request.
struct Stand {
    pki: Pki,
    clock: Clock,
    validity: Duration,
    script: Mutex<VecDeque<Scripted>>,
    requests: Mutex<Vec<(Instant, Step)>>,
    challenge: Mutex<Option<Challenge>>,
    nonces: AtomicU64,
}

impl Stand {
    fn new(pki: Pki, clock: Clock, validity: Duration) -> Arc<Self> {
        Arc::new(Self {
            pki,
            clock,
            validity,
            script: Mutex::new(VecDeque::new()),
            requests: Mutex::new(Vec::new()),
            challenge: Mutex::new(None),
            nonces: AtomicU64::new(0),
        })
    }

    fn then(&self, scripted: Scripted) {
        self.script.lock().unwrap().push_back(scripted);
    }

    fn requests(&self) -> Vec<(Instant, Step)> {
        self.requests.lock().unwrap().clone()
    }

    fn answer(&self, request: &RenewalRequest) -> Answer {
        let next = self.script.lock().unwrap().pop_front();
        let tamper = match next {
            Some(Scripted::Answer(answer)) => return answer,
            Some(Scripted::Tamper(tamper)) => Some(tamper),
            None => None,
        };
        match request {
            RenewalRequest::Challenge(request) => {
                let mut nonce = [0_u8; 32];
                nonce[..8]
                    .copy_from_slice(&self.nonces.fetch_add(1, Ordering::Relaxed).to_be_bytes());
                let challenge = Challenge {
                    hardware_id: request.hardware_id.clone(),
                    purpose: Purpose::CertRenewal,
                    nonce: base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(nonce),
                    expires: 2_000_000_000,
                };
                *self.challenge.lock().unwrap() = Some(challenge.clone());
                json(200, &challenge)
            }
            RenewalRequest::Renew(request) => {
                let Some(challenge) = self.challenge.lock().unwrap().take() else {
                    return json(409, &"challenge_unknown");
                };
                assert_eq!(request.nonce, challenge.nonce);
                let signature = base64::engine::general_purpose::STANDARD
                    .decode(&request.signature)
                    .unwrap();
                let verified =
                    UnparsedPublicKey::new(&ECDSA_P256_SHA256_ASN1, self.pki.key.public_key_raw())
                        .verify(&challenge.signing_input(), &signature);
                if verified.is_err() {
                    return json(401, &"signature_refused");
                }
                // Five minutes back for clock skew, as the certs service does.
                let not_before = (self.clock)() - Duration::from_secs(300);
                let (certificate, certificate_chain) =
                    self.pki
                        .chain(request.hardware_id.as_str(), not_before, self.validity);
                let mut renewed = RenewedCertificate {
                    hardware_id: request.hardware_id.clone(),
                    serial: "01".to_owned(),
                    ca_key_id: "00".to_owned(),
                    not_before: String::new(),
                    not_after: String::new(),
                    certificate,
                    certificate_chain,
                    root_certificate: self.pki.root.pem.clone(),
                };
                if let Some(tamper) = tamper {
                    tamper(&mut renewed, &self.pki);
                }
                json(201, &renewed)
            }
        }
    }
}

impl Endpoint for Arc<Stand> {
    async fn post(&self, request: &RenewalRequest) -> io::Result<Answer> {
        let step = match request {
            RenewalRequest::Challenge(_) => Step::Challenge,
            RenewalRequest::Renew(_) => Step::Renew,
        };
        self.requests.lock().unwrap().push((Instant::now(), step));
        Ok(self.answer(request))
    }
}

fn json(status: u16, body: &impl serde::Serialize) -> Answer {
    Answer {
        status,
        retry_after: None,
        body: serde_json::to_vec(body).unwrap(),
    }
}

fn refusal(status: u16, retry_after: Option<u64>) -> Scripted {
    Scripted::Answer(Answer {
        status,
        retry_after: retry_after.map(Duration::from_secs),
        body: b"{}".to_vec(),
    })
}

/// A wall clock that starts at `start` and runs with tokio's (possibly paused) clock.
fn clock_from(start: SystemTime) -> Clock {
    let origin = Instant::now();
    Arc::new(move || start + origin.elapsed())
}

/// A gateway with a certificate from `pki` valid from `not_before` for `validity`, its files in
/// `dir` (the certificate file modified at `not_before`, as if put in place when it was issued),
/// and a renewal of it through `stand`.
fn renewal(
    dir: &ScratchDir,
    stand: &Arc<Stand>,
    not_before: SystemTime,
    validity: Duration,
) -> Renewal<Arc<Stand>> {
    let (_, chain) = stand.pki.chain(GATEWAY, not_before, validity);
    let certificate = dir.write("gateway.pem", &chain);
    fs::set_permissions(&certificate, Permissions::from_mode(0o640)).unwrap();
    set_modified(&certificate, not_before);
    dir.write("gateway.key", stand.pki.key.serialize_pem());
    dir.write("broker-ca.pem", &stand.pki.root.pem);
    reload(dir, stand)
}

fn set_modified(path: &Path, at: SystemTime) {
    File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(at)
        .unwrap();
}

/// The renewal of the gateway whose files are in `dir`, as a restarted agent loads it.
fn reload(dir: &ScratchDir, stand: &Arc<Stand>) -> Renewal<Arc<Stand>> {
    let certificate = dir.path().join("gateway.pem");
    let key = dir.path().join("gateway.key");
    let ca = dir.path().join("broker-ca.pem");
    let identity = identity::load(&certificate, &KeySource::File { path: key }).unwrap();
    let trust = BrokerTrust::load(&ca).unwrap();
    let root_crl = tls::RootCrl::load(None, &trust, unix_time(SystemTime::now())).unwrap();
    let inner = InnerConfig::new(trust.clone(), root_crl, identity.clone()).unwrap();
    Renewal::new(
        Arc::clone(stand),
        RenewalSettings::default(),
        Credentials {
            identity,
            certificate,
            trust,
            inner,
        },
    )
    .with_clock(Arc::clone(&stand.clock))
    .with_jitter(|| u64::MAX)
}

fn days_ago(days: u64) -> SystemTime {
    SystemTime::now() - DAY * u32::try_from(days).unwrap()
}

#[test]
fn renewal_is_due_with_the_configured_share_of_the_validity_left() {
    let identity = |not_before: u64, not_after: u64| Identity {
        gateway: GatewayId::new(GATEWAY).unwrap(),
        chain: Vec::new(),
        key: Arc::clone(&*KEY),
        not_before: UnixMillis::new(not_before),
        not_after: UnixMillis::new(not_after),
    };
    let ninety_days = 90 * 86_400_000;
    let start = 1_800_000_000_000;
    let certificate = identity(start, start + ninety_days);
    // 33 % of 90 days is 29.7 days.
    assert_eq!(
        due_at(&certificate, 33).get(),
        start + ninety_days - 29 * 86_400_000 - 60_480_000
    );
    assert_eq!(due_at(&certificate, 80).get(), start + 18 * 86_400_000);
    assert_eq!(
        share(&certificate, 33),
        Duration::from_millis(ninety_days * 67 / 100)
    );
    // A certificate that ends before it starts is due at once and has no share.
    let backwards = identity(start, start - 1);
    assert_eq!(due_at(&backwards, 33).get(), start - 1);
    assert_eq!(share(&backwards, 33), Duration::ZERO);
}

static KEY: std::sync::LazyLock<Arc<dyn SigningKey>> = std::sync::LazyLock::new(|| {
    let key = KeyPair::generate().unwrap();
    let der =
        <rustls_pki_types::PrivateKeyDer<'_> as rustls_pki_types::pem::PemObject>::from_pem_slice(
            key.serialize_pem().as_bytes(),
        )
        .unwrap();
    tls::provider().key_provider.load_private_key(der).unwrap()
});

#[test]
fn retries_wait_for_retry_after_and_then_the_backoff() {
    let cap = Duration::from_secs(1800);
    let settings = BackoffSettings {
        base: Duration::from_secs(30),
        cap,
    };
    let refused = |status, retry_after: Option<u64>| RenewalError::Refused {
        step: Step::Challenge,
        status,
        retry_after: retry_after.map(Duration::from_secs),
    };
    let unreachable = RenewalError::Unreachable(io::Error::other("down"));
    // The longest draw: each delay is its ceiling, which doubles up to the cap.
    let mut backoff = Backoff::new(settings);
    let ceilings: Vec<Duration> = (0..8)
        .map(|_| retry_delay(&unreachable, &mut backoff, u64::MAX, cap))
        .collect();
    for pair in ceilings.windows(2) {
        assert!(pair[0] <= pair[1], "{ceilings:?}");
    }
    assert!(ceilings[0] <= Duration::from_secs(30) && ceilings[0] > Duration::from_secs(29));
    assert!(ceilings[7] <= cap && ceilings[7] > cap - Duration::from_secs(1));
    // The shortest draw never goes below the floor.
    let mut backoff = Backoff::new(settings);
    assert_eq!(retry_delay(&unreachable, &mut backoff, 0, cap), MIN_DELAY);
    // 429 and 503: at least Retry-After, plus the backoff.
    let mut backoff = Backoff::new(settings);
    let delay = retry_delay(&refused(503, Some(60)), &mut backoff, 0, cap);
    assert_eq!(delay, Duration::from_secs(60) + MIN_DELAY);
    let delay = retry_delay(&refused(429, Some(5)), &mut backoff, u64::MAX, cap);
    assert!(
        delay >= Duration::from_secs(5) + Duration::from_secs(59),
        "{delay:?}"
    );
    assert!(
        delay <= Duration::from_secs(5) + Duration::from_secs(60),
        "{delay:?}"
    );
    // A Retry-After beyond the cap counts as the cap.
    let mut backoff = Backoff::new(settings);
    let delay = retry_delay(&refused(503, Some(10_000_000)), &mut backoff, 0, cap);
    assert_eq!(delay, cap + MIN_DELAY);
    // Retry-After on another status, or none on 503, is the backoff alone.
    let mut backoff = Backoff::new(settings);
    assert_eq!(
        retry_delay(&refused(403, Some(600)), &mut backoff, 0, cap),
        MIN_DELAY
    );
    assert_eq!(
        retry_delay(&refused(503, None), &mut backoff, 0, cap),
        MIN_DELAY
    );
}

#[test]
fn retry_after_is_read_only_as_seconds() {
    let read = |value: &str| {
        let mut headers = hyper::HeaderMap::new();
        headers.insert(
            hyper::header::RETRY_AFTER,
            hyper::header::HeaderValue::from_str(value).unwrap(),
        );
        https::retry_after(&headers)
    };
    assert_eq!(read("30"), Some(Duration::from_secs(30)));
    assert_eq!(read(" 1 "), Some(Duration::from_secs(1)));
    assert_eq!(read("0"), Some(Duration::ZERO));
    assert_eq!(
        read("99999999999999999999999"),
        Some(Duration::from_secs(u64::MAX))
    );
    assert_eq!(read("Wed, 21 Oct 2026 07:28:00 GMT"), None);
    assert_eq!(read("-5"), None);
    assert_eq!(read("1.5"), None);
    assert_eq!(read(""), None);
    assert_eq!(https::retry_after(&hyper::HeaderMap::new()), None);
}

#[test]
fn the_certificate_file_is_replaced_atomically_with_its_permissions() {
    let dir = ScratchDir::new("renewal-write");
    let path = dir.write("gateway.pem", "old");
    fs::set_permissions(&path, Permissions::from_mode(0o640)).unwrap();
    // A temporary file left by an earlier power cut is overwritten.
    dir.write("gateway.pem.renewal.tmp", "half");
    let modified = SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000);
    write_atomically(&path, b"new", modified).unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), "new");
    assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), modified);
    let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o640, "mode {mode:o}");
    assert!(
        !dir.path().join("gateway.pem.renewal.tmp").exists(),
        "the temporary file is renamed, not left behind"
    );
    // No file yet: 0600.
    let fresh = dir.path().join("fresh.pem");
    write_atomically(&fresh, b"first", SystemTime::now()).unwrap();
    let mode = fs::metadata(&fresh).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "mode {mode:o}");
}

#[test]
fn the_write_waits_for_the_directory_lock_that_provision_install_holds() {
    let dir = ScratchDir::new("renewal-write-lock");
    let path = dir.write("gateway.pem", "old");
    // What `provision install` does: lock the directory, then write the certificate.
    let held = File::open(dir.path()).unwrap();
    held.lock().unwrap();
    let (started, starting) = std::sync::mpsc::channel();
    let writer = {
        let path = path.clone();
        std::thread::spawn(move || {
            started.send(()).unwrap();
            write_atomically(&path, b"renewed", SystemTime::now())
        })
    };
    starting.recv().unwrap();
    fs::write(&path, "installed").unwrap();
    // Still waiting for the lock, whatever the timing: it cannot have finished.
    assert!(
        !writer.is_finished(),
        "the renewal wrote while the lock was held"
    );
    held.unlock().unwrap();
    writer.join().unwrap().unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), "renewed");
}

#[test]
fn a_failed_write_leaves_the_old_certificate_file() {
    let dir = ScratchDir::new("renewal-write-fails");
    let path = dir.write("gateway.pem", "old");
    // The temporary file's name is taken by a directory that cannot be removed.
    let blocked = dir.path().join("gateway.pem.renewal.tmp");
    fs::create_dir(&blocked).unwrap();
    fs::write(blocked.join("inside"), "x").unwrap();
    assert!(matches!(
        write_atomically(&path, b"new", SystemTime::now()),
        Err(WriteError::NotReplaced(_))
    ));
    assert_eq!(fs::read_to_string(&path).unwrap(), "old");
    // A directory that does not exist.
    let missing = dir.path().join("missing").join("gateway.pem");
    assert!(matches!(
        write_atomically(&missing, b"new", SystemTime::now()),
        Err(WriteError::NotReplaced(_))
    ));
    assert!(!missing.exists());
}

#[test]
fn pem_chains_round_trip_and_only_certificates_are_accepted() {
    let pki = Pki::new();
    let (certificate, chain) = pki.chain(GATEWAY, SystemTime::now(), DAY);
    let parsed = certificates(&chain).unwrap();
    assert_eq!(parsed.len(), 2);
    assert_eq!(certificates(&pem_chain(&parsed)).unwrap(), parsed);
    assert_eq!(certificates(&certificate).unwrap().len(), 1);
    assert!(certificates("").is_err());
    assert!(certificates("not PEM").is_err());
    let with_key = format!("{chain}{}", pki.key.serialize_pem());
    assert!(certificates(&with_key).is_err());
    // Every line of the written PEM is at most 64 characters.
    assert!(pem_chain(&parsed).lines().all(|line| line.len() <= 64));
}

#[tokio::test]
async fn a_renewal_swaps_in_the_new_certificate_everywhere() {
    let dir = ScratchDir::new("renewal-swap");
    let stand = Stand::new(Pki::new(), Arc::new(SystemTime::now), 90 * DAY);
    let mut renewal = renewal(&dir, &stand, days_ago(80), 90 * DAY);
    let path = dir.path().join("gateway.pem");
    let old_config = renewal.credentials.inner.current().unwrap();
    let old_not_after = renewal.identity().not_after;
    let mut version = renewal.credentials();
    let mut not_after = renewal.certificate_not_after();
    assert_eq!(*version.borrow_and_update(), 0);

    let renewed = renewal.renew().await.unwrap();

    assert!(renewed.not_after > old_not_after);
    assert_eq!(renewed.gateway.as_str(), GATEWAY);
    assert_eq!(*version.borrow_and_update(), 1);
    assert_eq!(*not_after.borrow_and_update(), renewed.not_after);
    assert!(!Arc::ptr_eq(
        &old_config,
        &renewal.credentials.inner.current().unwrap()
    ));
    assert_eq!(renewal.identity().not_after, renewed.not_after);
    // The file holds the new chain (certificate, issuing CA) with the old permissions, and the
    // agent would load it after a restart.
    let written = certificates(&fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(written, renewed.chain);
    assert_eq!(written.len(), 2);
    let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o640, "mode {mode:o}");
    let key = dir.path().join("gateway.key");
    let reloaded = identity::load(&path, &KeySource::File { path: key }).unwrap();
    assert_eq!(reloaded.not_after, renewed.not_after);
    let steps: Vec<Step> = stand.requests().into_iter().map(|(_, step)| step).collect();
    assert_eq!(steps, [Step::Challenge, Step::Renew]);
    assert_eq!(RENEWAL_PATH, "/certs/renew");
}

/// The answers the agent must not use, each with what it must say.
fn tampered() -> Vec<(&'static str, Scripted)> {
    let tamper = |change: fn(&mut RenewedCertificate, &Pki)| Scripted::Tamper(Box::new(change));
    vec![
        (
            "it is for hardware ID tg-other",
            tamper(|renewed, _| renewed.hardware_id = GatewayId::new("tg-other").unwrap()),
        ),
        (
            "CN",
            tamper(|renewed, pki| {
                let (certificate, chain) = pki.chain("tg-other", SystemTime::now(), DAY);
                renewed.certificate = certificate;
                renewed.certificate_chain = chain;
            }),
        ),
        (
            "does not belong",
            tamper(|renewed, pki| {
                let other = KeyPair::generate().unwrap();
                let certificate = pki.issuing.client_for_key(
                    GATEWAY,
                    &other,
                    SystemTime::now(),
                    SystemTime::now() + DAY,
                );
                renewed.certificate_chain = format!("{certificate}{}", pki.issuing.pem);
                renewed.certificate = certificate;
            }),
        ),
        (
            "not one of the roots",
            tamper(|renewed, _| renewed.root_certificate = Authority::new("stranger").pem),
        ),
        (
            "does not pass as a client certificate",
            tamper(|renewed, pki| {
                // A chain from a CA the agent does not trust, with our root named as its root.
                let stranger = Authority::new("stranger").issuing("stranger issuing");
                let certificate = stranger.client_for_key(
                    GATEWAY,
                    &pki.key,
                    SystemTime::now(),
                    SystemTime::now() + DAY,
                );
                renewed.certificate_chain = format!("{certificate}{}", stranger.pem);
                renewed.certificate = certificate;
            }),
        ),
        (
            "not the first certificate",
            tamper(|renewed, pki| renewed.certificate = pki.issuing.pem.clone()),
        ),
        (
            // A certificate that starts in an hour: the broker would refuse it until then.
            "more than 300 s after this clock's now",
            tamper(|renewed, pki| {
                let start = SystemTime::now() + Duration::from_secs(3600);
                let (certificate, chain) = pki.chain(GATEWAY, start, 90 * DAY);
                renewed.certificate = certificate;
                renewed.certificate_chain = chain;
            }),
        ),
        (
            "it ended at",
            tamper(|renewed, pki| {
                let (certificate, chain) = pki.chain(GATEWAY, days_ago(2), DAY);
                renewed.certificate = certificate;
                renewed.certificate_chain = chain;
            }),
        ),
        (
            "not issued after",
            tamper(|renewed, pki| {
                let (certificate, chain) = pki.chain(GATEWAY, days_ago(85), 90 * DAY);
                renewed.certificate = certificate;
                renewed.certificate_chain = chain;
            }),
        ),
        (
            "certificate_chain",
            tamper(|renewed, pki| {
                renewed.certificate_chain =
                    format!("{}{}", renewed.certificate_chain, pki.key.serialize_pem());
            }),
        ),
        (
            "more than 4",
            tamper(|renewed, pki| {
                renewed.certificate_chain = format!(
                    "{}{}{}{}",
                    renewed.certificate_chain, pki.issuing.pem, pki.issuing.pem, pki.issuing.pem
                );
            }),
        ),
        (
            "HTTP 200 instead of 201",
            Scripted::Answer(Answer {
                status: 200,
                retry_after: None,
                body: b"{}".to_vec(),
            }),
        ),
        (
            "not the expected JSON",
            Scripted::Answer(Answer {
                status: 201,
                retry_after: None,
                body: b"<html>".to_vec(),
            }),
        ),
    ]
}

#[tokio::test]
async fn a_renewed_certificate_that_fails_a_check_changes_nothing() {
    for (expected, scripted) in tampered() {
        let dir = ScratchDir::new("renewal-refuses");
        let stand = Stand::new(Pki::new(), Arc::new(SystemTime::now), 90 * DAY);
        let mut renewal = renewal(&dir, &stand, days_ago(80), 90 * DAY);
        let path = dir.path().join("gateway.pem");
        let before = fs::read(&path).unwrap();
        let old_config = renewal.credentials.inner.current().unwrap();
        // The challenge is answered as usual; the scripted answer is the renew step's.
        stand.then(refusal_free_challenge());
        stand.then(scripted);
        let error = renewal.renew().await.unwrap_err();
        assert!(error.to_string().contains(expected), "{expected}: {error}");
        assert_eq!(
            fs::read(&path).unwrap(),
            before,
            "{expected}: the file changed"
        );
        assert_eq!(*renewal.credentials().borrow(), 0, "{expected}");
        assert!(Arc::ptr_eq(
            &old_config,
            &renewal.credentials.inner.current().unwrap()
        ));
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o640);
    }
}

/// A placeholder that lets the stand answer the challenge itself.
fn refusal_free_challenge() -> Scripted {
    Scripted::Tamper(Box::new(|_, _| {}))
}

#[tokio::test]
async fn a_challenge_the_agent_did_not_ask_for_is_not_signed() {
    let challenge = |hardware_id: &str, nonce: &str| {
        Scripted::Answer(json(
            200,
            &Challenge {
                hardware_id: GatewayId::new(hardware_id).unwrap(),
                purpose: Purpose::CertRenewal,
                nonce: nonce.to_owned(),
                expires: 2_000_000_000,
            },
        ))
    };
    let good_nonce = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([7_u8; 32]);
    let cases = [
        (challenge("tg-other", &good_nonce), "another hardware ID"),
        (challenge(GATEWAY, "short"), "nonce is malformed"),
        (
            Scripted::Answer(json(201, &"created")),
            "HTTP 201 instead of 200",
        ),
        (
            Scripted::Answer(json(200, &"nonsense")),
            "not the expected JSON",
        ),
    ];
    for (scripted, expected) in cases {
        let dir = ScratchDir::new("renewal-challenge");
        let stand = Stand::new(Pki::new(), Arc::new(SystemTime::now), 90 * DAY);
        let mut renewal = renewal(&dir, &stand, days_ago(80), 90 * DAY);
        stand.then(scripted);
        let error = renewal.renew().await.unwrap_err();
        assert!(error.to_string().contains(expected), "{expected}: {error}");
        let steps: Vec<Step> = stand.requests().into_iter().map(|(_, step)| step).collect();
        assert_eq!(steps, [Step::Challenge], "{expected}: nothing was redeemed");
    }
}

#[tokio::test]
async fn a_certificate_starting_within_the_clock_skew_is_used() {
    let dir = ScratchDir::new("renewal-skew");
    let stand = Stand::new(Pki::new(), Arc::new(SystemTime::now), 90 * DAY);
    let mut renewal = renewal(&dir, &stand, days_ago(80), 90 * DAY);
    // The issuer's clock is a few minutes ahead of the gateway's.
    stand.then(refusal_free_challenge());
    stand.then(starting_at(SystemTime::now() + Duration::from_secs(200)));
    let renewed = renewal.renew().await.unwrap();
    assert_eq!(*renewal.credentials().borrow(), 1);
    assert_eq!(renewal.identity().not_before, renewed.not_before);
}

/// A renew answer whose certificate starts at `start` and is valid for 90 days.
fn starting_at(start: SystemTime) -> Scripted {
    Scripted::Tamper(Box::new(move |renewed, pki| {
        let (certificate, chain) = pki.chain(GATEWAY, start, 90 * DAY);
        renewed.certificate = certificate;
        renewed.certificate_chain = chain;
    }))
}

#[tokio::test]
async fn the_replay_floor_is_the_start_of_the_last_proven_certificate() {
    let dir = ScratchDir::new("renewal-replay-floor");
    let stand = Stand::new(Pki::new(), Arc::new(SystemTime::now), 90 * DAY);
    let mut renewal = renewal(&dir, &stand, days_ago(80), 90 * DAY);
    // A renewal installs a certificate from five minutes ago, which no connection proves.
    renewal.renew().await.unwrap();
    // So an answer older than that one but newer than the last proven one is still accepted:
    // a renewed certificate that never worked does not raise the floor.
    stand.then(refusal_free_challenge());
    stand.then(starting_at(days_ago(40)));
    renewal.renew().await.unwrap();
    // Once a connection proves it, nothing that starts before it is accepted.
    renewal.mark_proven();
    stand.then(refusal_free_challenge());
    stand.then(starting_at(days_ago(60)));
    let error = renewal.renew().await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("not issued after the last proven"),
        "{error}"
    );
    assert_eq!(*renewal.credentials().borrow(), 2);
}

#[test]
fn an_unusable_previous_certificate_is_removed_at_startup() {
    let pki = Pki::new();
    let now = SystemTime::now();
    let stranger = Authority::new("stranger").issuing("stranger issuing");
    let other_key = KeyPair::generate().unwrap();
    let cases = [
        ("not a certificate", "garbage".to_owned()),
        (
            "another gateway's",
            pki.chain("tg-other", days_ago(10), 90 * DAY).1,
        ),
        (
            "not older than the one in use",
            pki.chain(GATEWAY, now - Duration::from_secs(100), 90 * DAY)
                .1,
        ),
        ("for another key", {
            let certificate =
                pki.issuing
                    .client_for_key(GATEWAY, &other_key, days_ago(10), now + 80 * DAY);
            format!("{certificate}{}", pki.issuing.pem)
        }),
        ("under another CA", {
            let certificate =
                stranger.client_for_key(GATEWAY, &pki.key, days_ago(10), now + 80 * DAY);
            format!("{certificate}{}", stranger.pem)
        }),
    ];
    let stand = Stand::new(pki, Arc::new(SystemTime::now), 90 * DAY);
    for (case, contents) in cases {
        let dir = ScratchDir::new("renewal-previous");
        drop(renewal(
            &dir,
            &stand,
            now - Duration::from_secs(300),
            90 * DAY,
        ));
        let kept = previous_path(&dir.path().join("gateway.pem"));
        fs::write(&kept, contents).unwrap();
        let renewal = reload(&dir, &stand);
        assert!(renewal.previous.is_none(), "{case}: kept");
        assert!(!kept.exists(), "{case}: not removed");
    }
    // A usable one is kept.
    let dir = ScratchDir::new("renewal-previous");
    drop(renewal(
        &dir,
        &stand,
        now - Duration::from_secs(300),
        90 * DAY,
    ));
    let kept = previous_path(&dir.path().join("gateway.pem"));
    fs::write(&kept, stand.pki.chain(GATEWAY, days_ago(10), 90 * DAY).1).unwrap();
    let renewal = reload(&dir, &stand);
    assert!(renewal.previous.is_some());
    assert!(kept.exists());
}

/// The link as the renewal sees it: its state and its connection attempts.
struct TestLink {
    state: watch::Sender<LinkState>,
    attempts: watch::Sender<Option<Attempt>>,
}

impl TestLink {
    fn new(
        state: LinkState,
        attempt: Option<Attempt>,
    ) -> (
        Self,
        watch::Receiver<LinkState>,
        watch::Receiver<Option<Attempt>>,
    ) {
        let (state, state_receiver) = watch::channel(state);
        let (attempts, attempts_receiver) = watch::channel(attempt);
        (Self { state, attempts }, state_receiver, attempts_receiver)
    }

    /// Connected with `session`, whose attempt presented credentials version `credentials`: the
    /// attempt first, as the link reports it.
    fn connect(&self, session: &str, credentials: u64) {
        self.attempts
            .send_replace(Some(attempt(session, credentials)));
        self.state.send_replace(connected(session));
    }

    fn set(&self, state: LinkState) {
        self.state.send_replace(state);
    }
}

fn attempt(session: &str, credentials: u64) -> Attempt {
    Attempt {
        session: SessionId::new(session).unwrap(),
        credentials,
    }
}

/// Runs `renewal` with a link that never connects; returns the link, the refusal sender and the
/// shutdown token.
fn start(
    renewal: Renewal<Arc<Stand>>,
) -> (
    TestLink,
    watch::Sender<Option<Refusal>>,
    CancellationToken,
    tokio::task::JoinHandle<()>,
) {
    let (state, link, attempts) = TestLink::new(LinkState::Connecting, None);
    let (refusals, refused) = watch::channel(None);
    let shutdown = CancellationToken::new();
    let task = tokio::spawn(renewal.run(link, attempts, refused, shutdown.clone()));
    (state, refusals, shutdown, task)
}

/// The broker refused credentials `credentials` (an alert or CONNACK 0x86/0x87).
fn explicit(credentials: u64) -> Option<Refusal> {
    Some(Refusal {
        credentials,
        explicit: true,
    })
}

/// The inner mTLS kept failing for credentials `credentials` while the cloud was reachable.
fn inferred(credentials: u64) -> Option<Refusal> {
    Some(Refusal {
        credentials,
        explicit: false,
    })
}

fn connected(session: &str) -> LinkState {
    LinkState::Connected {
        session: SessionId::new(session).unwrap(),
    }
}

/// The certificates in the PEM file at `path`.
fn chain_in(path: &Path) -> Vec<CertificateDer<'static>> {
    certificates(&fs::read_to_string(path).unwrap()).unwrap()
}

/// Asserts that `waited` is about `expected` (within a second either way).
fn about(waited: Duration, expected: Duration, what: &str) {
    assert!(
        waited + Duration::from_secs(1) >= expected && waited <= expected + Duration::from_secs(1),
        "{what}: waited {waited:?}, expected about {expected:?}"
    );
}

/// Waits (on the paused clock) until the stand has seen `count` requests.
async fn requests(stand: &Stand, count: usize, limit: Duration) -> Vec<(Instant, Step)> {
    let started = Instant::now();
    loop {
        let requests = stand.requests();
        if requests.len() >= count {
            return requests;
        }
        assert!(
            started.elapsed() < limit,
            "waited {limit:?} for {count} renewal requests, saw {requests:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test(start_paused = true)]
async fn renewal_waits_for_the_schedule_and_then_for_the_new_certificate_s_schedule() {
    let dir = ScratchDir::new("renewal-schedule");
    let start_time = SystemTime::now();
    let validity = Duration::from_secs(3000);
    let stand = Stand::new(Pki::new(), clock_from(start_time), validity);
    // Issued 1000 s ago: due when 990 s are left, at 1010 s from now.
    let renewal = renewal(
        &dir,
        &stand,
        start_time - Duration::from_secs(1000),
        validity,
    );
    let origin = Instant::now();
    let mut version = renewal.credentials();
    let (_state, _refusals, shutdown, task) = start(renewal);

    let first = requests(&stand, 2, Duration::from_secs(2000)).await;
    let at = first[0].0 - origin;
    assert!(
        at >= Duration::from_secs(1009) && at <= Duration::from_secs(1012),
        "the first renewal came at {at:?}"
    );
    bounded_version(&mut version, 1).await;
    // The new certificate (from 300 s back, 3000 s) is due by the wall clock 1710 s after its
    // renewal, but not before two thirds of its validity (2010 s) have passed since then by the
    // monotonic clock: the later of the two.
    let second = requests(&stand, 4, Duration::from_secs(4000)).await;
    let gap = second[2].0 - first[1].0;
    assert!(
        gap >= Duration::from_secs(2009) && gap <= Duration::from_secs(2012),
        "the second renewal came {gap:?} after the first"
    );
    shutdown.cancel();
    task.await.unwrap();
}

async fn bounded_version(version: &mut watch::Receiver<u64>, wanted: u64) {
    tokio::time::timeout(
        Duration::from_secs(60),
        version.wait_for(|version| *version >= wanted),
    )
    .await
    .unwrap_or_else(|_| panic!("waited for credentials version {wanted}"))
    .unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_clock_ahead_renews_once_and_not_again_after_a_restart() {
    let dir = ScratchDir::new("renewal-clock-ahead");
    let validity = Duration::from_secs(3000);
    // The certificates are issued by the real time; the gateway's clock is 2000 s ahead, so the
    // certificate in use (issued 100 s ago, from five minutes before that like every certificate
    // of the certs service) looks due to it.
    let now = SystemTime::now();
    let ahead = Duration::from_secs(2000);
    let stand = Stand::new(Pki::new(), clock_from(now), validity);
    let renewal = renewal(&dir, &stand, now - Duration::from_secs(400), validity)
        .with_clock(clock_from(now + ahead));
    let origin = Instant::now();
    let mut version = renewal.credentials();
    let (_state, _refusals, shutdown, task) = start(renewal);
    let first = requests(&stand, 2, Duration::from_secs(60)).await;
    assert!(first[0].0 - origin < Duration::from_secs(1));
    bounded_version(&mut version, 1).await;
    shutdown.cancel();
    task.await.unwrap();

    // A restart with the same clock: the renewed certificate looks due by its validity, but it
    // was put in place just now by this clock (the file's modification time), so it waits until
    // two thirds of its validity have passed since then.
    let elapsed = Instant::now() - origin;
    let renewal = reload(&dir, &stand).with_clock(clock_from(now + ahead + elapsed));
    let (_state, _refusals, shutdown, task) = start(renewal);
    tokio::time::sleep(Duration::from_secs(1900)).await;
    assert_eq!(stand.requests().len(), 2, "renewed again after the restart");
    let second = requests(&stand, 4, Duration::from_secs(400)).await;
    about(
        second[2].0 - first[1].0,
        Duration::from_secs(2010),
        "the second renewal after the first",
    );
    shutdown.cancel();
    task.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn failed_attempts_back_off_and_obey_retry_after() {
    let dir = ScratchDir::new("renewal-retries");
    let now = SystemTime::now();
    let stand = Stand::new(Pki::new(), clock_from(now), 90 * DAY);
    // Expired by the gateway's clock: due at once.
    let renewal = renewal(&dir, &stand, now - 100 * DAY, 90 * DAY);
    stand.then(refusal(503, Some(30)));
    stand.then(refusal(429, Some(5)));
    stand.then(refusal(500, None));
    stand.then(refusal(503, Some(1_000_000)));
    let mut version = renewal.credentials();
    let (_state, _refusals, shutdown, task) = start(renewal);
    let seen = requests(&stand, 6, Duration::from_secs(10_000)).await;
    bounded_version(&mut version, 1).await;
    let steps: Vec<Step> = seen.iter().map(|(_, step)| *step).collect();
    // Every retry starts over with a challenge.
    assert_eq!(
        steps,
        [
            Step::Challenge,
            Step::Challenge,
            Step::Challenge,
            Step::Challenge,
            Step::Challenge,
            Step::Renew
        ]
    );
    let gaps: Vec<Duration> = seen.windows(2).map(|pair| pair[1].0 - pair[0].0).collect();
    // The jitter is fixed at its largest draw, so each delay is Retry-After (capped at 1800 s)
    // plus the backoff's ceiling: 30 s, 60 s, 120 s, 240 s.
    let expected = [30 + 30, 5 + 60, 120, 1800 + 240];
    for (index, (gap, seconds)) in gaps.iter().zip(expected).enumerate() {
        let wanted = Duration::from_secs(seconds);
        assert!(
            *gap <= wanted && *gap + Duration::from_secs(1) >= wanted,
            "retry {index}: waited {gap:?}, expected about {wanted:?} ({gaps:?})"
        );
    }
    shutdown.cancel();
    task.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_refused_certificate_is_renewed_at_once_and_a_refused_renewal_waits() {
    let dir = ScratchDir::new("renewal-refused");
    let now = SystemTime::now();
    let stand = Stand::new(Pki::new(), clock_from(now), 90 * DAY);
    // Fresh: not due for 60 days.
    let renewal = renewal(&dir, &stand, now - Duration::from_secs(300), 90 * DAY);
    let mut version = renewal.credentials();
    let (state, refusals, shutdown, task) = start(renewal);
    tokio::time::sleep(Duration::from_secs(600)).await;
    assert!(stand.requests().is_empty(), "renewed before it was due");

    // The broker refuses the certificate (revoked): renewal at once.
    let refused_at = Instant::now();
    refusals.send_replace(explicit(0));
    let first = requests(&stand, 2, Duration::from_secs(60)).await;
    assert!(first[0].0 - refused_at < Duration::from_secs(1));
    bounded_version(&mut version, 1).await;

    // A refusal of an older version changes nothing.
    refusals.send_replace(explicit(0));
    tokio::time::sleep(Duration::from_secs(600)).await;
    assert_eq!(stand.requests().len(), 2);

    // The broker refuses the renewed certificate too before any connection used it: the next
    // renewal waits for the backoff (its first ceiling, 30 s, with the largest draw).
    let refused_again = Instant::now();
    refusals.send_replace(explicit(1));
    let second = requests(&stand, 4, Duration::from_secs(120)).await;
    about(
        second[2].0 - refused_again,
        Duration::from_secs(30),
        "a refused renewal",
    );
    bounded_version(&mut version, 2).await;

    // A connection that ends before STABLE_AFTER proves nothing: the next refusal waits for the
    // backoff's second ceiling, 60 s.
    state.connect("s-1", 2);
    tokio::time::sleep(STABLE_AFTER - Duration::from_secs(10)).await;
    state.set(LinkState::Connecting);
    let refused_short = Instant::now();
    refusals.send_replace(explicit(2));
    let third = requests(&stand, 6, Duration::from_secs(180)).await;
    about(
        third[4].0 - refused_short,
        Duration::from_secs(60),
        "a refusal after a short connection",
    );
    bounded_version(&mut version, 3).await;
    let refusal_renewal = third[5].0;

    // A connection that lasts STABLE_AFTER proves version 3. A refusal of a proven certificate
    // renews, but no sooner than REFUSAL_RENEWAL_INTERVAL after the last renewal a refusal
    // started, so a broker (or anything posing as one) cannot make the gateway renew in a loop.
    state.connect("s-2", 3);
    tokio::time::sleep(STABLE_AFTER + Duration::from_secs(1)).await;
    refusals.send_replace(explicit(3));
    let fourth = requests(
        &stand,
        8,
        REFUSAL_RENEWAL_INTERVAL + Duration::from_secs(120),
    )
    .await;
    about(
        fourth[6].0 - refusal_renewal,
        REFUSAL_RENEWAL_INTERVAL,
        "a refusal of a proven certificate",
    );
    bounded_version(&mut version, 4).await;
    // The broker refused the certificate it replaced: not worth keeping to go back to.
    assert!(!previous_path(&dir.path().join("gateway.pem")).exists());
    shutdown.cancel();
    task.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn an_inferred_refusal_of_a_certificate_valid_by_this_clock_waits_for_the_backoff() {
    let dir = ScratchDir::new("renewal-inferred");
    let now = SystemTime::now();
    let stand = Stand::new(Pki::new(), clock_from(now), 90 * DAY);
    let renewal = renewal(&dir, &stand, now - Duration::from_secs(300), 90 * DAY);
    let (_state, refusals, shutdown, task) = start(renewal);
    // The inner mTLS kept failing without an alert: maybe the certificate, maybe the broker.
    // The certificate is valid by this clock, so the renewal waits like a retry (30 s).
    let refused_at = Instant::now();
    refusals.send_replace(inferred(0));
    let seen = requests(&stand, 2, Duration::from_secs(120)).await;
    about(
        seen[0].0 - refused_at,
        Duration::from_secs(30),
        "an inferred refusal",
    );
    shutdown.cancel();
    task.await.unwrap();
}

/// A gateway whose certificate (valid for 3000 s from 400 s ago, so due by schedule in 1610 s)
/// a connection up from the start proves, then renewed on schedule: version 1 is in use and the
/// proven certificate is kept.
struct Renewed {
    dir: ScratchDir,
    stand: Arc<Stand>,
    path: PathBuf,
    kept: PathBuf,
    old_chain: Vec<CertificateDer<'static>>,
    old_start: SystemTime,
    version: watch::Receiver<u64>,
    refusals: watch::Sender<Option<Refusal>>,
    shutdown: CancellationToken,
    task: tokio::task::JoinHandle<()>,
    _state: TestLink,
}

async fn proven_and_renewed_on_schedule(name: &str) -> Renewed {
    let dir = ScratchDir::new(name);
    let now = SystemTime::now();
    let stand = Stand::new(Pki::new(), clock_from(now), 90 * DAY);
    let old_start = now - Duration::from_secs(400);
    let renewal = renewal(&dir, &stand, old_start, Duration::from_secs(3000));
    let path = dir.path().join("gateway.pem");
    let kept = previous_path(&path);
    let old_chain = chain_in(&path);
    let mut version = renewal.credentials();
    let (state, link, attempts) = TestLink::new(connected("s-1"), Some(attempt("s-1", 0)));
    let (refusals, refused) = watch::channel(None);
    let shutdown = CancellationToken::new();
    let task = tokio::spawn(renewal.run(link, attempts, refused, shutdown.clone()));
    requests(&stand, 2, Duration::from_secs(2000)).await;
    bounded_version(&mut version, 1).await;
    Renewed {
        dir,
        stand,
        path,
        kept,
        old_chain,
        old_start,
        version,
        refusals,
        shutdown,
        task,
        _state: state,
    }
}

#[tokio::test(start_paused = true)]
async fn a_renewed_certificate_the_broker_refuses_before_it_is_proven_is_rolled_back() {
    let Renewed {
        stand,
        path,
        kept,
        old_chain,
        old_start,
        mut version,
        refusals,
        shutdown,
        task,
        dir: _dir,
        _state,
        ..
    } = proven_and_renewed_on_schedule("renewal-rollback").await;
    // The proven certificate is kept beside the renewed one, with the same permissions.
    assert_ne!(chain_in(&path), old_chain);
    assert_eq!(chain_in(&kept), old_chain);
    let mode = fs::metadata(&kept).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o640, "mode {mode:o}");

    // The broker refuses the renewed certificate before any connection with it lasted (the one
    // that was up when it came in was opened with the old certificate and does not count): the
    // kept one comes back, without a request.
    tokio::time::sleep(STABLE_AFTER * 2).await;
    refusals.send_replace(explicit(1));
    bounded_version(&mut version, 2).await;
    let rolled_back = Instant::now();
    assert_eq!(chain_in(&path), old_chain);
    assert!(!kept.exists(), "the kept certificate is still there");
    let modified = fs::metadata(&path).unwrap().modified().unwrap();
    assert!(
        modified <= old_start && modified + Duration::from_secs(1) > old_start,
        "modified at {modified:?}, the certificate starts at {old_start:?}"
    );
    assert_eq!(stand.requests().len(), 2);

    // The certificate that came back is due by schedule and counts as not proven: the next
    // renewal waits for the backoff (30 s), and the certificate it replaces is not kept.
    let seen = requests(&stand, 4, Duration::from_secs(120)).await;
    about(
        seen[2].0 - rolled_back,
        Duration::from_secs(30),
        "the renewal after a rollback",
    );
    bounded_version(&mut version, 3).await;
    assert!(!kept.exists(), "an unproven certificate was kept");

    // The new certificate is refused as well: no rollback to the unproven one, a renewal after
    // the backoff's next step.
    refusals.send_replace(explicit(3));
    let seen = requests(&stand, 6, Duration::from_secs(300)).await;
    assert_ne!(chain_in(&path), old_chain);
    assert!(seen[4].0 - seen[3].0 > Duration::from_secs(30));
    shutdown.cancel();
    task.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn an_inferred_refusal_of_a_renewed_certificate_does_not_roll_back() {
    let Renewed {
        stand,
        path,
        kept,
        old_chain,
        mut version,
        refusals,
        shutdown,
        task,
        dir: _dir,
        _state,
        ..
    } = proven_and_renewed_on_schedule("renewal-no-inferred-rollback").await;
    // The inner mTLS keeps failing with the renewed certificate, without a word from the broker:
    // no rollback, a renewal after the backoff (30 s), and the proven certificate stays kept.
    let refused_at = Instant::now();
    refusals.send_replace(inferred(1));
    let seen = requests(&stand, 4, Duration::from_secs(120)).await;
    about(
        seen[2].0 - refused_at,
        Duration::from_secs(30),
        "an inferred refusal of a renewed certificate",
    );
    bounded_version(&mut version, 2).await;
    assert_ne!(chain_in(&path), old_chain);
    assert_eq!(chain_in(&kept), old_chain);
    shutdown.cancel();
    task.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_restart_keeps_the_previous_certificate_until_the_renewed_one_is_proven() {
    let dir = ScratchDir::new("renewal-rollback-restart");
    let now = SystemTime::now();
    let stand = Stand::new(Pki::new(), clock_from(now), 90 * DAY);
    // An earlier run renewed and stopped before a connection proved the new certificate.
    drop(renewal(
        &dir,
        &stand,
        now - Duration::from_secs(300),
        90 * DAY,
    ));
    let path = dir.path().join("gateway.pem");
    let kept = previous_path(&path);
    let (_, previous) = stand.pki.chain(GATEWAY, days_ago(10), 90 * DAY);
    fs::write(&kept, &previous).unwrap();
    let renewal = reload(&dir, &stand);
    let mut version = renewal.credentials();
    let (_state, refusals, shutdown, task) = start(renewal);
    refusals.send_replace(explicit(0));
    bounded_version(&mut version, 1).await;
    assert_eq!(chain_in(&path), certificates(&previous).unwrap());
    assert!(!kept.exists());
    assert!(stand.requests().is_empty());
    shutdown.cancel();
    task.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_connection_already_up_at_the_start_proves_the_certificate() {
    // The fixture's link is connected before the renewal starts and never changes: that
    // connection proves the certificate, so the scheduled renewal keeps it.
    let Renewed {
        kept,
        old_chain,
        shutdown,
        task,
        dir: _dir,
        _state,
        ..
    } = proven_and_renewed_on_schedule("renewal-connected-at-start").await;
    assert_eq!(chain_in(&kept), old_chain);
    shutdown.cancel();
    task.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn only_a_connection_whose_attempt_presented_the_renewed_certificate_proves_it() {
    let Renewed {
        stand,
        path,
        kept,
        old_chain,
        mut version,
        refusals,
        shutdown,
        task,
        dir: _dir,
        _state: link,
        ..
    } = proven_and_renewed_on_schedule("renewal-proof-attempts").await;
    // A new session whose attempt read the credentials before version 1 came in (it presented
    // the old certificate) proves nothing however long it lasts: the kept one stays.
    link.connect("s-2", 0);
    tokio::time::sleep(STABLE_AFTER * 3).await;
    assert_eq!(chain_in(&kept), old_chain);

    // The link reconnects with the renewed certificate: that connection proves it, and the kept
    // certificate goes.
    link.connect("s-3", 1);
    tokio::time::sleep(STABLE_AFTER + Duration::from_secs(1)).await;
    assert!(!kept.exists(), "the renewed certificate is not proven");

    // A refusal of the proven certificate renews at once, no rollback.
    let refused_at = Instant::now();
    refusals.send_replace(explicit(1));
    let seen = requests(&stand, 4, Duration::from_secs(60)).await;
    assert!(seen[2].0 - refused_at < Duration::from_secs(1));
    bounded_version(&mut version, 2).await;
    assert_ne!(chain_in(&path), old_chain);
    shutdown.cancel();
    task.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_rollback_that_cannot_be_saved_keeps_the_previous_certificate_for_a_restart() {
    let Renewed {
        dir,
        stand,
        path,
        kept,
        old_chain,
        mut version,
        refusals,
        shutdown,
        task,
        _state,
        ..
    } = proven_and_renewed_on_schedule("renewal-rollback-unsaved").await;
    let renewed_chain = chain_in(&path);

    // The certificate file cannot be written: the temporary file's name is taken by a directory
    // that cannot be removed.
    let blocked = dir.path().join("gateway.pem.renewal.tmp");
    fs::create_dir(&blocked).unwrap();
    fs::write(blocked.join("inside"), "x").unwrap();
    refusals.send_replace(explicit(1));
    bounded_version(&mut version, 2).await;
    // The old certificate is in use, but the file still holds the refused one, so the kept copy
    // stays for a restart.
    assert_eq!(chain_in(&path), renewed_chain);
    assert_eq!(chain_in(&kept), old_chain);
    shutdown.cancel();
    task.await.unwrap();

    // After a restart the refused certificate is loaded, and refused again: the kept one comes
    // back, now saved.
    fs::remove_dir_all(&blocked).unwrap();
    let renewal = reload(&dir, &stand);
    let mut version = renewal.credentials();
    let (_state, refusals, shutdown, task) = start(renewal);
    refusals.send_replace(explicit(0));
    bounded_version(&mut version, 1).await;
    assert_eq!(chain_in(&path), old_chain);
    assert!(!kept.exists());
    assert_eq!(stand.requests().len(), 2);
    shutdown.cancel();
    task.await.unwrap();
}
