//! Each issuing CA's CRL (Laysi/trustiot-v2#78 acceptance 2): what a CRL holds and how a TLS
//! server uses it, then the real service on the product's schema renewing every CA's CRL on its
//! schedule, publishing them, and raising the alert (the `certs.crl.expiring` gauge over OTLP and
//! an ERROR line) when a CRL's expiry is brought near while the CRL job cannot run, and clearing
//! it once the job renews the CRL.
//!
//! The new CRL right after a revocation (Laysi/trustiot-v2#635), on the product's schema as the
//! service role: `crl::reissue` gives the CA the next number whatever its nextUpdate; a round of
//! the job renews a CRL that is not due when a certificate was revoked after its thisUpdate, and
//! stops doing so once thisUpdate has passed the revocation; and many reissues and rounds at once
//! never lower the number, skip one, or leave out a revocation committed before them. The API
//! side (the response, the failure path) is in `tests/workstation.rs`.
//!
//! The database tests start a throwaway Postgres with TLS (podman or docker); skipped without
//! either unless `CI` is set.

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use certs::config::CrlSchedule;
use certs::crl::{self, CrlError, Reissued, Revoked};
use certs::http::{CHAIN_PATH, CRL_PATH};
use certs::issue::{self, CLOCK_SKEW_SECS};
use contract::ServiceName;
use protocol::GatewayId;
use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair, PKCS_ECDSA_P256_SHA256};
use service_kit::identity::ServiceIdentity;
use service_kit::job_lock::lock_key;
use service_kit::tenant::{BackgroundTenantDb, TenantDb};
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgConnection, Row as _};
use support::{FakeCollector, Service, TestCa, now, points, verify_client, wait_for};
use testkit::product::{DATABASE, ProductPostgres, product_postgres};
use x509_parser::certificate::X509Certificate;
use x509_parser::extensions::ParsedExtension;
use x509_parser::prelude::FromDer;
use x509_parser::revocation_list::CertificateRevocationList;

/// The schedule of the service test, in seconds: a CRL is valid for 20 s, replaced once it has
/// less than 12 s left and alerts below 4 s; the job and the monitor run every second. Eight
/// seconds lie between renewal and the alert, so a loaded host may skip several rounds before a
/// healthy CRL alerts.
const CHECK: &str = "1s";
const VALIDITY_SECS: i64 = 20;
const RENEW_BEFORE_SECS: i64 = 12;
const ALERT_BEFORE_SECS: i64 = 4;
const ALERT_BEFORE_F64: f64 = 4.0;

fn gateway_request(name: &str) -> (KeyPair, Vec<u8>) {
    let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).expect("a gateway key");
    let mut params = CertificateParams::default();
    let mut subject = DistinguishedName::new();
    subject.push(DnType::CommonName, name);
    params.distinguished_name = subject;
    let request = params
        .serialize_request(&key)
        .expect("a CSR")
        .der()
        .to_vec();
    (key, request)
}

fn issue_for(ca: &certs::authority::IssuingCa, name: &str) -> issue::IssuedCertificate {
    let (_, request) = gateway_request(name);
    issue::gateway_certificate(
        ca,
        &request,
        &GatewayId::new(name).expect("a gateway id"),
        Duration::from_secs(90 * 86_400),
        now(),
    )
    .expect("signed")
}

fn subject_key_id(certificate: &X509Certificate<'_>) -> Vec<u8> {
    certificate
        .extensions()
        .iter()
        .find_map(|extension| match extension.parsed_extension() {
            ParsedExtension::SubjectKeyIdentifier(id) => Some(id.0.to_vec()),
            _ => None,
        })
        .expect("a subject key identifier")
}

/// Checks that `der` is a CRL of the issuing CA `issuing` (DER): its issuer, signature and
/// authority key identifier; returns its number and its lifetime in seconds.
fn check_crl(der: &[u8], issuing: &[u8]) -> (String, i64) {
    let (_, ca) = X509Certificate::from_der(issuing).expect("the issuing CA certificate");
    let (rest, crl) = CertificateRevocationList::from_der(der).expect("a CRL");
    assert!(rest.is_empty());
    assert_eq!(crl.issuer().as_raw(), ca.subject().as_raw());
    crl.verify_signature(ca.public_key())
        .expect("signed by the issuing CA");
    let authority_key_id = crl
        .extensions()
        .iter()
        .find_map(|extension| match extension.parsed_extension() {
            ParsedExtension::AuthorityKeyIdentifier(aki) => {
                aki.key_identifier.as_ref().map(|id| id.0.to_vec())
            }
            _ => None,
        })
        .expect("an authority key identifier");
    assert_eq!(authority_key_id, subject_key_id(&ca));
    let number = crl.crl_number().expect("a CRL number").to_string();
    let next_update = crl.next_update().expect("a nextUpdate").timestamp();
    (number, next_update - crl.last_update().timestamp())
}

/// The serial numbers a CRL lists.
fn listed(der: &[u8]) -> Vec<Vec<u8>> {
    let (_, crl) = CertificateRevocationList::from_der(der).expect("a CRL");
    crl.iter_revoked_certificates()
        .map(|revoked| revoked.raw_serial().to_vec())
        .collect()
}

#[test]
fn a_crl_revokes_what_it_lists_and_lapses_at_next_update() {
    let ca = TestCa::new(&[1]);
    let authority = ca.authority();
    let issuing = authority.current(now()).expect("an issuing CA");
    let revoked = issue_for(issuing, "gw-78-crl-1");
    let kept = issue_for(issuing, "gw-78-crl-2");
    let this_update = now() - 10;
    let der = crl::build(
        issuing,
        7,
        &[Revoked {
            serial: revoked.serial.clone(),
            revoked_at: now() - 60,
            reason: Some(1),
        }],
        this_update,
        Duration::from_secs(600),
    )
    .expect("a CRL");
    let (number, lifetime) = check_crl(&der, issuing.certificate_der());
    assert_eq!((number.as_str(), lifetime), ("7", 600));
    assert_eq!(listed(&der), std::slice::from_ref(&revoked.serial));
    let (_, parsed) = CertificateRevocationList::from_der(&der).expect("a CRL");
    let reason = parsed
        .iter_revoked_certificates()
        .next()
        .and_then(|entry| entry.reason_code())
        .expect("a reason code");
    assert_eq!(reason.1.0, 1, "keyCompromise");

    // A TLS server with this CRL refuses the revoked certificate and accepts the other, until
    // the CRL's nextUpdate has passed: then it refuses every certificate of the CA, as
    // Mosquitto does.
    let (root, chain) = (ca.root_der(), ca.issuing_ders());
    let crls = [der];
    verify_client(&root, &chain, &kept.der, &crls, now()).expect("not revoked");
    let refused = verify_client(&root, &chain, &revoked.der, &crls, now()).expect_err("revoked");
    println!("the revoked certificate: {refused}");
    let lapsed = verify_client(&root, &chain, &kept.der, &crls, this_update + 601)
        .expect_err("the CRL has lapsed");
    println!("after the CRL's nextUpdate: {lapsed}");
}

/// The CRLs of the CAs `ids` in the database: key id, number, nextUpdate (Unix seconds),
/// thisUpdate, DER. The tests of this binary share one database, each with CAs of its own.
async fn stored(
    product: &ProductPostgres,
    ids: &[String],
) -> Vec<(String, i64, i64, i64, Vec<u8>)> {
    let mut admin = product.superuser().await;
    sqlx::query(
        "SELECT ca_key_id, crl_number,
                extract(epoch FROM next_update)::bigint AS next_update,
                extract(epoch FROM this_update)::bigint AS this_update, crl
         FROM certs.crls WHERE ca_key_id = ANY($1) ORDER BY ca_key_id",
    )
    .bind(ids)
    .fetch_all(&mut admin)
    .await
    .expect("read the CRLs")
    .into_iter()
    .map(|row| {
        (
            row.get("ca_key_id"),
            row.get("crl_number"),
            row.get("next_update"),
            row.get("this_update"),
            row.get("crl"),
        )
    })
    .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn crls_are_renewed_on_schedule_and_alert_before_they_lapse() {
    let Some(product) = product_postgres(&["certs_app"]).await else {
        return;
    };
    let ca = TestCa::new(&[1, 2]);
    let authority = ca.authority();
    let ids: Vec<String> = authority
        .issuing()
        .iter()
        .map(|issuing| issuing.key_id().to_owned())
        .collect();
    let ders = ca.issuing_ders();
    let collector = FakeCollector::start();
    let endpoint = collector.url();
    let (validity, renew, alert) = (
        format!("{VALIDITY_SECS}s"),
        format!("{RENEW_BEFORE_SECS}s"),
        format!("{ALERT_BEFORE_SECS}s"),
    );
    let service = Service::start(
        &product,
        &ca,
        &[
            ("TRUSTIOT__CRL__CHECK_INTERVAL", CHECK),
            ("TRUSTIOT__CRL__VALIDITY", validity.as_str()),
            ("TRUSTIOT__CRL__RENEW_BEFORE", renew.as_str()),
            ("TRUSTIOT__CRL__ALERT_BEFORE", alert.as_str()),
            ("TRUSTIOT__TELEMETRY__OTLP_ENDPOINT", endpoint.as_str()),
            ("OTEL_METRIC_EXPORT_INTERVAL", "300"),
        ],
    )
    .await;

    // 1. The job gives each issuing CA its first CRL at once.
    let first = wait_for("a first CRL for both issuing CAs", || async {
        let rows = stored(&product, &ids).await;
        (rows.len() == 2).then_some(rows)
    })
    .await;
    for (index, id) in ids.iter().enumerate() {
        let row = first.iter().find(|row| &row.0 == id).expect("a CRL per CA");
        let (number, lifetime) = check_crl(&row.4, &ders[index]);
        assert_eq!(number, row.1.to_string());
        assert_eq!(
            lifetime,
            CLOCK_SKEW_SECS + VALIDITY_SECS,
            "thisUpdate lies the clock skew back"
        );
        println!("first CRL of CA generation {}: number {number}", index + 1);
    }

    // 2. On schedule, each is replaced by the next number once less than renew_before is left,
    //    and not before.
    let renewed = wait_for("a scheduled renewal of both CRLs", || async {
        let rows = stored(&product, &ids).await;
        let all = ids.iter().all(|id| {
            let before = first.iter().find(|row| &row.0 == id).map(|row| row.1);
            rows.iter().any(|row| &row.0 == id && Some(row.1) > before)
        });
        all.then_some(rows)
    })
    .await;
    for (index, id) in ids.iter().enumerate() {
        let before = first.iter().find(|row| &row.0 == id).expect("first");
        let after = renewed.iter().find(|row| &row.0 == id).expect("renewed");
        let (number, lifetime) = check_crl(&after.4, &ders[index]);
        assert_eq!(number, after.1.to_string());
        assert_eq!(
            lifetime,
            CLOCK_SKEW_SECS + VALIDITY_SECS,
            "thisUpdate lies the clock skew back"
        );
        let renewed_at = after.3 + CLOCK_SKEW_SECS;
        assert!(
            renewed_at >= before.2 - RENEW_BEFORE_SECS,
            "renewed at {renewed_at} although the CRL had more than {RENEW_BEFORE_SECS} s left \
             (nextUpdate {})",
            before.2
        );
        println!(
            "CA generation {}: CRL {} (thisUpdate {}) replaced by {} (thisUpdate {}) with {} s \
             left",
            index + 1,
            before.1,
            before.3,
            after.1,
            after.3,
            before.2 - renewed_at
        );
    }

    // 3. A revoked certificate of the newer CA appears in its next scheduled CRL, which the
    //    service publishes; a TLS server with the published CRLs refuses it.
    let newer = &authority.issuing()[1];
    let revoked = issue_for(newer, "gw-78-revoked");
    let kept = issue_for(newer, "gw-78-kept");
    {
        let mut app = product.postgres.connect("certs_app", DATABASE).await;
        issue::record(&mut app, &revoked).await.expect("recorded");
        issue::record(&mut app, &kept).await.expect("recorded");
        // Gateway revocation is Laysi/trustiot-v2#82; the test marks the row as the service
        // role would, with the column grant of 0002_workstation_certificates.sql.
        sqlx::query(
            "UPDATE certs.issued_certificates SET revoked_at = now(), revocation_reason = 1
             WHERE serial = $1",
        )
        .bind(&revoked.serial)
        .execute(&mut app)
        .await
        .expect("mark the certificate revoked");
    }
    wait_for("the revoked serial in the newer CA's CRL", || async {
        stored(&product, &ids)
            .await
            .into_iter()
            .find(|row| row.0 == ids[1])
            .filter(|row| listed(&row.4).contains(&revoked.serial))
    })
    .await;
    let bundle = wait_for("the published CRLs listing the revoked serial", || async {
        let (status, body) = service.get(CRL_PATH).await;
        assert_eq!(status, 200, "{body}");
        let crls = certs::pem::decode_all(&body, certs::pem::CRL).expect("PEM CRLs");
        assert_eq!(crls.len(), 2, "one CRL per issuing CA");
        crls.iter()
            .any(|crl| listed(crl).contains(&revoked.serial))
            .then_some(crls)
    })
    .await;
    for (index, der) in ders.iter().enumerate() {
        let (_, ca_certificate) = X509Certificate::from_der(der).expect("a CA certificate");
        let crl = bundle
            .iter()
            .find(|crl| {
                CertificateRevocationList::from_der(crl).is_ok_and(|(_, parsed)| {
                    parsed.issuer().as_raw() == ca_certificate.subject().as_raw()
                })
            })
            .expect("a published CRL per CA");
        check_crl(crl, der);
        println!("published CRL of CA generation {}: ok", index + 1);
    }
    let (root, chain) = (ca.root_der(), ders.clone());
    verify_client(&root, &chain, &kept.der, &bundle, now()).expect("not revoked");
    let refused = verify_client(&root, &chain, &revoked.der, &bundle, now())
        .expect_err("revoked by the published CRL");
    println!("the revoked gateway certificate with the published CRLs: {refused}");
    let (status, chain_pem) = service.get(CHAIN_PATH).await;
    assert_eq!(status, 200);
    let published_chain =
        certs::pem::decode_all(&chain_pem, certs::pem::CERTIFICATE).expect("PEM certificates");
    let mut expected_chain = vec![root.clone()];
    expected_chain.extend(ders.iter().cloned());
    assert_eq!(published_chain, expected_chain);

    // 4. The alert. While nothing is wrong, the gauge reads 0 for both CAs.
    let older = ids[0].clone();
    wait_for("certs.crl.expiring = 0 for both CAs", || async {
        let points = points(&collector);
        ids.iter()
            .all(|id| {
                points.iter().any(|point| {
                    point.metric == "certs.crl.expiring"
                        && &point.key_id == id
                        && point.value == 0.0
                })
            })
            .then_some(())
    })
    .await;
    // The CRL job cannot run (the test holds its advisory lock, as a stuck replica would), and
    // the older CA's CRL expiry is brought near.
    let mut holder = product.superuser().await;
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(lock_key("certs", crl::JOB))
        .execute(&mut holder)
        .await
        .expect("take the CRL job's lock");
    let exports_before = points(&collector).len();
    sqlx::query(
        "UPDATE certs.crls SET next_update = now() + interval '2 seconds' WHERE ca_key_id = $1",
    )
    .bind(&older)
    .execute(&mut product.superuser().await)
    .await
    .expect("bring the CRL's expiry near");
    let alerted = wait_for("certs.crl.expiring = 1 for the older CA", || async {
        points(&collector)
            .into_iter()
            .enumerate()
            .skip(exports_before)
            .find(|(_, point)| {
                point.metric == "certs.crl.expiring" && point.key_id == older && point.value == 1.0
            })
            .map(|(index, _)| index)
    })
    .await;
    let error = wait_for("the ERROR line for the older CA", || async {
        service
            .stdout
            .messages("the CRL of an issuing CA expires soon")
            .into_iter()
            .find(|line| line["level"] == "ERROR" && line["fields"]["ca_key_id"] == older.as_str())
    })
    .await;
    println!("alert line: {error}");
    // An export after the alert reads the same or a later reading: less than alert_before left.
    let remaining = wait_for("certs.crl.remaining after the alert", || async {
        points(&collector)
            .into_iter()
            .skip(alerted + 1)
            .find(|point| point.metric == "certs.crl.remaining" && point.key_id == older)
    })
    .await;
    assert!(remaining.value < ALERT_BEFORE_F64, "{remaining:?}");

    // Once the job can run again, it renews the CRL and the alert clears.
    sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(lock_key("certs", crl::JOB))
        .execute(&mut holder)
        .await
        .expect("release the CRL job's lock");
    wait_for("certs.crl.expiring = 0 again for the older CA", || async {
        points(&collector)
            .into_iter()
            .enumerate()
            .skip(alerted + 1)
            .find(|(_, point)| {
                point.metric == "certs.crl.expiring" && point.key_id == older && point.value == 0.0
            })
    })
    .await;
    let after = stored(&product, &ids)
        .await
        .into_iter()
        .find(|row| row.0 == older)
        .expect("the older CA's CRL");
    assert!(after.2 - now() > ALERT_BEFORE_SECS, "renewed: {after:?}");
    println!(
        "alert raised at export point {alerted}, cleared after renewal to CRL number {}",
        after.1
    );
    drop(service);
}

// ---- the new CRL right after a revocation (Laysi/trustiot-v2#635) ----

/// How long a reissue may wait for a CA's locks: generous, since in the concurrency test every
/// reissue and round of the CA waits its turn on a loaded host.
const LOCK_WAIT: Duration = Duration::from_secs(120);

/// A schedule under which an hour-long CRL is not due: renewed below 30 minutes left.
fn hour_schedule() -> CrlSchedule {
    CrlSchedule {
        check_interval: Duration::from_secs(1),
        validity: Duration::from_secs(3600),
        renew_before: Duration::from_secs(1800),
        alert_before: Duration::from_secs(600),
    }
}

/// The certs service's role as its CRL job and its handlers use it.
fn certs_work(product: &ProductPostgres, connections: u32) -> BackgroundTenantDb {
    TenantDb::connect_lazy_with_background(
        product.postgres.options("certs_app", DATABASE),
        PgPoolOptions::new().max_connections(connections),
        ServiceIdentity::new(ServiceName::Certs),
    )
    .expect("the certs service role's pool")
    .1
}

/// Revokes `serial` as the service role does (the column grant of
/// 0002_workstation_certificates.sql, `revoked_at` from the clock at the update).
async fn revoke(app: &mut PgConnection, serial: &[u8]) {
    let updated = sqlx::query(
        "UPDATE certs.issued_certificates
         SET revoked_at = clock_timestamp(), revocation_reason = 1
         WHERE serial = $1 AND revoked_at IS NULL",
    )
    .bind(serial)
    .execute(app)
    .await
    .expect("revoke the certificate")
    .rows_affected();
    assert_eq!(updated, 1, "one certificate revoked");
}

/// Records `certificate`, then revokes it.
async fn record_and_revoke(app: &mut PgConnection, certificate: &issue::IssuedCertificate) {
    issue::record(app, certificate).await.expect("recorded");
    revoke(app, &certificate.serial).await;
}

/// The CRL of the CA `ca_key_id`: number, thisUpdate (Unix seconds), DER.
async fn only_crl(product: &ProductPostgres, ca_key_id: &str) -> (i64, i64, Vec<u8>) {
    let rows = stored(product, &[ca_key_id.to_owned()]).await;
    assert_eq!(rows.len(), 1, "a CRL of {ca_key_id}");
    let (_, number, _, this_update, der) = rows.into_iter().next().expect("a CRL");
    (number, this_update, der)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_revocation_gets_a_new_crl_at_once_and_the_job_catches_up_on_one_it_missed() {
    let Some(product) = product_postgres(&["certs_app"]).await else {
        return;
    };
    let ca = TestCa::new(&[1]);
    let authority = ca.authority();
    let issuing = Arc::clone(authority.current(now()).expect("an issuing CA"));
    let issuing_der = ca.issuing_ders().remove(0);
    let db = certs_work(&product, 2);
    let schedule = hour_schedule();
    let start = now();

    // The first CRL, then nothing is due.
    let renewed = crl::refresh(&db, &authority, &schedule, start)
        .await
        .expect("a round");
    assert_eq!(renewed, [issuing.key_id().to_owned()]);
    assert!(
        crl::refresh(&db, &authority, &schedule, start)
            .await
            .expect("a round")
            .is_empty(),
        "an hour-long CRL with no revocation is not renewed"
    );
    assert_eq!(only_crl(&product, issuing.key_id()).await.0, 1);

    // A revocation, then the new CRL at once: the next number, listing it, with the schedule's
    // thisUpdate and lifetime, although the current CRL is far from renew_before.
    let mut app = product.postgres.connect("certs_app", DATABASE).await;
    let first = issue_for(&issuing, "gw-635-first");
    record_and_revoke(&mut app, &first).await;
    let at = now();
    let reissued = crl::reissue(&db, &authority, issuing.key_id(), &schedule, at, LOCK_WAIT)
        .await
        .expect("a new CRL");
    assert_eq!(
        reissued,
        Reissued::Issued {
            crl_number: 2,
            revoked: 1
        }
    );
    let (number, this_update, der) = only_crl(&product, issuing.key_id()).await;
    assert_eq!(number, 2);
    assert_eq!(this_update, at - CLOCK_SKEW_SECS);
    assert_eq!(listed(&der), std::slice::from_ref(&first.serial));
    let (number, lifetime) = check_crl(&der, &issuing_der);
    assert_eq!((number.as_str(), lifetime), ("2", CLOCK_SKEW_SECS + 3600));
    println!(
        "reissued after the revocation: CRL 2 lists {}",
        issue::hex(&first.serial)
    );

    // The job's catch-up: a second revocation whose new CRL never came (its caller failed). The
    // current CRL is not due, yet the next round renews it, because the revocation is newer
    // than its thisUpdate, and the CRL lists both.
    let second = issue_for(&issuing, "gw-635-second");
    record_and_revoke(&mut app, &second).await;
    let round = now();
    let renewed = crl::refresh(&db, &authority, &schedule, round)
        .await
        .expect("a round");
    assert_eq!(renewed, [issuing.key_id().to_owned()], "caught up");
    let (number, _, der) = only_crl(&product, issuing.key_id()).await;
    assert_eq!(number, 3);
    let mut expected = vec![first.serial.clone(), second.serial.clone()];
    expected.sort();
    assert_eq!(listed(&der), expected);
    println!("the round after a missed reissue: CRL 3 lists both revocations");

    // While a revocation is newer than the thisUpdate (the round minus the clock skew), a round
    // renews once more; once thisUpdate has passed every revocation, rounds stop renewing.
    let later = round + CLOCK_SKEW_SECS + 60;
    let renewed = crl::refresh(&db, &authority, &schedule, later)
        .await
        .expect("a round");
    assert_eq!(renewed.len(), 1, "the revocation was newer than thisUpdate");
    for offset in [1, 2] {
        assert!(
            crl::refresh(&db, &authority, &schedule, later + offset)
                .await
                .expect("a round")
                .is_empty(),
            "thisUpdate has passed every revocation: not renewed"
        );
    }
    assert_eq!(only_crl(&product, issuing.key_id()).await.0, 4);

    // A key id the authority does not have is refused, and nothing changes.
    let unknown = "0".repeat(64);
    let refused = crl::reissue(&db, &authority, &unknown, &schedule, now(), LOCK_WAIT)
        .await
        .expect_err("not an issuing CA");
    assert!(matches!(refused, CrlError::UnknownCa { .. }), "{refused}");
    assert_eq!(only_crl(&product, issuing.key_id()).await.0, 4);
}

#[tokio::test(flavor = "multi_thread")]
async fn reissues_and_rounds_at_once_never_go_back_skip_a_number_or_miss_a_revocation() {
    const REVOCATIONS: usize = 24;
    const ROUND_TASKS: usize = 4;
    const ROUNDS_EACH: usize = 6;
    let Some(product) = product_postgres(&["certs_app"]).await else {
        return;
    };
    let ca = TestCa::new(&[1]);
    let authority = Arc::new(ca.authority());
    let issuing = Arc::clone(authority.current(now()).expect("an issuing CA"));
    let db = certs_work(&product, 12);
    // Every round renews every CA: as much contention with the reissues as there can be.
    let always_due = CrlSchedule {
        renew_before: Duration::from_secs(3600),
        ..hour_schedule()
    };
    crl::refresh(&db, &authority, &always_due, now())
        .await
        .expect("the first CRL");
    let certificates: Vec<_> = (0..REVOCATIONS)
        .map(|index| issue_for(&issuing, &format!("gw-635-race-{index}")))
        .collect();
    {
        let mut app = product.postgres.connect("certs_app", DATABASE).await;
        for certificate in &certificates {
            issue::record(&mut app, certificate)
                .await
                .expect("recorded");
        }
    }

    // An observer reads the stored CRL over and over while the work runs.
    let stop = Arc::new(AtomicBool::new(false));
    let observer = {
        let stop = Arc::clone(&stop);
        let key_id = issuing.key_id().to_owned();
        let mut admin = product.superuser().await;
        tokio::spawn(async move {
            let mut seen: Vec<(i64, Vec<u8>)> = Vec::new();
            while !stop.load(Ordering::SeqCst) {
                let row =
                    sqlx::query("SELECT crl_number, crl FROM certs.crls WHERE ca_key_id = $1")
                        .bind(&key_id)
                        .fetch_one(&mut admin)
                        .await
                        .expect("read the CRL");
                seen.push((row.get("crl_number"), row.get("crl")));
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            seen
        })
    };
    // Each revocation commits on a connection of its own, then reissues; rounds run alongside.
    let mut revokers = Vec::new();
    for certificate in certificates.clone() {
        let (db, authority, schedule) = (db.clone(), Arc::clone(&authority), always_due.clone());
        let mut app = product.postgres.connect("certs_app", DATABASE).await;
        let key_id = issuing.key_id().to_owned();
        revokers.push(tokio::spawn(async move {
            revoke(&mut app, &certificate.serial).await;
            let reissued = crl::reissue(&db, &authority, &key_id, &schedule, now(), LOCK_WAIT)
                .await
                .expect("a new CRL");
            let Reissued::Issued { crl_number, .. } = reissued else {
                panic!("the CA has not expired: {reissued:?}");
            };
            (certificate.serial, crl_number)
        }));
    }
    let mut rounds = Vec::new();
    for _ in 0..ROUND_TASKS {
        let (db, authority, schedule) = (db.clone(), Arc::clone(&authority), always_due.clone());
        rounds.push(tokio::spawn(async move {
            let mut renewed = 0;
            for _ in 0..ROUNDS_EACH {
                renewed += crl::refresh(&db, &authority, &schedule, now())
                    .await
                    .expect("a round")
                    .len();
            }
            renewed
        }));
    }
    let mut reissued = Vec::new();
    for revoker in revokers {
        reissued.push(revoker.await.expect("a revocation task"));
    }
    let mut renewed = 0;
    for round in rounds {
        renewed += round.await.expect("a round task");
    }
    stop.store(true, Ordering::SeqCst);
    let seen = observer.await.expect("the observer");

    // Every new CRL got a number of its own, one above the last: none was dropped or reused.
    let (last, _, der) = only_crl(&product, issuing.key_id()).await;
    let mut numbers: Vec<i64> = reissued.iter().map(|(_, number)| *number).collect();
    numbers.sort_unstable();
    numbers.dedup();
    assert_eq!(numbers.len(), REVOCATIONS, "each reissue its own number");
    let made = i64::try_from(REVOCATIONS + renewed).expect("a count");
    assert_eq!(last, 1 + made, "CRL 1, then one number per new CRL");
    // The number never went back, and every CRL from a reissue's number on lists the
    // certificate whose revocation that reissue followed.
    for pair in seen.windows(2) {
        assert!(
            pair[0].0 <= pair[1].0,
            "the number went back: {} then {}",
            pair[0].0,
            pair[1].0
        );
    }
    for (number, crl) in &seen {
        let listed = listed(crl);
        for (serial, after) in &reissued {
            if number >= after {
                assert!(
                    listed.contains(serial),
                    "CRL {number} does not list {} although the reissue after its revocation \
                     made CRL {after}",
                    issue::hex(serial)
                );
            }
        }
    }
    let mut expected: Vec<Vec<u8>> = certificates
        .iter()
        .map(|certificate| certificate.serial.clone())
        .collect();
    expected.sort();
    assert_eq!(
        listed(&der),
        expected,
        "the last CRL lists every revocation"
    );
    println!(
        "{REVOCATIONS} reissues and {renewed} renewals by {ROUND_TASKS} concurrent round tasks: \
         CRL numbers 2..={last} each used once; {} observed CRLs, never going back, none missing \
         a revocation whose reissue came before it",
        seen.len()
    );
}
