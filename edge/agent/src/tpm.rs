//! The gateway's identity key in its TPM (decision 12): a minimal TPM 2.0 client in Rust that
//! sends TPM2_ReadPublic and TPM2_Sign to the kernel's resource manager (`/dev/tpmrm0`), and a
//! rustls [`SigningKey`] on top, so the inner mTLS handshake signs inside the TPM.
//!
//! There is no TSS library (tss-esapi, libtss2): nothing in C to cross-build or ship, and the
//! two commands need only a few structures. [`TpmKey::open`] reads the key's public area and
//! refuses a key that is not an ECC NIST P-256 signing key with scheme ECDSA-SHA256 and the
//! attributes of decision 12 (`fixedTPM`, `fixedParent`, `sensitiveDataOrigin`, `userWithAuth`,
//! `sign`; not `restricted` or `decrypt`), then signs one test message and verifies it with the
//! public key, so a key that cannot sign stops the agent at start rather than at every handshake.
//!
//! One thread owns the device: rustls' synchronous [`Signer::sign`] queues the command and waits
//! for the answer, about 70 ms on the gateway's TPM (docs/feasibility/2026-10-12-tpm-poc.md).
//! The key is used with an empty password through the password session (`TPM_RS_PW`); the
//! digest is computed in software and signed with the null hash-check ticket.
//!
//! [`ensure_key`] provisions that key for the factory tool (Laysi/trustiot-v2#80): it reuses the
//! key at the handle, creates one where the handle is empty, and refuses anything else there
//! without touching it (`provision`).

#[cfg(test)]
mod fixtures;
mod marshal;
mod provision;
mod rc;
mod worker;

use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use ring::signature::{ECDSA_P256_SHA256_ASN1, UnparsedPublicKey};
use rustls::SignatureAlgorithm;
use rustls::SignatureScheme;
use rustls::pki_types::SubjectPublicKeyInfoDer;
use rustls::sign::{Signer, SigningKey};

pub use provision::{
    PROVISION_RESPONSE_TIMEOUT, ProvisionError, Provisioned, SRK_HANDLE, ensure_key,
};
pub use rc::ResponseCode;
pub use worker::TpmChannel;

/// Where the gateway's identity key is persisted unless configured otherwise (the handle of the
/// 2026-10-12 PoC; the factory tool, Laysi/trustiot-v2#80, provisions it).
pub const DEFAULT_KEY_HANDLE: u32 = 0x8100_0100;
/// The kernel's TPM resource manager, which the agent uses rather than `/dev/tpm0` so other
/// programs (the LUKS unlock, tpm2-tools) can use the TPM too.
pub const DEFAULT_DEVICE: &str = "/dev/tpmrm0";
/// How long a TPM command may take before the caller gives up, queueing included. The kernel's
/// own command timeout is shorter for these commands, so this only guards against a stuck
/// worker. A signature blocks the connection thread, so this plus one progress tick must fit in
/// half of the unit's `WatchdogSec`, and two commands (opening the key) in its
/// `TimeoutStartSec` (`tests/systemd_unit.rs` checks both).
pub const RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);
/// The largest TPM response accepted.
pub const MAX_RESPONSE_BYTES: usize = marshal::MAX_RESPONSE_BYTES;

/// What [`TpmKey::open`] signs and verifies before the key is used. TLS 1.3 signs a 64-space
/// prefix and a context string (RFC 8446, section 4.4.3), so this can never pass for a handshake
/// signature.
const SELF_TEST_MESSAGE: &[u8] = b"TrustIoT gateway agent: TPM key self-test\0";

/// How to reach the TPM.
pub enum TpmTransport {
    /// A TPM character device; on the gateway [`DEFAULT_DEVICE`].
    Device(PathBuf),
    /// Any other [`TpmChannel`], such as a socket to a software TPM in tests.
    Channel(Box<dyn TpmChannel>),
}

impl fmt::Debug for TpmTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Device(path) => f.debug_tuple("Device").field(path).finish(),
            Self::Channel(_) => f.debug_tuple("Channel").finish_non_exhaustive(),
        }
    }
}

/// Why the TPM key cannot be used or a signature failed.
#[derive(Debug, thiserror::Error)]
pub enum TpmError {
    /// The configured handle is not a persistent object handle (`0x81xxxxxx`).
    #[error("{0:#010x} is not a persistent TPM handle (0x81000000 to 0x81ffffff)")]
    NotPersistentHandle(u32),
    /// The device could not be opened.
    #[error("cannot open the TPM device {}: {source}", path.display())]
    Open {
        /// The device path.
        path: PathBuf,
        /// What failed.
        #[source]
        source: std::io::Error,
    },
    /// Sending a command or reading its response failed.
    #[error("TPM I/O failed: {0}")]
    Io(#[source] std::io::Error),
    /// The TPM refused the command.
    #[error("the TPM refused the command: {0}")]
    Response(ResponseCode),
    /// The response does not follow the TPM 2.0 specification.
    #[error("malformed TPM response: {0}")]
    Malformed(String),
    /// The key at the handle is not one the agent may use.
    #[error("the TPM key is not usable: {0}")]
    UnsuitableKey(String),
    /// The test signature made at open did not verify with the key's public key.
    #[error("the TPM key's test signature does not verify with its public key")]
    SelfTestFailed,
    /// The worker thread could not be started.
    #[error("cannot start the TPM thread: {0}")]
    Spawn(#[source] std::io::Error),
    /// The worker thread has stopped (it panicked), so the TPM cannot be reached.
    #[error("the TPM thread is not running")]
    WorkerStopped,
    /// Too many requests are already waiting for the TPM.
    #[error("too many TPM requests are waiting")]
    Busy,
    /// The TPM did not answer in time.
    #[error("the TPM did not answer within {0:?}")]
    Timeout(Duration),
}

/// The gateway's identity key, persisted in the TPM; cheap to clone (all clones share one
/// worker thread). The thread stops, closing the device, when the last clone and the last
/// [`Signer`] made from it are dropped.
#[derive(Clone)]
pub struct TpmKey {
    inner: Arc<Inner>,
}

struct Inner {
    handle: u32,
    public: marshal::PublicKey,
    spki: Vec<u8>,
    worker: worker::Worker,
}

impl TpmKey {
    /// Opens the TPM (a device is opened here, by the caller's thread), starts the thread that
    /// owns it, reads and checks the public area of the persistent key `handle`, and makes one
    /// test signature. Blocks for that signature (about 70 ms on the gateway).
    ///
    /// # Errors
    ///
    /// When the handle is not a persistent handle, the device cannot be opened, the TPM refuses
    /// a command or answers malformed, the key is not an ECC P-256 ECDSA-SHA256 signing key with
    /// the required attributes, or its test signature does not verify.
    pub fn open(transport: TpmTransport, handle: u32) -> Result<Self, TpmError> {
        Self::open_with_timeout(transport, handle, RESPONSE_TIMEOUT)
    }

    fn open_with_timeout(
        transport: TpmTransport,
        handle: u32,
        timeout: Duration,
    ) -> Result<Self, TpmError> {
        if handle >> 24 != 0x81 {
            return Err(TpmError::NotPersistentHandle(handle));
        }
        let worker = spawn_worker(transport, timeout, worker::Retry::Never)?;
        let response = worker.transact(marshal::read_public_command(handle))?;
        let public = marshal::parse_read_public(&response)?;
        Self::with_public(worker, handle, public)
    }

    /// The key at `handle` whose public area is `public` (already checked), used through
    /// `worker`; makes the test signature.
    fn with_public(
        worker: worker::Worker,
        handle: u32,
        public: marshal::PublicKey,
    ) -> Result<Self, TpmError> {
        let key = Self {
            inner: Arc::new(Inner {
                handle,
                spki: public.spki_der(),
                public,
                worker,
            }),
        };
        let signature = key.sign(SELF_TEST_MESSAGE)?;
        if !key.verify(SELF_TEST_MESSAGE, &signature) {
            return Err(TpmError::SelfTestFailed);
        }
        Ok(key)
    }

    /// The key's DER `SubjectPublicKeyInfo`, to compare with the client certificate's (rustls'
    /// `CertifiedKey::keys_match` does that through [`SigningKey::public_key`]).
    #[must_use]
    pub fn public_key_spki_der(&self) -> &[u8] {
        &self.inner.spki
    }

    /// The persistent handle of the key.
    #[must_use]
    pub fn handle(&self) -> u32 {
        self.inner.handle
    }

    /// Signs `message` with ECDSA P-256 over its SHA-256 digest, in the TPM, and returns the DER
    /// `ECDSA-Sig-Value`. Blocks until the TPM answers (about 70 ms on the gateway), at most
    /// [`RESPONSE_TIMEOUT`].
    ///
    /// # Errors
    ///
    /// When the TPM refuses, answers malformed or not in time, or the worker thread is gone.
    pub fn sign(&self, message: &[u8]) -> Result<Vec<u8>, TpmError> {
        let digest = ring::digest::digest(&ring::digest::SHA256, message);
        let mut sha256 = [0_u8; 32];
        sha256.copy_from_slice(digest.as_ref());
        let response = self
            .inner
            .worker
            .transact(marshal::sign_command(self.inner.handle, &sha256))?;
        marshal::parse_sign(&response)
    }

    /// Whether `signature` (DER) is this key's ECDSA P-256 SHA-256 signature of `message`.
    fn verify(&self, message: &[u8], signature: &[u8]) -> bool {
        UnparsedPublicKey::new(&ECDSA_P256_SHA256_ASN1, &self.inner.public.point)
            .verify(message, signature)
            .is_ok()
    }
}

/// Opens the transport (a device is opened by the caller's thread) and starts the thread that
/// owns it.
fn spawn_worker(
    transport: TpmTransport,
    timeout: Duration,
    retry: worker::Retry,
) -> Result<worker::Worker, TpmError> {
    let channel: Box<dyn TpmChannel> = match transport {
        TpmTransport::Device(path) => Box::new(worker::DeviceChannel::open(&path)?),
        TpmTransport::Channel(channel) => channel,
    };
    worker::Worker::spawn(channel, timeout, retry)
}

impl fmt::Debug for TpmKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TpmKey")
            .field("handle", &format_args!("{:#010x}", self.inner.handle))
            .finish_non_exhaustive()
    }
}

impl SigningKey for TpmKey {
    /// Only `ecdsa_secp256r1_sha256`, the one scheme the key can sign.
    fn choose_scheme(&self, offered: &[SignatureScheme]) -> Option<Box<dyn Signer>> {
        offered
            .contains(&SignatureScheme::ECDSA_NISTP256_SHA256)
            .then(|| Box::new(TpmSigner { key: self.clone() }) as Box<dyn Signer>)
    }

    fn public_key(&self) -> Option<SubjectPublicKeyInfoDer<'_>> {
        Some(SubjectPublicKeyInfoDer::from(self.public_key_spki_der()))
    }

    fn algorithm(&self) -> SignatureAlgorithm {
        SignatureAlgorithm::ECDSA
    }
}

/// A [`Signer`] for one handshake.
#[derive(Debug)]
struct TpmSigner {
    key: TpmKey,
}

impl Signer for TpmSigner {
    /// Blocks the calling thread until the TPM answers; rustls turns an error into a failed
    /// handshake.
    fn sign(&self, message: &[u8]) -> Result<Vec<u8>, rustls::Error> {
        self.key
            .sign(message)
            .map_err(|error| rustls::Error::General(format!("TPM signature failed: {error}")))
    }

    fn scheme(&self) -> SignatureScheme {
        SignatureScheme::ECDSA_NISTP256_SHA256
    }
}

#[cfg(test)]
mod tests {
    //! The key, its worker thread and the rustls integration against a fake TPM that signs with
    //! a software key. Parsing is tested in `marshal`; the real TPM test is
    //! `tests/tpm_swtpm.rs`.

    use std::collections::HashMap;
    use std::io;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use ring::rand::SystemRandom;
    use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair};
    use rustls::SignatureScheme;
    use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};
    use rustls::sign::{CertifiedKey, SigningKey};

    use super::marshal::tests::{READ_PUBLIC_RESPONSE, hex};
    use super::*;

    /// How long a test waits for something that happens at once unless the code is wrong.
    const WAIT: Duration = Duration::from_secs(30);

    fn sha256(message: &[u8]) -> [u8; 32] {
        ring::digest::digest(&ring::digest::SHA256, message)
            .as_ref()
            .try_into()
            .unwrap()
    }

    /// What the fake TPM does with the next command.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Mode {
        Answer,
        /// Wait until released.
        Block,
        Panic,
        IoError,
        /// Answer a sign command with another key's signature.
        WrongKey,
    }

    #[derive(Clone)]
    struct Control {
        mode: Arc<Mutex<Mode>>,
        /// Signalled when a blocked command starts waiting.
        entered: mpsc::SyncSender<()>,
        release: Arc<Mutex<mpsc::Receiver<()>>>,
        dropped: Arc<AtomicBool>,
        commands: Arc<Mutex<Vec<Vec<u8>>>>,
    }

    /// A TPM that holds one software P-256 key. It signs the digests of messages the test
    /// registered (ring signs messages, not digests).
    struct FakeTpm {
        key: EcdsaKeyPair,
        wrong_key: EcdsaKeyPair,
        messages: HashMap<[u8; 32], Vec<u8>>,
        control: Control,
    }

    impl Drop for FakeTpm {
        fn drop(&mut self) {
            self.control.dropped.store(true, Ordering::SeqCst);
        }
    }

    fn generate() -> (EcdsaKeyPair, Vec<u8>) {
        let random = SystemRandom::new();
        let pkcs8 =
            EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &random).unwrap();
        let key =
            EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), &random)
                .unwrap();
        (key, pkcs8.as_ref().to_vec())
    }

    impl FakeTpm {
        /// The fake and the receiving ends the test keeps: entered, release sender.
        fn new(messages: &[&[u8]]) -> (Self, Control, mpsc::Receiver<()>, mpsc::Sender<()>) {
            let (entered, entered_rx) = mpsc::sync_channel(16);
            let (release_tx, release) = mpsc::channel();
            let control = Control {
                mode: Arc::new(Mutex::new(Mode::Answer)),
                entered,
                release: Arc::new(Mutex::new(release)),
                dropped: Arc::new(AtomicBool::new(false)),
                commands: Arc::new(Mutex::new(Vec::new())),
            };
            let mut all = vec![SELF_TEST_MESSAGE];
            all.extend_from_slice(messages);
            let fake = Self {
                key: generate().0,
                wrong_key: generate().0,
                messages: all.iter().map(|m| (sha256(m), m.to_vec())).collect(),
                control: control.clone(),
            };
            (fake, control, entered_rx, release_tx)
        }

        /// The ReadPublic response for this fake's key: the fixture with the point replaced.
        fn read_public(&self) -> Vec<u8> {
            let mut response = hex(READ_PUBLIC_RESPONSE);
            let point = self.key.public_key().as_ref();
            response[34..66].copy_from_slice(&point[1..33]);
            response[68..100].copy_from_slice(&point[33..65]);
            response
        }

        fn sign_response(&self, digest: &[u8], key: &EcdsaKeyPair) -> Vec<u8> {
            let message = &self.messages[digest];
            let signature = key.sign(&SystemRandom::new(), message).unwrap();
            let (r, s) = signature.as_ref().split_at(32);
            let mut response = hex("8002_0000005b_00000000_00000048_0018_000b_0020");
            response.extend_from_slice(r);
            response.extend_from_slice(&[0x00, 0x20]);
            response.extend_from_slice(s);
            response.extend_from_slice(&hex("0000_01_0000"));
            response
        }
    }

    impl TpmChannel for FakeTpm {
        fn transact(&mut self, command: &[u8]) -> io::Result<Vec<u8>> {
            self.control.commands.lock().unwrap().push(command.to_vec());
            let mode = *self.control.mode.lock().unwrap();
            match mode {
                Mode::Block => {
                    self.control.entered.send(()).unwrap();
                    let _ = self.control.release.lock().unwrap().recv();
                }
                Mode::Panic => panic!("the fake TPM panics, as a worker bug would"),
                Mode::IoError => return Err(io::Error::other("the fake TPM is unplugged")),
                Mode::Answer | Mode::WrongKey => {}
            }
            let code = u32::from_be_bytes(command[6..10].try_into().unwrap());
            Ok(match code {
                0x173 => self.read_public(),
                0x15d => {
                    // The digest follows the header (10), handle (4) and auth area (4 + 9), and
                    // its own size (2).
                    let digest = &command[29..61];
                    let key = if mode == Mode::WrongKey {
                        &self.wrong_key
                    } else {
                        &self.key
                    };
                    self.sign_response(digest, key)
                }
                _ => hex("80010000000a00000143"),
            })
        }
    }

    fn open(fake: FakeTpm) -> Result<TpmKey, TpmError> {
        TpmKey::open(TpmTransport::Channel(Box::new(fake)), DEFAULT_KEY_HANDLE)
    }

    fn verify(spki_point: &[u8], message: &[u8], signature: &[u8]) -> bool {
        UnparsedPublicKey::new(&ECDSA_P256_SHA256_ASN1, spki_point)
            .verify(message, signature)
            .is_ok()
    }

    #[test]
    fn open_reads_the_key_signs_a_test_message_and_signs_with_it() {
        let (fake, control, _entered, _release) = FakeTpm::new(&[b"hello"]);
        let point = fake.key.public_key().as_ref().to_vec();
        let key = open(fake).unwrap();
        // The SPKI is the fixed P-256 prefix and the point.
        assert_eq!(key.public_key_spki_der().len(), 91);
        assert_eq!(key.public_key_spki_der()[26..], point[..]);
        assert_eq!(key.handle(), DEFAULT_KEY_HANDLE);
        let signature = key.sign(b"hello").unwrap();
        assert!(verify(&point, b"hello", &signature));
        // ReadPublic, the self-test signature, then the caller's.
        let commands = control.commands.lock().unwrap();
        let codes: Vec<u32> = commands
            .iter()
            .map(|command| u32::from_be_bytes(command[6..10].try_into().unwrap()))
            .collect();
        assert_eq!(codes, [0x173, 0x15d, 0x15d]);
        assert_eq!(commands[2][29..61], sha256(b"hello"));
    }

    #[test]
    fn handles_outside_the_persistent_range_are_refused_before_any_command() {
        for handle in [0x8000_0000, 0x4000_0001, 0x8200_0000, 0] {
            let (fake, control, _entered, _release) = FakeTpm::new(&[]);
            let error = TpmKey::open(TpmTransport::Channel(Box::new(fake)), handle).unwrap_err();
            assert!(matches!(error, TpmError::NotPersistentHandle(h) if h == handle));
            assert!(control.commands.lock().unwrap().is_empty());
        }
    }

    #[test]
    fn a_device_that_is_missing_or_not_a_character_device_is_refused() {
        let missing = TpmKey::open(
            TpmTransport::Device("/nonexistent/tpmrm0".into()),
            DEFAULT_KEY_HANDLE,
        )
        .unwrap_err();
        assert!(matches!(missing, TpmError::Open { .. }), "{missing}");
        let file = std::env::temp_dir().join(format!("agent-tpm-test-{}", std::process::id()));
        std::fs::write(&file, b"").unwrap();
        let error = TpmKey::open(TpmTransport::Device(file.clone()), DEFAULT_KEY_HANDLE);
        std::fs::remove_file(file).unwrap();
        let error = error.unwrap_err();
        assert!(matches!(error, TpmError::Open { .. }), "{error}");
        assert!(
            error.to_string().contains("not a character device"),
            "{error}"
        );
        // A character device that is not a TPM answers nothing.
        let error =
            TpmKey::open(TpmTransport::Device("/dev/null".into()), DEFAULT_KEY_HANDLE).unwrap_err();
        assert!(matches!(error, TpmError::Malformed(_)), "{error}");
    }

    #[test]
    fn a_key_whose_test_signature_does_not_verify_is_refused() {
        let (fake, control, _entered, _release) = FakeTpm::new(&[]);
        *control.mode.lock().unwrap() = Mode::WrongKey;
        assert!(matches!(open(fake), Err(TpmError::SelfTestFailed)));
    }

    #[test]
    fn tpm_errors_at_open_are_returned_and_stop_the_thread() {
        let (fake, control, _entered, _release) = FakeTpm::new(&[]);
        *control.mode.lock().unwrap() = Mode::IoError;
        let error = open(fake).unwrap_err();
        assert!(matches!(error, TpmError::Io(_)), "{error}");
        wait_for_drop(&control);
    }

    fn wait_for_drop(control: &Control) {
        let started = Instant::now();
        while !control.dropped.load(Ordering::SeqCst) {
            assert!(
                started.elapsed() < WAIT,
                "the TPM thread did not close the channel"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn dropping_the_last_clone_stops_the_thread_and_closes_the_channel() {
        let (fake, control, _entered, _release) = FakeTpm::new(&[]);
        let key = open(fake).unwrap();
        let clone = key.clone();
        let signer = clone
            .choose_scheme(&[SignatureScheme::ECDSA_NISTP256_SHA256])
            .unwrap();
        drop(key);
        drop(clone);
        // The signer still holds the key.
        std::thread::sleep(Duration::from_millis(50));
        assert!(!control.dropped.load(Ordering::SeqCst));
        drop(signer);
        wait_for_drop(&control);
    }

    #[test]
    fn a_dead_thread_makes_signing_fail_instead_of_hanging() {
        let (fake, control, _entered, _release) = FakeTpm::new(&[b"m"]);
        let key = open(fake).unwrap();
        *control.mode.lock().unwrap() = Mode::Panic;
        // The request in flight when the thread dies, then every later one.
        assert!(matches!(key.sign(b"m"), Err(TpmError::WorkerStopped)));
        assert!(matches!(key.sign(b"m"), Err(TpmError::WorkerStopped)));
        let signer = key
            .choose_scheme(&[SignatureScheme::ECDSA_NISTP256_SHA256])
            .unwrap();
        let error = signer.sign(b"m").unwrap_err().to_string();
        assert!(error.contains("TPM signature failed"), "{error}");
    }

    #[test]
    fn an_io_error_fails_one_request_and_the_next_one_works() {
        let (fake, control, _entered, _release) = FakeTpm::new(&[b"m"]);
        let point = fake.key.public_key().as_ref().to_vec();
        let key = open(fake).unwrap();
        *control.mode.lock().unwrap() = Mode::IoError;
        assert!(matches!(key.sign(b"m"), Err(TpmError::Io(_))));
        *control.mode.lock().unwrap() = Mode::Answer;
        assert!(verify(&point, b"m", &key.sign(b"m").unwrap()));
    }

    #[test]
    fn a_tpm_that_does_not_answer_times_out_and_a_full_queue_is_refused() {
        let (fake, control, entered, release) = FakeTpm::new(&[b"m"]);
        let key = TpmKey::open_with_timeout(
            TpmTransport::Channel(Box::new(fake)),
            DEFAULT_KEY_HANDLE,
            Duration::from_millis(200),
        )
        .unwrap();
        *control.mode.lock().unwrap() = Mode::Block;
        // The first request occupies the TPM until released; waiting for it times out.
        assert!(matches!(key.sign(b"m"), Err(TpmError::Timeout(_))));
        entered
            .recv_timeout(WAIT)
            .expect("the fake TPM got the command");
        // The queue takes QUEUE_CAPACITY more requests, then refuses.
        let worker = &key.inner.worker;
        let command = marshal::sign_command(DEFAULT_KEY_HANDLE, &sha256(b"m"));
        let pending: Vec<_> = (0..worker::QUEUE_CAPACITY)
            .map(|_| worker.submit(command.clone()).unwrap())
            .collect();
        assert!(matches!(worker.submit(command), Err(TpmError::Busy)));
        // Released, the TPM answers every queued request.
        *control.mode.lock().unwrap() = Mode::Answer;
        release.send(()).unwrap();
        for request in pending {
            marshal::parse_sign(&request.wait(WAIT).unwrap()).unwrap();
        }
    }

    /// A self-signed certificate for `public_key` made by rcgen: the subject key is what
    /// matters to `keys_match`, not who signed it.
    fn certificate_for(spki: &[u8]) -> CertificateDer<'static> {
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
        let issuer_params = rcgen::CertificateParams::new(vec!["ca.test".to_owned()]).unwrap();
        let issuer = rcgen::Issuer::new(issuer_params, issuer_key);
        let params = rcgen::CertificateParams::new(vec!["gw.test".to_owned()]).unwrap();
        params
            .signed_by(&Public(spki[26..].to_vec()), &issuer)
            .unwrap()
            .der()
            .clone()
    }

    #[test]
    fn rustls_sees_an_ecdsa_p256_key_whose_spki_matches_only_its_own_certificate() {
        let message = b"what rustls hands the signer: the whole CertificateVerify content";
        let (fake, _control, _entered, _release) = FakeTpm::new(&[message]);
        let key = open(fake).unwrap();
        assert_eq!(key.algorithm(), SignatureAlgorithm::ECDSA);
        assert!(key.choose_scheme(&[]).is_none());
        assert!(
            key.choose_scheme(&[
                SignatureScheme::ECDSA_NISTP384_SHA384,
                SignatureScheme::ED25519,
                SignatureScheme::RSA_PSS_SHA256,
            ])
            .is_none()
        );
        let signer = key
            .choose_scheme(&[
                SignatureScheme::RSA_PSS_SHA256,
                SignatureScheme::ECDSA_NISTP256_SHA256,
            ])
            .unwrap();
        assert_eq!(signer.scheme(), SignatureScheme::ECDSA_NISTP256_SHA256);
        let signature = signer.sign(message).unwrap();
        assert!(verify(
            &key.public_key_spki_der()[26..],
            message,
            &signature
        ));

        let matching = CertifiedKey::new(
            vec![certificate_for(key.public_key_spki_der())],
            Arc::new(key.clone()),
        );
        matching.keys_match().unwrap();
        // A certificate for any other key does not match.
        let (_, other_pkcs8) = generate();
        let other = rustls::crypto::ring::default_provider()
            .key_provider
            .load_private_key(PrivatePkcs8KeyDer::from(other_pkcs8).into())
            .unwrap();
        let other_spki = other.public_key().unwrap().as_ref().to_vec();
        let mismatched = CertifiedKey::new(vec![certificate_for(&other_spki)], Arc::new(key));
        assert!(mismatched.keys_match().is_err());
    }
}
