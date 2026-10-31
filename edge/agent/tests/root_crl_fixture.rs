//! The agent accepts the root CRL the offline ceremony signs (Laysi/trustiot-v2#829): the files
//! in `tests/fixtures/root-crl/` were made by `certs-ca root create`, `root sign` and two
//! `root crl` runs (the second revokes the issuing CA), as that directory's README says. They are
//! public test data; the keys that made them were thrown away. `RootCrl::load` checks the format
//! rules of services/certs/README.md 「根 CRL」 (one PEM block, no issuing distribution point,
//! signed by a trust anchor with an algorithm the handshake accepts, parsed by webpki), so this
//! is the cross-check of the agent against the tool;
//! `services/certs/tests/ceremony_root_crl.rs` checks the fields and the handshake verdict on
//! the tool's side.

use std::path::{Path, PathBuf};
use std::time::Duration;

use agent::tls::{BrokerTrust, RootCrl};
use rustls_pki_types::UnixTime;
use rustls_pki_types::pem::PemObject as _;
use x509_parser::prelude::{FromDer as _, X509Certificate};
use x509_parser::revocation_list::CertificateRevocationList;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/root-crl")
        .join(name)
}

fn at(seconds: i64) -> UnixTime {
    UnixTime::since_unix_epoch(Duration::from_secs(
        u64::try_from(seconds).expect("after 1970"),
    ))
}

#[test]
fn the_agent_loads_a_root_crl_that_certs_ca_signed() {
    let trust = BrokerTrust::load(&fixture("root.pem")).expect("the fixture root as broker.ca");
    let path = fixture("root-crl.pem");
    let issuing =
        rustls_pki_types::CertificateDer::from_pem_file(fixture("issuing-ca-generation-1.pem"))
            .expect("the revoked issuing CA");
    let (_, issuing) = X509Certificate::from_der(&issuing).expect("a certificate");

    // While the fixture was current: a day after its thisUpdate.
    let bytes = std::fs::read(&path).expect("the fixture root CRL");
    let der =
        rustls_pki_types::CertificateRevocationListDer::from_pem_slice(&bytes).expect("a PEM CRL");
    let (_, crl) = CertificateRevocationList::from_der(&der).expect("a CRL");
    let this_update = crl.last_update().timestamp();
    let loaded = RootCrl::load(Some(&path), &trust, at(this_update + 86_400))
        .expect("the agent accepts the root CRL certs-ca signed");
    let RootCrl::Loaded {
        number,
        expired,
        der: loaded_der,
        ..
    } = &loaded
    else {
        panic!("not loaded: {loaded:?}");
    };
    assert_eq!(number.as_deref(), Some("2"), "the second root CRL");
    assert!(!expired);
    assert_eq!(loaded_der.as_ref(), der.as_ref());
    let serials: Vec<&[u8]> = crl
        .iter_revoked_certificates()
        .map(|entry| entry.raw_serial())
        .collect();
    assert_eq!(serials, [issuing.raw_serial()], "it revokes generation 1");

    // After its nextUpdate (400 days after thisUpdate): the agent still loads an expired root
    // CRL on purpose (README 「根 CRL」, 「過期」: a late ceremony must not cut off the fleet)
    // and marks it expired, so this test needs no new fixture once that day passes.
    let next_update = crl
        .next_update()
        .expect("certs-ca writes nextUpdate")
        .timestamp();
    assert_eq!(next_update - this_update, 400 * 86_400);
    let late = RootCrl::load(Some(&path), &trust, at(next_update + 1))
        .expect("an expired root CRL still loads");
    let RootCrl::Loaded {
        number: late_number,
        expired: late_expired,
        ..
    } = &late
    else {
        panic!("not loaded after nextUpdate: {late:?}");
    };
    assert!(*late_expired, "loaded after nextUpdate, it is expired");
    assert_eq!(late_number.as_deref(), Some("2"));
    println!(
        "root CRL fixture: number {number:?}, loaded at thisUpdate + 1 day (current) and at \
         nextUpdate + 1 s (expired)"
    );
}
