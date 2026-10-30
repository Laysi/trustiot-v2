//! The root CRL (Laysi/trustiot-v2#830; README.md, 「根 CRL」): the service adopts the root CRL
//! from the CA certificates ConfigMap only when the loaded root signed it and it is in the format
//! the gateway agent accepts, keeps the one adopted before (checked against the same root) when a
//! new file cannot be adopted, picks up a new file without a restart, and alerts when the root
//! CRL has less than 60 days left or has expired, still adopting it then.
//!
//! The first tests drive `CaState` with files on disk and a fixed clock; the last runs the real
//! service on the product's schema (a throwaway Postgres with TLS, podman or docker; skipped
//! without either unless `CI` is set) and reads its log and its gauges over OTLP. The root CRLs
//! are signed here with the test root, as `certs-ca` will at the root ceremony.

mod support;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use certs::authority::CaState;
use certs::ceremony;
use certs::config::CaFiles;
use certs::root_crl::{ALERT_BEFORE_SECS, RootCrl};
use rcgen::{
    CertificateRevocationListParams, CrlDistributionPoint, CrlIssuingDistributionPoint, CrlScope,
    Issuer, KeyIdMethod, KeyPair, RevokedCertParams, SerialNumber,
};
use support::{FakeCollector, Service, TestCa, now, points, temp_dir, wait_for};
use testkit::product::product_postgres;
use time::OffsetDateTime;
use x509_parser::prelude::FromDer as _;

const DAY: i64 = 86_400;

/// A root that signs root CRLs: the test CA's own root, or another one.
struct Root {
    key: KeyPair,
    pem: String,
}

impl Root {
    fn of(ca: &TestCa) -> Self {
        Self {
            key: KeyPair::from_pem(&ca.root_key.serialize_pem()).expect("the root key"),
            pem: ca.root_pem.clone(),
        }
    }

    /// A root named `name` with a key of its own.
    fn new(name: &str) -> Self {
        let key = ceremony::new_root_key().expect("a root key");
        let pem = ceremony::root_certificate(&key, name, 3650, now()).expect("a root");
        Self { key, pem }
    }

    /// A root CRL numbered `number`, valid from `this_update` to `next_update` (Unix seconds),
    /// revoking the serial `revoked` if given, with an issuing distribution point if `scoped`.
    fn crl(
        &self,
        number: u64,
        this_update: i64,
        next_update: i64,
        revoked: Option<&[u8]>,
        scoped: bool,
    ) -> String {
        let issuer = Issuer::from_ca_cert_pem(&self.pem, &self.key).expect("the root as issuer");
        let at = |seconds| OffsetDateTime::from_unix_timestamp(seconds).expect("a time");
        CertificateRevocationListParams {
            this_update: at(this_update),
            next_update: at(next_update),
            crl_number: SerialNumber::from(number),
            issuing_distribution_point: scoped.then(|| CrlIssuingDistributionPoint {
                distribution_point: CrlDistributionPoint {
                    uris: vec!["http://crl.invalid/root.crl".to_owned()],
                },
                scope: Some(CrlScope::UserCertsOnly),
            }),
            revoked_certs: revoked
                .map(|serial| RevokedCertParams {
                    serial_number: SerialNumber::from_slice(serial),
                    revocation_time: at(this_update),
                    reason_code: None,
                    invalidity_date: None,
                })
                .into_iter()
                .collect(),
            key_identifier_method: KeyIdMethod::Sha256,
        }
        .signed_by(&issuer)
        .expect("a root CRL")
        .pem()
        .expect("PEM")
    }
}

/// The serial of the test CA's first issuing CA certificate, which a root CRL can revoke.
fn issuing_serial(ca: &TestCa) -> Vec<u8> {
    let der = ca.issuing_ders().remove(0);
    let (_, certificate) =
        x509_parser::certificate::X509Certificate::from_der(&der).expect("a certificate");
    certificate.raw_serial().to_vec()
}

/// The CA files of `ca` with a root CRL file beside them.
fn files(ca: &TestCa) -> (CaFiles, PathBuf) {
    let path = ca
        .root_file
        .parent()
        .expect("the CA directory")
        .join("root-crl.pem");
    std::fs::write(&path, "").expect("write an empty root CRL file");
    let files = CaFiles {
        keys_file: ca.keys_file.clone(),
        root_certificate_file: ca.root_file.clone(),
        certificates_file: ca.certificates_file.clone(),
        root_crl_file: Some(path.clone()),
        reload_interval: Duration::from_secs(30),
    };
    (files, path)
}

/// Writes `contents` to `path` the way the kubelet updates a ConfigMap volume: a new file
/// renamed over the old one, so a reader sees one or the other.
fn replace(path: &Path, contents: &str) {
    let next = path.with_extension("next");
    std::fs::write(&next, contents).expect("write the new file");
    std::fs::rename(&next, path).expect("replace the file");
}

/// The adopted root CRL's number.
fn adopted(state: &CaState) -> Option<String> {
    state.root_crl().map(|crl| crl.number().to_owned())
}

#[test]
fn a_root_crl_is_adopted_only_when_the_root_signed_it_in_the_agents_format() {
    let (capture, _guard) = Capture::install();
    let ca = TestCa::new(&[1]);
    let root = Root::of(&ca);
    let (files, path) = files(&ca);
    let state = CaState::default();
    let t = now();

    // An empty file: no root CRL yet, which is no problem.
    assert!(state.reload(&files, t), "the authority loads");
    assert_eq!((adopted(&state), state.root_crl_problem()), (None, None));

    // Signed by the loaded root: adopted, with the authority it was checked against.
    let first = root.crl(1, t - DAY, t + 400 * DAY, Some(&issuing_serial(&ca)), false);
    replace(&path, &first);
    assert!(
        !state.reload(&files, t),
        "the authority itself did not change"
    );
    let (authority, crl) = state.authority_and_root_crl();
    let crl = crl.expect("adopted");
    assert!(crl.checked_against(authority.expect("loaded").root_der()));
    assert_eq!(
        (crl.number(), crl.next_update(), crl.this_update()),
        ("1", t + 400 * DAY, t - DAY)
    );
    // What gateways get: the same CRL, as one PEM block.
    assert_eq!(
        certs::pem::decode_all(crl.pem(), certs::pem::CRL).expect("PEM"),
        vec![crl.der().to_vec()]
    );
    assert_eq!(
        certs::pem::decode_all(&first, certs::pem::CRL).expect("PEM"),
        vec![crl.der().to_vec()]
    );
    let adoption = capture.events("adopted the root CRL");
    assert_eq!(adoption.len(), 1, "{adoption:?}");
    assert_eq!(adoption[0].0, tracing::Level::INFO);
    assert_eq!(adoption[0].1["number"], "1");

    // Files it must not adopt: the ERROR names the problem, and the root CRL adopted before
    // (checked against the same root) stays.
    let other = Root::new("TrustIoT other test root");
    let impostor = Root::new("TrustIoT test root");
    let cases = [
        (
            "signed by another root",
            other.crl(2, t - DAY, t + 400 * DAY, None, false),
            "is not a trust anchor",
        ),
        (
            "signed by a key with the root's name",
            impostor.crl(2, t - DAY, t + 400 * DAY, None, false),
            "does not verify",
        ),
        (
            "with an issuing distribution point",
            root.crl(2, t - DAY, t + 400 * DAY, None, true),
            "issuing distribution point",
        ),
        ("two CRLs", format!("{first}{first}"), "holds 2 PEM blocks"),
        ("a certificate", ca.root_pem.clone(), "holds 0 PEM blocks"),
        (
            "too large",
            "A".repeat(usize::try_from(root_crl::MAX_BYTES).expect("fits") + 1),
            "larger than",
        ),
    ];
    for (what, contents, reason) in cases {
        replace(&path, &contents);
        state.reload(&files, t);
        let problem = state.root_crl_problem().expect(what);
        assert!(problem.contains(reason), "{what}: {problem}");
        assert_eq!(adopted(&state).as_deref(), Some("1"), "{what}: kept");
        let refusal = capture
            .events("cannot adopt the root CRL")
            .pop()
            .expect(what);
        assert_eq!(refusal.0, tracing::Level::ERROR, "{what}");
        assert_eq!(refusal.1["problem"], problem, "{what}");
        assert_eq!(refusal.1["kept_number"], "1", "{what}");
        println!("{what}: refused, kept number 1: {problem}");
    }
    // The monitor repeats the ERROR at every check while the file stays unusable.
    let before = capture.events("cannot adopt the root CRL").len();
    state.log_root_crl(t);
    assert_eq!(
        capture.events("cannot adopt the root CRL").len(),
        before + 1
    );

    // Back to a good file: adopted, and the problem clears.
    replace(&path, &root.crl(3, t - DAY, t + 400 * DAY, None, false));
    state.reload(&files, t);
    assert_eq!(
        (adopted(&state).as_deref(), state.root_crl_problem()),
        (Some("3"), None)
    );

    // A new root (a new root certificate in the values) with the old root's CRL: not adopted,
    // and nothing is kept, since the kept CRL was checked against the old root.
    let new_ca = TestCa::new(&[1]);
    std::fs::copy(&new_ca.root_file, &ca.root_file).expect("the new root");
    std::fs::copy(&new_ca.certificates_file, &ca.certificates_file).expect("its issuing CA");
    std::fs::copy(&new_ca.keys_file, &ca.keys_file).expect("its keys");
    assert!(state.reload(&files, t), "the new authority loads");
    assert_eq!(adopted(&state), None);
    let problem = state.root_crl_problem().expect("the old root's CRL");
    assert!(problem.contains("does not verify"), "{problem}");
    // Its own root's CRL is adopted.
    replace(
        &path,
        &Root::of(&new_ca).crl(1, t - DAY, t + 400 * DAY, None, false),
    );
    state.reload(&files, t);
    assert_eq!(adopted(&state).as_deref(), Some("1"));

    // Emptied again: no root CRL, no problem.
    replace(&path, "\n");
    state.reload(&files, t);
    assert_eq!((adopted(&state), state.root_crl_problem()), (None, None));
}

#[test]
fn a_new_root_crl_file_is_adopted_without_a_restart() {
    let ca = TestCa::new(&[1]);
    let root = Root::of(&ca);
    let (files, path) = files(&ca);
    let state = CaState::default();
    let t = now();
    replace(&path, &root.crl(1, t - DAY, t + 400 * DAY, None, false));
    state.reload(&files, t);
    assert_eq!(adopted(&state).as_deref(), Some("1"));
    // The same file read again changes nothing.
    assert!(!state.reload(&files, t));
    assert_eq!(adopted(&state).as_deref(), Some("1"));
    // The next ceremony's root CRL replaces it at the next reload of the same state, which
    // `CaState::watch` runs every `ca.reload_interval`.
    replace(
        &path,
        &root.crl(2, t, t + 400 * DAY, Some(&issuing_serial(&ca)), false),
    );
    state.reload(&files, t);
    assert_eq!(adopted(&state).as_deref(), Some("2"));
}

#[test]
fn the_alert_holds_below_60_days_and_after_expiry_while_the_crl_stays_adopted() {
    let (capture, _guard) = Capture::install();
    let ca = TestCa::new(&[1]);
    let (files, path) = files(&ca);
    let state = CaState::default();
    let t = now();
    let next_update = t + 100 * DAY;
    replace(
        &path,
        &Root::of(&ca).crl(1, t - DAY, next_update, None, false),
    );
    state.reload(&files, t);
    let crl: Arc<RootCrl> = state.root_crl().expect("adopted");
    assert_eq!(ALERT_BEFORE_SECS, 60 * DAY);

    // (clock, expiring, expired): a fixed clock before and after the 60-day mark and past the
    // nextUpdate.
    for (clock, expiring, expired) in [
        (next_update - 61 * DAY, false, false),
        (next_update - ALERT_BEFORE_SECS, false, false),
        (next_update - ALERT_BEFORE_SECS + 1, true, false),
        (next_update - 59 * DAY, true, false),
        (next_update, true, true),
        (next_update + 30 * DAY, true, true),
    ] {
        assert_eq!(crl.expiring(clock), expiring, "at {clock}");
        assert_eq!(crl.expired(clock), expired, "at {clock}");
        assert_eq!(crl.remaining(clock), next_update - clock);
        let before = capture.events("the root CRL expires").len();
        state.log_root_crl(clock);
        let warnings = capture.events("the root CRL expires");
        if expiring {
            assert_eq!(warnings.len(), before + 1, "at {clock}");
            let (level, fields) = warnings.last().expect("the warning");
            assert_eq!(*level, tracing::Level::WARN);
            assert_eq!(fields["number"], "1");
            assert_eq!(fields["expired"], expired.to_string());
            assert_eq!(
                fields["remaining_seconds"],
                (next_update - clock).to_string()
            );
            println!(
                "{} days before nextUpdate: WARN, expired {expired}",
                (next_update - clock) / DAY
            );
        } else {
            assert_eq!(warnings.len(), before, "no warning at {clock}");
        }
    }
    // An expired root CRL file is adopted all the same: gateways keep using it.
    replace(
        &path,
        &Root::of(&ca).crl(2, t - 500 * DAY, t - DAY, None, false),
    );
    state.reload(&files, t);
    let expired = state.root_crl().expect("adopted although expired");
    assert_eq!(
        (expired.number(), expired.expired(t), expired.expiring(t)),
        ("2", true, true)
    );
    assert_eq!(state.root_crl_problem(), None);
    // Nothing to alert about without a root CRL.
    replace(&path, "");
    state.reload(&files, t);
    let before = capture.events("the root CRL expires").len()
        + capture.events("cannot adopt the root CRL").len();
    state.log_root_crl(t);
    assert_eq!(
        capture.events("the root CRL expires").len()
            + capture.events("cannot adopt the root CRL").len(),
        before
    );
}

/// The service on the product's schema: adopts the root CRL at start, alerts on it, adopts the
/// next one written to the file without a restart, refuses one the root did not sign (keeping
/// the one before), and adopts an expired one, alerting.
#[tokio::test(flavor = "multi_thread")]
async fn the_service_adopts_alerts_and_reloads_the_root_crl() {
    let Some(product) = product_postgres(&["certs_app"]).await else {
        return;
    };
    let ca = TestCa::new(&[1]);
    let root = Root::of(&ca);
    let path = temp_dir("certs-root-crl").join("root-crl.pem");
    let t = now();
    // 30 days left: adopted and alerting from the start.
    std::fs::write(&path, root.crl(1, t - DAY, t + 30 * DAY, None, false)).expect("write");
    let collector = FakeCollector::start();
    let endpoint = collector.url();
    let file = path.display().to_string();
    let service = Service::start(
        &product,
        &ca,
        &[
            ("TRUSTIOT__CA__ROOT_CRL_FILE", file.as_str()),
            ("TRUSTIOT__CA__RELOAD_INTERVAL", "1s"),
            ("TRUSTIOT__CRL__CHECK_INTERVAL", "1s"),
            ("TRUSTIOT__TELEMETRY__OTLP_ENDPOINT", endpoint.as_str()),
            ("OTEL_METRIC_EXPORT_INTERVAL", "300"),
        ],
    )
    .await;
    let adoption = |number: &'static str| {
        let service = &service;
        move || async move {
            service
                .stdout
                .messages("adopted the root CRL")
                .into_iter()
                .find(|line| line["fields"]["number"] == number)
        }
    };
    let line = wait_for("the root CRL 1 adopted at start", adoption("1")).await;
    println!("start: {line}");
    let gauge = |metric: &'static str, wanted: fn(f64) -> bool, after: usize| {
        let collector = &collector;
        move || async move {
            points(collector)
                .into_iter()
                .enumerate()
                .skip(after)
                .find(|(_, point)| point.metric == metric && wanted(point.value))
        }
    };
    let (index, _) = wait_for(
        "certs.root_crl.expiring = 1 with 30 days left",
        gauge("certs.root_crl.expiring", |value| value == 1.0, 0),
    )
    .await;
    let (_, remaining) = wait_for(
        "certs.root_crl.remaining below 60 days",
        gauge(
            "certs.root_crl.remaining",
            |value| value > 0.0 && value < 30.0 * 86_400.0 + 1.0,
            0,
        ),
    )
    .await;
    println!(
        "30 days left: certs.root_crl.expiring = 1, remaining {} s",
        remaining.value
    );
    let warning = wait_for("the WARN line of the root CRL", || async {
        service
            .stdout
            .messages("the root CRL expires in less than 60 days")
            .into_iter()
            .find(|line| line["level"] == "WARN" && line["fields"]["number"] == "1")
    })
    .await;
    println!("alert line: {warning}");

    // The next ceremony's root CRL: adopted without a restart, and the alert clears.
    replace(
        &path,
        &root.crl(2, t, t + 400 * DAY, Some(&issuing_serial(&ca)), false),
    );
    wait_for(
        "the root CRL 2 adopted after the file changed",
        adoption("2"),
    )
    .await;
    let exports = points(&collector).len().max(index + 1);
    wait_for(
        "certs.root_crl.expiring = 0 after the new root CRL",
        gauge("certs.root_crl.expiring", |value| value == 0.0, exports),
    )
    .await;
    println!("root CRL 2 adopted without a restart; certs.root_crl.expiring = 0");

    // One the root did not sign: refused with an ERROR, the one before kept, the gauge set.
    let other = Root::new("TrustIoT other test root");
    replace(&path, &other.crl(3, t, t + 400 * DAY, None, false));
    let refusal = wait_for("the ERROR line for a root CRL of another root", || async {
        service
            .stdout
            .messages("cannot adopt the root CRL")
            .into_iter()
            .find(|line| line["level"] == "ERROR")
    })
    .await;
    assert_eq!(refusal["fields"]["kept_number"], "2", "{refusal}");
    println!("refused: {refusal}");
    let exports = points(&collector).len();
    wait_for(
        "certs.root_crl.invalid = 1",
        gauge("certs.root_crl.invalid", |value| value == 1.0, exports),
    )
    .await;

    // An expired root CRL of the root: adopted all the same, alerting, with a negative
    // remaining time; the refusal clears.
    replace(&path, &root.crl(4, t - 500 * DAY, t - DAY, None, false));
    wait_for("the expired root CRL 4 adopted", adoption("4")).await;
    let exports = points(&collector).len();
    wait_for(
        "certs.root_crl.remaining < 0 for the expired root CRL",
        gauge("certs.root_crl.remaining", |value| value < 0.0, exports),
    )
    .await;
    wait_for(
        "certs.root_crl.expiring = 1 for the expired root CRL",
        gauge("certs.root_crl.expiring", |value| value == 1.0, exports),
    )
    .await;
    wait_for(
        "certs.root_crl.invalid = 0 again",
        gauge("certs.root_crl.invalid", |value| value == 0.0, exports),
    )
    .await;
    let expired = wait_for("the WARN line of the expired root CRL", || async {
        service
            .stdout
            .messages("the root CRL expires in less than 60 days")
            .into_iter()
            .find(|line| line["fields"]["number"] == "4" && line["fields"]["expired"] == true)
    })
    .await;
    println!("expired root CRL still adopted: {expired}");
    drop(service);
}

/// Each captured event's level, and its fields with the message under `message`.
type Captured = Vec<(tracing::Level, BTreeMap<String, String>)>;

/// Captures the events of the current thread.
#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Captured>>);

impl Capture {
    /// Captures until the guard is dropped; a second dispatcher kept alive for the whole binary
    /// keeps tracing from caching a callsite as unwanted (tests/bindings.rs).
    fn install() -> (Self, tracing::subscriber::DefaultGuard) {
        use tracing_subscriber::layer::SubscriberExt as _;
        static KEEPER: OnceLock<tracing::Dispatch> = OnceLock::new();
        KEEPER.get_or_init(|| tracing::Dispatch::new(tracing::subscriber::NoSubscriber::new()));
        let capture = Self::default();
        let guard =
            tracing::subscriber::set_default(tracing_subscriber::registry().with(capture.clone()));
        (capture, guard)
    }

    /// The level and fields of every event whose message starts with `message`.
    fn events(&self, message: &str) -> Vec<(tracing::Level, BTreeMap<String, String>)> {
        self.0
            .lock()
            .expect("the lock")
            .iter()
            .filter(|(_, fields)| {
                fields
                    .get("message")
                    .is_some_and(|text| text.starts_with(message))
            })
            .cloned()
            .collect()
    }
}

struct Visitor<'a>(&'a mut BTreeMap<String, String>);

impl tracing::field::Visit for Visitor<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0.insert(field.name().to_owned(), format!("{value:?}"));
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.0.insert(field.name().to_owned(), value.to_owned());
    }
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Capture {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _context: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut fields = BTreeMap::new();
        event.record(&mut Visitor(&mut fields));
        let level = *event.metadata().level();
        self.0.lock().expect("the lock").push((level, fields));
    }
}
