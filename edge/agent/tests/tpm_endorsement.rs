//! `agent::tpm::endorsement_key` against a real TPM 2.0 implementation: swtpm (libtpms) in a
//! throwaway container (`testkit::swtpm`), set up and inspected with tpm2-tools
//! (Laysi/trustiot-v2#612).
//!
//! - the EK is the one `tpm2_createek -G ecc` makes, and every run, on the same connection or a
//!   new one, gets the same; nothing is persisted or left loaded;
//! - an EK certificate made by rcgen and written to `0x01C0000A` with `tpm2_nvdefine` and
//!   `tpm2_nvwrite` is returned when it is for the EK (read in chunks, through the owner or the
//!   index's own authorization), refused when it is for another key or is not a certificate, and
//!   reported absent when there is none or the index was never written;
//! - an authorization on the endorsement hierarchy ends with a clear error and leaves the TPM
//!   exactly as it was;
//! - an EK template or nonce in NV is refused before anything is created;
//! - a process that dies while the EK is loaded leaves nothing loaded.
//!
//! Each [`Process`] is one `testkit::swtpm::Session`, which flushes what it loaded when it ends,
//! as the kernel's resource manager does. Without podman or docker the tests skip, unless `CI` is
//! set, where they fail.

#![allow(
    clippy::unwrap_used,
    reason = "test helpers unwrap like the tests themselves (clippy.toml allows it in tests)"
)]

use std::io;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent::tpm::{
    EK_CERTIFICATE_INDEX, EkCertificate, Endorsement, EndorsementError, NV_READ_CHUNK, TpmChannel,
    TpmError, TpmTransport, endorsement_key,
};
use base64::Engine as _;
use testkit::swtpm::{Session, Swtpm};

/// How long a test waits for the TPM thread to close its session: milliseconds unless the code
/// is wrong.
const WAIT: Duration = Duration::from_secs(60);

/// TPM command codes the tests look for.
const CREATE_PRIMARY: u32 = 0x131;
const NV_READ: u32 = 0x14e;
const FLUSH_CONTEXT: u32 = 0x165;
const NV_READ_PUBLIC: u32 = 0x169;
const READ_PUBLIC: u32 = 0x173;

/// The commands of a run on a TPM without an EK certificate.
const WITHOUT_CERTIFICATE: [u32; 6] = [
    NV_READ_PUBLIC,
    NV_READ_PUBLIC,
    CREATE_PRIMARY,
    READ_PUBLIC,
    FLUSH_CONTEXT,
    NV_READ_PUBLIC,
];

/// How an NV index is defined: the hierarchy (tpm2-tools' `-C`) and the attributes.
#[derive(Clone, Copy, Debug)]
struct Index {
    hierarchy: &'static str,
    attributes: &'static str,
}

/// The TCG EK Credential Profile's EK certificate index, defined by the platform as a
/// manufacturer does.
const PLATFORM_INDEX: Index = Index {
    hierarchy: "p",
    attributes: "ppwrite|writedefine|ppread|ownerread|authread|no_da|platformcreate",
};
/// An index the owner defined that only its own authorization can read.
const AUTHREAD_INDEX: Index = Index {
    hierarchy: "o",
    attributes: "ownerwrite|authread|no_da",
};

/// Defines the NV index `index` as `PLATFORM_INDEX` with `size` bytes, without writing it.
fn define_platform_index(swtpm: &Swtpm, index: u32, size: usize) {
    let _ = swtpm.tools(&format!(
        "tpm2_nvdefine {index:#010x} -C p -a '{}' -s {size}",
        PLATFORM_INDEX.attributes
    ));
}

/// One process using the TPM through its own session.
struct Process {
    session: Session,
    dies_at: Option<u32>,
    dead: bool,
    commands: Arc<Mutex<Vec<Vec<u8>>>>,
    /// Dropped with the channel, when the TPM thread has let go of it.
    _closed: mpsc::Sender<()>,
}

impl TpmChannel for Process {
    fn transact(&mut self, command: &[u8]) -> io::Result<Vec<u8>> {
        self.commands.lock().unwrap().push(command.to_vec());
        if self.dies_at == Some(code(command)) {
            self.dead = true;
        }
        if self.dead {
            return Err(io::Error::other("the process has died"));
        }
        self.session.transact(command)
    }
}

fn code(command: &[u8]) -> u32 {
    u32::from_be_bytes(command[6..10].try_into().unwrap())
}

/// What a test can see of a process after it ran.
struct Watch {
    commands: Arc<Mutex<Vec<Vec<u8>>>>,
    closed: mpsc::Receiver<()>,
}

impl Watch {
    fn commands(&self) -> Vec<Vec<u8>> {
        self.commands.lock().unwrap().clone()
    }

    fn codes(&self) -> Vec<u32> {
        self.commands()
            .iter()
            .map(|command| code(command))
            .collect()
    }

    /// Waits until the session is closed (its transient objects flushed).
    fn closed(&self) {
        match self.closed.recv_timeout(WAIT) {
            Err(RecvTimeoutError::Disconnected) => {}
            other => panic!("the TPM thread did not close its session within {WAIT:?}: {other:?}"),
        }
    }
}

/// Runs `endorsement_key` as one process that ends afterwards.
fn run_until(
    swtpm: &Swtpm,
    dies_at: Option<u32>,
) -> (Result<Endorsement, EndorsementError>, Watch) {
    let commands = Arc::new(Mutex::new(Vec::new()));
    let (closed_sender, closed) = mpsc::channel();
    let process = Process {
        session: swtpm.session(),
        dies_at,
        dead: false,
        commands: Arc::clone(&commands),
        _closed: closed_sender,
    };
    let result = endorsement_key(TpmTransport::Channel(Box::new(process)));
    let watch = Watch { commands, closed };
    watch.closed();
    (result, watch)
}

fn run(swtpm: &Swtpm) -> (Result<Endorsement, EndorsementError>, Watch) {
    run_until(swtpm, None)
}

fn hex(text: &str) -> Vec<u8> {
    text.trim()
        .as_bytes()
        .chunks(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}

/// The SubjectPublicKeyInfo of `tpm2_createek -G ecc`'s EK, as tpm2-tools computes it.
fn tools_ek(swtpm: &Swtpm) -> Vec<u8> {
    hex(&swtpm.tools(
        "tpm2_createek -Q -G ecc -c ek.ctx\n\
         tpm2_readpublic -Q -c ek.ctx -f der -o ek.der\n\
         tpm2_flushcontext -t\n\
         od -An -v -tx1 ek.der | tr -d ' \\n'",
    ))
}

/// What the TPM holds, with every NV index's public area.
fn state(swtpm: &Swtpm) -> String {
    let mut state = swtpm.snapshot();
    state.push_str(&swtpm.tools("echo 'nv public:'; tpm2_nvreadpublic"));
    state
}

/// A certificate made by rcgen for the P-256 key `spki`, signed by a throwaway CA, with enough
/// names to need two TPM2_NV_Read chunks.
fn certificate_for(spki: &[u8]) -> Vec<u8> {
    struct Public(Vec<u8>);
    impl rcgen::PublicKeyData for Public {
        fn der_bytes(&self) -> &[u8] {
            &self.0
        }
        fn algorithm(&self) -> &'static rcgen::SignatureAlgorithm {
            &rcgen::PKCS_ECDSA_P256_SHA256
        }
    }
    let issuer_key = rcgen::KeyPair::generate().unwrap();
    let issuer_params = rcgen::CertificateParams::new(vec!["ek-ca.test".to_owned()]).unwrap();
    let issuer = rcgen::Issuer::new(issuer_params, issuer_key);
    let names: Vec<String> = (0..12)
        .map(|n| format!("endorsement-key-{n}.manufacturer.test"))
        .collect();
    let params = rcgen::CertificateParams::new(names).unwrap();
    // The point after the fixed 26-byte P-256 SubjectPublicKeyInfo prefix.
    let der = params
        .signed_by(&Public(spki[26..].to_vec()), &issuer)
        .unwrap()
        .der()
        .to_vec();
    let chunk = usize::from(NV_READ_CHUNK);
    assert!(
        (chunk + 1..2 * chunk).contains(&der.len()),
        "the test certificate has {} bytes",
        der.len()
    );
    der
}

/// A fresh P-256 key's SubjectPublicKeyInfo.
fn other_key() -> Vec<u8> {
    rcgen::PublicKeyData::subject_public_key_info(&rcgen::KeyPair::generate().unwrap())
}

/// Defines the EK certificate index as `index` says and writes `contents` to it, through the
/// same hierarchy.
fn put_certificate(swtpm: &Swtpm, index: Index, contents: &[u8]) {
    let Index {
        hierarchy,
        attributes,
    } = index;
    let encoded = base64::engine::general_purpose::STANDARD.encode(contents);
    let _ = swtpm.tools(&format!(
        "printf '%s' '{encoded}' | base64 -d > cert.der\n\
         tpm2_nvdefine {EK_CERTIFICATE_INDEX:#010x} -C {hierarchy} -a '{attributes}' -s {}\n\
         tpm2_nvwrite {EK_CERTIFICATE_INDEX:#010x} -C {hierarchy} -i cert.der",
        contents.len()
    ));
}

fn remove_certificate(swtpm: &Swtpm, index: Index) {
    let _ = swtpm.tools(&format!(
        "tpm2_nvundefine {EK_CERTIFICATE_INDEX:#010x} -C {}",
        index.hierarchy
    ));
}

/// The authorization handle of each TPM2_NV_Read the process sent.
fn nv_read_authorizations(watch: &Watch) -> Vec<u32> {
    watch
        .commands()
        .iter()
        .filter(|command| code(command) == NV_READ)
        .map(|command| u32::from_be_bytes(command[10..14].try_into().unwrap()))
        .collect()
}

#[test]
fn the_ek_is_tpm2_tools_ecc_ek_and_the_same_on_every_run() {
    let Some(swtpm) = Swtpm::start() else {
        return;
    };
    let before = state(&swtpm);
    let (first, watch) = run(&swtpm);
    let first = first.unwrap();
    assert_eq!(first.certificate, EkCertificate::Absent);
    assert_eq!(watch.codes(), WITHOUT_CERTIFICATE);
    // An ECC P-256 SubjectPublicKeyInfo with an uncompressed point.
    assert_eq!(first.public_key_spki_der.len(), 91);
    assert_eq!(
        first.public_key_spki_der[..27],
        hex("3059301306072a8648ce3d020106082a8648ce3d03010703420004")[..]
    );
    // Flushed by endorsement_key itself (the session has nothing left to flush), nothing kept.
    assert_eq!(state(&swtpm), before, "the TPM changed");

    assert_eq!(first.public_key_spki_der, tools_ek(&swtpm));
    assert!(swtpm.transient_handles().is_empty());

    // Again on the same connection, then on a new one.
    for new_connection in [false, true] {
        if new_connection {
            swtpm.disconnect();
        }
        let (again, watch) = run(&swtpm);
        assert_eq!(again.unwrap(), first);
        assert_eq!(watch.codes(), WITHOUT_CERTIFICATE);
    }
    assert_eq!(state(&swtpm), before, "the TPM changed");
}

#[test]
fn an_ek_certificate_is_returned_when_it_is_for_the_ek_and_refused_otherwise() {
    let Some(swtpm) = Swtpm::start() else {
        return;
    };
    // No certificate: marked absent.
    let (absent, _) = run(&swtpm);
    let absent = absent.unwrap();
    assert_eq!(absent.certificate, EkCertificate::Absent);
    let ek = absent.public_key_spki_der;

    // The EK's certificate, as a manufacturer stores it, then in an index only its own
    // authorization reads: returned as it is, read in two chunks.
    let certificate = certificate_for(&ek);
    for (definition, authorization) in [
        (PLATFORM_INDEX, 0x4000_0001_u32),
        (AUTHREAD_INDEX, EK_CERTIFICATE_INDEX),
    ] {
        put_certificate(&swtpm, definition, &certificate);
        let before = state(&swtpm);
        let (matching, watch) = run(&swtpm);
        let matching = matching.unwrap();
        assert_eq!(matching.public_key_spki_der, ek);
        assert_eq!(
            matching.certificate,
            EkCertificate::Matches(certificate.clone())
        );
        let mut expected = WITHOUT_CERTIFICATE.to_vec();
        expected.extend([NV_READ, NV_READ]);
        assert_eq!(watch.codes(), expected, "{definition:?}");
        assert_eq!(nv_read_authorizations(&watch), [authorization; 2]);
        assert_eq!(state(&swtpm), before, "the TPM changed");
        remove_certificate(&swtpm, definition);
    }

    // A certificate for another key: refused, nothing changed.
    let other = certificate_for(&other_key());
    put_certificate(&swtpm, PLATFORM_INDEX, &other);
    let before = state(&swtpm);
    let (mismatch, _) = run(&swtpm);
    match mismatch {
        Err(EndorsementError::CertificateMismatch {
            endorsement_key,
            certificate,
            reason,
        }) => {
            assert_eq!(endorsement_key, ek);
            assert_eq!(certificate, other);
            assert!(reason.contains("is not the EK's"), "{reason}");
        }
        other => panic!("expected a mismatch, got {other:?}"),
    }
    assert_eq!(state(&swtpm), before, "the TPM changed");
    remove_certificate(&swtpm, PLATFORM_INDEX);

    // Something that is not a certificate: refused.
    let mut truncated = certificate.clone();
    truncated.truncate(certificate.len() - 1);
    put_certificate(&swtpm, PLATFORM_INDEX, &truncated);
    let (invalid, _) = run(&swtpm);
    assert!(
        matches!(&invalid, Err(EndorsementError::CertificateInvalid { certificate, .. }) if *certificate == truncated),
        "{invalid:?}"
    );
    remove_certificate(&swtpm, PLATFORM_INDEX);

    // An index defined but never written holds no certificate.
    define_platform_index(&swtpm, EK_CERTIFICATE_INDEX, 600);
    let (unwritten, watch) = run(&swtpm);
    assert_eq!(unwritten.unwrap().certificate, EkCertificate::Absent);
    assert_eq!(watch.codes(), WITHOUT_CERTIFICATE);
}

#[test]
fn an_endorsement_authorization_is_refused_and_the_tpm_is_unchanged() {
    let Some(swtpm) = Swtpm::start() else {
        return;
    };
    let _ = swtpm.tools("tpm2_changeauth -c e \"$(od -An -N16 -tx1 /dev/urandom | tr -d ' \\n')\"");
    let before = state(&swtpm);
    let (result, watch) = run(&swtpm);
    match &result {
        // TPM_RC_BAD_AUTH for session 1: hierarchy authorizations are not subject to lockout.
        Err(error @ EndorsementError::EndorsementAuthorization(code)) => {
            assert_eq!(code.code(), 0x9a2);
            assert!(error.to_string().contains("authorization value"), "{error}");
        }
        other => panic!("expected the endorsement authorization error, got {other:?}"),
    }
    assert_eq!(
        watch.codes(),
        [NV_READ_PUBLIC, NV_READ_PUBLIC, CREATE_PRIMARY]
    );
    // Lockout counter included.
    assert_eq!(state(&swtpm), before, "the TPM changed");
}

#[test]
fn an_ek_template_or_nonce_in_nv_is_refused_before_anything_is_created() {
    let Some(swtpm) = Swtpm::start() else {
        return;
    };
    for (index, what, sent) in [
        (0x01c0_000c_u32, "template", &[NV_READ_PUBLIC][..]),
        (0x01c0_000b, "nonce", &[NV_READ_PUBLIC, NV_READ_PUBLIC]),
    ] {
        define_platform_index(&swtpm, index, 32);
        let (result, watch) = run(&swtpm);
        assert!(
            matches!(result, Err(EndorsementError::CustomTemplate { index: refused, what: named }) if refused == index && named == what),
            "{result:?}"
        );
        assert_eq!(watch.codes(), sent);
        let _ = swtpm.tools(&format!("tpm2_nvundefine {index:#010x} -C p"));
    }
}

#[test]
fn a_process_that_dies_while_the_ek_is_loaded_leaves_nothing() {
    let Some(swtpm) = Swtpm::start() else {
        return;
    };
    let before = state(&swtpm);
    let (result, watch) = run_until(&swtpm, Some(READ_PUBLIC));
    assert!(
        matches!(result, Err(EndorsementError::Tpm(TpmError::Io(_)))),
        "{result:?}"
    );
    // The flush endorsement_key tried failed too; the session's end flushed the EK.
    assert_eq!(
        watch.codes(),
        [
            NV_READ_PUBLIC,
            NV_READ_PUBLIC,
            CREATE_PRIMARY,
            READ_PUBLIC,
            FLUSH_CONTEXT
        ]
    );
    assert!(swtpm.transient_handles().is_empty());
    assert_eq!(state(&swtpm), before, "the TPM changed");
    // The next run works and gets the EK tpm2-tools gets.
    let (rerun, _) = run(&swtpm);
    assert_eq!(rerun.unwrap().public_key_spki_der, tools_ek(&swtpm));
}
