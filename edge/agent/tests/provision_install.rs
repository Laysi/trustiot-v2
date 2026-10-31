//! `trustiot-agent provision install` (`agent::provision::install::run`, what the executable
//! runs) against a real TPM 2.0 implementation: swtpm (libtpms) in a throwaway container
//! (`testkit::swtpm`), whose gateway key the agent's own `ensure_key` creates
//! (Laysi/trustiot-v2#617).
//!
//! - wrong certificates are refused with status 8 and leave the directories exactly as they were
//!   (no file, or the files from before with their inodes): a gateway certificate for another
//!   key or another hardware ID, a chain that does not lead to the root sent, a root that is not
//!   a self-signed CA, the certificates out of order, no issuing CA, a private key or other text
//!   on standard input, too much input, and a root the configured root CRL does not fit;
//! - the installed files have the owner and mode of the rules, and the agent's own start-up
//!   checks (`Setup::new_with`, with the swtpm key) accept them;
//! - a rerun with the same certificates writes nothing (same inodes and contents); a new root
//!   replaces both files with new ones;
//! - when the root's owner cannot be set (the test does not run as root), nothing is renamed and
//!   no staged file is left;
//! - a configuration whose key is not the TPM's, or whose directories do not exist, ends with
//!   status 9 before anything is read;
//! - a root CRL sent after the certificates (Laysi/trustiot-v2#832) is installed as `broker.crl`
//!   with the owner of its directory, the agent starts with it and checks the issuing CAs against
//!   it; it only moves forward (a higher number installed is kept, the same number with other
//!   content refuses everything); one the root sent did not sign, a tampered one, one that
//!   revokes the issuing CA, or a misplaced one is refused with nothing written; another root's
//!   root CRL is replaced together with that root; and a root CRL sent while `broker.crl` is not
//!   configured ends with status 9, nothing written; the installed root CRL is read only under
//!   the lock, so a newer one written while an install waits for it is kept.
//!
//! The executable itself (arguments, help, no installation) is tested in `tests/binary.rs`.
//! Without podman or docker the tests skip, unless `CI` is set, where they fail.

#![allow(
    clippy::unwrap_used,
    reason = "test helpers unwrap like the tests themselves (clippy.toml allows it in tests)"
)]

use std::io;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use agent::config::Config;
use agent::provision::install::{Exit, FILE_MODE, MAX_INPUT_BYTES, Options, Owner, Owners, run};
use agent::revocation::{Fetched, Marker, Source, Update, marker_path};
use agent::setup::Setup;
use agent::tls::{BrokerTrust, RootCrl, Validity, check_chain_under};
use agent::tpm::{DEFAULT_KEY_HANDLE, TpmChannel, TpmTransport, ensure_key};
use base64::Engine as _;
use protocol::GatewayId;
use rcgen::{
    BasicConstraints, CertificateParams, CertificateRevocationListParams, DnType,
    ExtendedKeyUsagePurpose, IsCa, Issuer, KeyIdMethod, KeyPair, KeyUsagePurpose,
    PublicKeyData as _, RevocationReason, RevokedCertParams, SerialNumber,
};
use testkit::mqtt::ScratchDir;
use testkit::swtpm::{Session, Swtpm};
use x509_parser::prelude::{FromDer as _, X509Certificate};

const HARDWARE_ID: &str = "tg-0001";

/// One TPM connection, as a process opening `/dev/tpmrm0` would have.
struct Connection(Session);

impl TpmChannel for Connection {
    fn transact(&mut self, command: &[u8]) -> io::Result<Vec<u8>> {
        self.0.transact(command)
    }
}

/// The uncompressed P-256 point of a SubjectPublicKeyInfo, as rcgen's public key.
struct PublicPoint(Vec<u8>);

impl rcgen::PublicKeyData for PublicPoint {
    fn der_bytes(&self) -> &[u8] {
        &self.0
    }

    fn algorithm(&self) -> &'static rcgen::SignatureAlgorithm {
        &rcgen::PKCS_ECDSA_P256_SHA256
    }
}

fn pem(der: &[u8]) -> String {
    let body = base64::engine::general_purpose::STANDARD.encode(der);
    let mut text = "-----BEGIN CERTIFICATE-----\n".to_owned();
    for line in body.as_bytes().chunks(64) {
        text.push_str(std::str::from_utf8(line).unwrap());
        text.push('\n');
    }
    text.push_str("-----END CERTIFICATE-----\n");
    text
}

fn ca_params(name: &str, constraints: Option<BasicConstraints>) -> CertificateParams {
    let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
    params.distinguished_name = rcgen::DistinguishedName::new();
    params.distinguished_name.push(DnType::CommonName, name);
    params.is_ca = constraints.map_or(IsCa::NoCa, IsCa::Ca);
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    params
}

/// A root, an issuing CA under it (path length 0, as the certs service's), and gateway
/// certificates under that.
struct Pki {
    root: Vec<u8>,
    root_issuer: Issuer<'static, KeyPair>,
    issuing: Vec<u8>,
    issuer: Issuer<'static, KeyPair>,
}

impl Pki {
    fn new() -> Self {
        Self::with_root(Some(BasicConstraints::Unconstrained))
    }

    /// With the root's basicConstraints (`None`: not a CA).
    fn with_root(constraints: Option<BasicConstraints>) -> Self {
        let root_key = KeyPair::generate().unwrap();
        let root_params = ca_params("TrustIoT Test Root CA", constraints);
        let root = root_params.self_signed(&root_key).unwrap().der().to_vec();
        let root_issuer = Issuer::new(root_params, root_key);
        let issuing_key = KeyPair::generate().unwrap();
        let issuing_params = ca_params(
            "TrustIoT Test Issuing CA",
            Some(BasicConstraints::Constrained(0)),
        );
        let issuing = issuing_params
            .signed_by(&issuing_key, &root_issuer)
            .unwrap()
            .der()
            .to_vec();
        Self {
            root,
            root_issuer,
            issuing,
            issuer: Issuer::new(issuing_params, issuing_key),
        }
    }

    /// A gateway certificate (CN `common_name`, clientAuth, digitalSignature) for the P-256 key
    /// whose SubjectPublicKeyInfo is `spki`.
    fn leaf(&self, common_name: &str, spki: &[u8]) -> Vec<u8> {
        self.leaf_from(common_name, spki, None)
    }

    /// [`Pki::leaf`], valid from `year` (rcgen's default when `None`), as a renewal issues it.
    fn leaf_from(&self, common_name: &str, spki: &[u8], year: Option<i32>) -> Vec<u8> {
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        if let Some(year) = year {
            params.not_before = rcgen::date_time_ymd(year, 1, 1);
        }
        params.distinguished_name = rcgen::DistinguishedName::new();
        params
            .distinguished_name
            .push(DnType::CommonName, common_name);
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params
            .signed_by(&PublicPoint(spki[26..].to_vec()), &self.issuer)
            .unwrap()
            .der()
            .to_vec()
    }

    /// What the workstation sends: the chain (leaf, issuing CA) and the root, PEM.
    fn input(&self, leaf: &[u8]) -> String {
        format!("{}{}{}", pem(leaf), pem(&self.issuing), pem(&self.root))
    }

    /// Another issuing CA under this root, DER, with its issuer.
    fn another_issuing(&self) -> (Vec<u8>, Issuer<'static, KeyPair>) {
        let key = KeyPair::generate().unwrap();
        let params = ca_params(
            "TrustIoT Test Issuing CA 2",
            Some(BasicConstraints::Constrained(0)),
        );
        let der = params
            .signed_by(&key, &self.root_issuer)
            .unwrap()
            .der()
            .to_vec();
        (der, Issuer::new(params, key))
    }

    /// An empty root CRL signed by this root, PEM.
    fn crl(&self) -> String {
        self.crl_revoking(None)
    }

    /// A root CRL signed by this root that revokes `revoked` (DER), if given.
    fn crl_revoking(&self, revoked: Option<&[u8]>) -> String {
        self.crl_numbered(1, revoked)
    }

    /// A root CRL numbered `number`, signed by this root, that revokes `revoked` (DER), if given.
    fn crl_numbered(&self, number: u64, revoked: Option<&[u8]>) -> String {
        let revoked_certs = revoked
            .map(|der| {
                let (_, certificate) = X509Certificate::from_der(der).unwrap();
                RevokedCertParams {
                    serial_number: SerialNumber::from_slice(certificate.raw_serial()),
                    revocation_time: rcgen::date_time_ymd(2021, 1, 1),
                    reason_code: Some(RevocationReason::KeyCompromise),
                    invalidity_date: None,
                }
            })
            .into_iter()
            .collect();
        CertificateRevocationListParams {
            this_update: rcgen::date_time_ymd(2020, 1, 1),
            next_update: rcgen::date_time_ymd(2099, 1, 1),
            crl_number: SerialNumber::from(number),
            issuing_distribution_point: None,
            revoked_certs,
            key_identifier_method: KeyIdMethod::Sha256,
        }
        .signed_by(&self.root_issuer)
        .unwrap()
        .pem()
        .unwrap()
    }
}

/// A software TPM with the gateway key, and a gateway's directories and configuration.
struct Gateway {
    swtpm: Swtpm,
    /// The TPM key's SubjectPublicKeyInfo.
    spki: Vec<u8>,
    dir: ScratchDir,
    /// `dir`'s path without symbolic links (the system's temporary directory may be reached
    /// through one): install refuses a path through a link.
    root: PathBuf,
    config: PathBuf,
    /// `/etc/trustiot` and `/var/lib/trustiot-agent` of this gateway.
    etc: PathBuf,
    state: PathBuf,
}

impl Gateway {
    fn start() -> Option<Self> {
        let swtpm = Swtpm::start()?;
        swtpm.create_standard_srk();
        let provisioned = ensure_key(
            TpmTransport::Channel(Box::new(Connection(swtpm.session()))),
            DEFAULT_KEY_HANDLE,
        )
        .unwrap();
        let spki = provisioned.key.public_key_spki_der().to_vec();
        drop(provisioned);
        let dir = ScratchDir::new("agent-provision-install");
        let root = dir.path().canonicalize().unwrap();
        let etc = root.join("etc");
        let state = root.join("state");
        std::fs::create_dir(&etc).unwrap();
        std::fs::create_dir(&state).unwrap();
        // Whatever the umask: install needs directories and a configuration that only their
        // owner can write.
        for directory in [&root, &etc, &state] {
            std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let mut gateway = Self {
            swtpm,
            spki,
            dir,
            config: PathBuf::new(),
            root,
            etc,
            state,
        };
        // The outer TLS's trust: any CA certificate; this test never connects.
        let outer = gateway.write("outer-ca.pem", pem(&Pki::new().root));
        gateway.config = gateway.write(
            "agent.toml",
            config_text(
                &outer,
                &gateway.etc,
                &gateway.state,
                "key = { source = \"tpm\" }",
                "",
            ),
        );
        Some(gateway)
    }

    /// Writes `contents` to `name` in the scratch directory, mode 0644, and returns its path.
    fn write(&self, name: &str, contents: impl AsRef<[u8]>) -> PathBuf {
        let path = self.root.join(name);
        std::fs::write(&path, contents).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        path
    }

    fn chain_path(&self) -> PathBuf {
        self.state.join("gateway.pem")
    }

    fn root_path(&self) -> PathBuf {
        self.etc.join("broker-ca.pem")
    }

    fn options(&self) -> Options {
        self.options_with(&self.config)
    }

    fn options_with(&self, config: &Path) -> Options {
        Options {
            hardware_id: GatewayId::new(HARDWARE_ID).unwrap(),
            config: config.to_owned(),
        }
    }

    fn open_tpm(&self) -> impl FnMut(&Path) -> TpmTransport + '_ {
        |device| {
            assert_eq!(device, Path::new("/dev/tpmrm0"), "the configured device");
            TpmTransport::Channel(Box::new(Connection(self.swtpm.session())))
        }
    }

    /// Runs `provision install` with `input` on standard input.
    fn install(&self, options: &Options, owners: &Owners, input: &[u8]) -> Ran {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let exit = run(
            options,
            owners,
            &mut self.open_tpm(),
            &mut &input[..],
            &mut stdout,
            &mut stderr,
        );
        Ran {
            exit,
            stdout: String::from_utf8(stdout).unwrap(),
            stderr: String::from_utf8(stderr).unwrap(),
        }
    }

    /// Every file under the gateway's two directories: name, inode, mode, owner, contents.
    fn files(&self) -> Vec<FileEntry> {
        let mut files = Vec::new();
        for directory in [&self.etc, &self.state] {
            for entry in std::fs::read_dir(directory).unwrap() {
                let path = entry.unwrap().path();
                let metadata = std::fs::symlink_metadata(&path).unwrap();
                files.push((
                    path.display().to_string(),
                    metadata.ino(),
                    metadata.mode(),
                    metadata.uid(),
                    metadata.gid(),
                    std::fs::read(&path).unwrap(),
                ));
            }
        }
        files.sort();
        files
    }

    /// The agent's start-up checks with this configuration and the swtpm key.
    fn setup(&self) -> Setup {
        let config = Config::load(&self.config).unwrap();
        Setup::new_with(&config, &mut self.open_tpm())
            .unwrap_or_else(|error| panic!("the agent refuses the installed files: {error}"))
    }
}

fn config_text(outer: &Path, etc: &Path, state: &Path, key: &str, broker_extra: &str) -> String {
    format!(
        "[cloud]\nurl = \"wss://gw.trustiot.example/mqtt\"\ntrust = [\"{}\"]\n\n\
         [broker]\nca = \"{}\"\n{broker_extra}\n\n\
         [identity]\ncertificate = \"{}\"\n{key}\n",
        outer.display(),
        etc.join("broker-ca.pem").display(),
        state.join("gateway.pem").display(),
    )
}

/// One file of [`Gateway::files`]: name, inode, mode, owner (uid, gid), contents.
type FileEntry = (String, u64, u32, u32, u32, Vec<u8>);

struct Ran {
    exit: Exit,
    stdout: String,
    stderr: String,
}

impl Ran {
    fn installed(&self) -> &str {
        assert_eq!(self.exit, Exit::Success, "stderr:\n{}", self.stderr);
        &self.stdout
    }

    fn failed(&self, exit: Exit, expected: &str) {
        assert_eq!(self.exit, exit, "stderr:\n{}", self.stderr);
        assert!(self.stdout.is_empty(), "{}", self.stdout);
        assert!(
            self.stderr.contains(expected),
            "expected {expected:?} in:\n{}",
            self.stderr
        );
    }
}

/// The owner this process gives a new file.
fn me(dir: &ScratchDir) -> Owner {
    let metadata = std::fs::metadata(dir.write("owner-probe", "")).unwrap();
    std::fs::remove_file(dir.path().join("owner-probe")).unwrap();
    Owner {
        uid: metadata.uid(),
        gid: metadata.gid(),
    }
}

/// A software P-256 key's SubjectPublicKeyInfo.
fn software_spki() -> Vec<u8> {
    KeyPair::generate().unwrap().subject_public_key_info()
}

#[test]
fn wrong_certificates_are_refused_and_nothing_is_written() {
    let Some(gateway) = Gateway::start() else {
        return;
    };
    let mine = me(&gateway.dir);
    let owners = Owners {
        chain: mine,
        root: mine,
    };
    let pki = Pki::new();
    let other = Pki::new();
    let leaf = pki.leaf(HARDWARE_ID, &gateway.spki);
    let not_a_ca_root = Pki::with_root(None);
    let key = KeyPair::generate().unwrap().serialize_pem();
    let cases: Vec<(String, &str)> = vec![
        (
            pki.input(&pki.leaf(HARDWARE_ID, &software_spki())),
            "not the TPM key's",
        ),
        (
            pki.input(&pki.leaf("tg-0002", &gateway.spki)),
            "not the hardware ID tg-0001",
        ),
        (
            format!("{}{}{}", pem(&leaf), pem(&pki.issuing), pem(&other.root)),
            "does not lead to that root",
        ),
        (
            not_a_ca_root.input(&not_a_ca_root.leaf(HARDWARE_ID, &gateway.spki)),
            "not a root CA certificate",
        ),
        (
            format!("{}{}{}", pem(&pki.root), pem(&pki.issuing), pem(&leaf)),
            "out of order",
        ),
        (
            format!("{}{}", pem(&leaf), pem(&pki.root)),
            "without the issuing CA",
        ),
        (pem(&leaf), "without the issuing CA"),
        (format!("{}{key}", pki.input(&leaf)), "not a CERTIFICATE"),
        (format!("cookie: x\n{}", pki.input(&leaf)), "text outside"),
        (
            format!("{}{}", pki.input(&leaf), "\n".repeat(MAX_INPUT_BYTES)),
            "larger than",
        ),
        (String::new(), "holds no certificate"),
    ];

    // Into empty directories, then over installed files.
    for round in ["empty", "installed"] {
        let before = gateway.files();
        for (index, (input, expected)) in cases.iter().enumerate() {
            let ran = gateway.install(&gateway.options(), &owners, input.as_bytes());
            ran.failed(Exit::Refused, expected);
            assert!(
                ran.stderr.contains("nothing was written"),
                "{round} {index}: {}",
                ran.stderr
            );
            assert_eq!(gateway.files(), before, "{round} {index}: {expected}");
        }
        if round == "empty" {
            assert!(before.is_empty(), "{before:?}");
            gateway
                .install(&gateway.options(), &owners, pki.input(&leaf).as_bytes())
                .installed();
        }
    }

    // A root CRL from another root: with the new root the agent would not start. It lives in a
    // directory of its own, so `files()` sees the installed files only (and never reads the FIFO
    // below).
    let crl_directory = gateway.root.join("crl");
    std::fs::create_dir(&crl_directory).unwrap();
    std::fs::set_permissions(&crl_directory, std::fs::Permissions::from_mode(0o755)).unwrap();
    let crl = crl_directory.join("root-crl.pem");
    std::fs::write(&crl, other.crl()).unwrap();
    let outer = gateway.root.join("outer-ca.pem");
    let with_crl = gateway.write(
        "agent-crl.toml",
        config_text(
            &outer,
            &gateway.etc,
            &gateway.state,
            "key = { source = \"tpm\" }",
            &format!("crl = \"{}\"", crl.display()),
        ),
    );
    let before = gateway.files();
    gateway
        .install(
            &gateway.options_with(&with_crl),
            &owners,
            pki.input(&leaf).as_bytes(),
        )
        .failed(Exit::Refused, "broker.crl");
    assert_eq!(gateway.files(), before);
    // That root's CRL revokes the issuing CA: the broker would refuse the gateway.
    std::fs::write(&crl, pki.crl_revoking(Some(&pki.issuing))).unwrap();
    gateway
        .install(
            &gateway.options_with(&with_crl),
            &owners,
            pki.input(&leaf).as_bytes(),
        )
        .failed(Exit::Refused, "does not pass as a client certificate");
    assert_eq!(gateway.files(), before);
    // Running as root, install reads broker.crl only as a regular file: a symbolic link (here
    // to a usable CRL) or a FIFO, which a plain open would block on, is refused without reading.
    let usable = gateway.write("usable-crl.pem", pki.crl());
    std::fs::remove_file(&crl).unwrap();
    std::os::unix::fs::symlink(&usable, &crl).unwrap();
    gateway
        .install(
            &gateway.options_with(&with_crl),
            &owners,
            pki.input(&leaf).as_bytes(),
        )
        .failed(Exit::Configuration, "is not a regular file");
    assert_eq!(gateway.files(), before);
    std::fs::remove_file(&crl).unwrap();
    let fifo = std::ffi::CString::new(crl.as_os_str().as_encoded_bytes()).unwrap();
    #[allow(unsafe_code, reason = "mkfifo(3) has no safe wrapper in std")]
    // SAFETY: `fifo` is a NUL-terminated path that outlives the call.
    let made = unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) };
    assert_eq!(made, 0, "mkfifo: {}", io::Error::last_os_error());
    gateway
        .install(
            &gateway.options_with(&with_crl),
            &owners,
            pki.input(&leaf).as_bytes(),
        )
        .failed(Exit::Configuration, "is not a regular file");
    assert_eq!(gateway.files(), before);
    std::fs::remove_file(&crl).unwrap();
    // With that root's CRL revoking nothing it installs, and is then unchanged.
    std::fs::write(&crl, pki.crl()).unwrap();
    let stdout = gateway
        .install(
            &gateway.options_with(&with_crl),
            &owners,
            pki.input(&leaf).as_bytes(),
        )
        .installed()
        .to_owned();
    assert_eq!(stdout.matches(": unchanged\n").count(), 2, "{stdout}");
}

#[test]
fn the_installed_files_pass_the_agents_checks_a_rerun_writes_nothing_and_a_new_root_replaces_them()
{
    let Some(gateway) = Gateway::start() else {
        return;
    };
    let mine = me(&gateway.dir);
    let owners = Owners {
        chain: mine,
        root: mine,
    };
    let pki = Pki::new();
    let leaf = pki.leaf(HARDWARE_ID, &gateway.spki);
    let input = pki.input(&leaf);

    let stdout = gateway
        .install(&gateway.options(), &owners, input.as_bytes())
        .installed()
        .to_owned();
    assert_eq!(
        stdout,
        format!(
            "{}: written\n{}: written\n",
            gateway.chain_path().display(),
            gateway.root_path().display()
        )
    );
    assert_eq!(
        std::fs::read_to_string(gateway.chain_path()).unwrap(),
        format!("{}{}", pem(&leaf), pem(&pki.issuing))
    );
    assert_eq!(
        std::fs::read_to_string(gateway.root_path()).unwrap(),
        pem(&pki.root)
    );
    for path in [gateway.chain_path(), gateway.root_path()] {
        let metadata = std::fs::symlink_metadata(&path).unwrap();
        assert!(metadata.is_file(), "{}", path.display());
        assert_eq!(metadata.mode() & 0o7777, FILE_MODE, "{}", path.display());
        assert_eq!((metadata.uid(), metadata.gid()), (mine.uid, mine.gid));
    }
    let files = gateway.files();
    assert_eq!(files.len(), 2, "no staged file is left: {files:?}");

    let setup = gateway.setup();
    assert_eq!(setup.gateway.as_str(), HARDWARE_ID);
    assert_eq!(setup.certificate_validity, Validity::Current);
    drop(setup);

    // The same certificates again, as text with other line endings: nothing is written.
    let stdout = gateway
        .install(
            &gateway.options(),
            &owners,
            input.replace('\n', "\r\n").as_bytes(),
        )
        .installed()
        .to_owned();
    assert_eq!(stdout.matches(": unchanged\n").count(), 2, "{stdout}");
    assert_eq!(gateway.files(), files, "a rerun changed a file");

    // A new root (and issuing CA, and certificate for the same TPM key): both files replaced.
    let new = Pki::new();
    let new_leaf = new.leaf(HARDWARE_ID, &gateway.spki);
    let stdout = gateway
        .install(&gateway.options(), &owners, new.input(&new_leaf).as_bytes())
        .installed()
        .to_owned();
    assert_eq!(stdout.matches(": written\n").count(), 2, "{stdout}");
    let replaced = gateway.files();
    assert_eq!(replaced.len(), 2, "{replaced:?}");
    for (now, before) in replaced.iter().zip(&files) {
        assert_eq!(now.0, before.0);
        assert_ne!(now.1, before.1, "{} kept its inode", now.0);
        assert_ne!(now.5, before.5, "{} kept its contents", now.0);
        assert_eq!(now.2 & 0o7777, FILE_MODE);
    }
    assert_eq!(
        std::fs::read_to_string(gateway.root_path()).unwrap(),
        pem(&new.root)
    );
    assert_eq!(gateway.setup().gateway.as_str(), HARDWARE_ID);

    // Only the issuing CA changed (same root): only the chain is written.
    let mut same_root = Pki::new();
    same_root.root = new.root.clone();
    let issuing_key = KeyPair::generate().unwrap();
    let issuing_params = ca_params(
        "TrustIoT Test Issuing CA",
        Some(BasicConstraints::Constrained(0)),
    );
    same_root.issuing = issuing_params
        .signed_by(&issuing_key, &new.root_issuer)
        .unwrap()
        .der()
        .to_vec();
    same_root.issuer = Issuer::new(issuing_params, issuing_key);
    let root_inode = std::fs::metadata(gateway.root_path()).unwrap().ino();
    let stdout = gateway
        .install(
            &gateway.options(),
            &owners,
            same_root
                .input(&same_root.leaf(HARDWARE_ID, &gateway.spki))
                .as_bytes(),
        )
        .installed()
        .to_owned();
    assert_eq!(
        stdout,
        format!(
            "{}: written\n{}: unchanged\n",
            gateway.chain_path().display(),
            gateway.root_path().display()
        )
    );
    assert_eq!(
        std::fs::metadata(gateway.root_path()).unwrap().ino(),
        root_inode
    );
    assert_eq!(gateway.setup().gateway.as_str(), HARDWARE_ID);
}

#[test]
fn a_renewed_certificate_is_never_replaced_by_an_older_one() {
    let Some(gateway) = Gateway::start() else {
        return;
    };
    let mine = me(&gateway.dir);
    let owners = Owners {
        chain: mine,
        root: mine,
    };
    let pki = Pki::new();
    // The registration's certificate, then the one the agent renewed it with.
    let registered = pki.input(&pki.leaf_from(HARDWARE_ID, &gateway.spki, Some(2025)));
    let renewed = pki.input(&pki.leaf_from(HARDWARE_ID, &gateway.spki, Some(2026)));
    gateway
        .install(&gateway.options(), &owners, renewed.as_bytes())
        .installed();
    let before = gateway.files();

    // Resending the registration answers with its original certificate: the renewed one is
    // kept (reported as such, never "remove it"), the root is already right, nothing is written,
    // and the run succeeds: the gateway has what it needs.
    let ran = gateway.install(&gateway.options(), &owners, registered.as_bytes());
    let stdout = ran.installed().to_owned();
    assert_eq!(
        stdout,
        format!(
            "{}: renewed\n{}: unchanged\n",
            gateway.chain_path().display(),
            gateway.root_path().display()
        )
    );
    assert!(
        ran.stderr
            .contains("holds a newer certificate for this TPM key")
            && ran.stderr.contains("it is kept")
            && !ran.stderr.contains("remove that file"),
        "{}",
        ran.stderr
    );
    assert_eq!(gateway.files(), before);

    // The root is still installed as usual next to a kept renewed certificate.
    std::fs::remove_file(gateway.root_path()).unwrap();
    let stdout = gateway
        .install(&gateway.options(), &owners, registered.as_bytes())
        .installed()
        .to_owned();
    assert_eq!(
        stdout,
        format!(
            "{}: renewed\n{}: written\n",
            gateway.chain_path().display(),
            gateway.root_path().display()
        )
    );
    let entry = |files: &[FileEntry], path: PathBuf| {
        files
            .iter()
            .find(|file| file.0 == path.display().to_string())
            .cloned()
            .unwrap_or_else(|| panic!("{} is there", path.display()))
    };
    let after = gateway.files();
    assert_eq!(
        entry(&after, gateway.chain_path()),
        entry(&before, gateway.chain_path()),
        "the renewed certificate's file is untouched (same inode and contents)"
    );
    assert_eq!(
        entry(&after, gateway.root_path()).5,
        entry(&before, gateway.root_path()).5,
        "the same root again"
    );
    assert_eq!(gateway.setup().gateway.as_str(), HARDWARE_ID);

    // The same renewed certificate is unchanged; a later one is written.
    let stdout = gateway
        .install(&gateway.options(), &owners, renewed.as_bytes())
        .installed()
        .to_owned();
    assert_eq!(stdout.matches(": unchanged\n").count(), 2, "{stdout}");
    let later = pki.input(&pki.leaf_from(HARDWARE_ID, &gateway.spki, Some(2027)));
    let stdout = gateway
        .install(&gateway.options(), &owners, later.as_bytes())
        .installed()
        .to_owned();
    assert!(stdout.contains(": written\n"), "{stdout}");

    // A newer certificate of another key (not this TPM's) does not hold the file: the
    // registration's certificate replaces it.
    let other_key = pki.input(&pki.leaf_from(HARDWARE_ID, &software_spki(), Some(2030)));
    std::fs::write(
        gateway.chain_path(),
        other_key
            .split("-----BEGIN CERTIFICATE-----")
            .nth(1)
            .map_or_else(
                || panic!("a certificate"),
                |block| format!("-----BEGIN CERTIFICATE-----{block}"),
            ),
    )
    .unwrap();
    gateway
        .install(&gateway.options(), &owners, registered.as_bytes())
        .installed();
    assert_eq!(gateway.setup().gateway.as_str(), HARDWARE_ID);

    // A newer certificate of this key under another root does not hold the file either: after
    // the root changed, the registration under the new root replaces both files.
    gateway
        .install(&gateway.options(), &owners, later.as_bytes())
        .installed();
    let rotated = Pki::new();
    let stdout = gateway
        .install(
            &gateway.options(),
            &owners,
            rotated
                .input(&rotated.leaf_from(HARDWARE_ID, &gateway.spki, Some(2025)))
                .as_bytes(),
        )
        .installed()
        .to_owned();
    assert_eq!(stdout.matches(": written\n").count(), 2, "{stdout}");
    assert_eq!(gateway.setup().gateway.as_str(), HARDWARE_ID);
}

#[test]
fn owners_are_set_per_file_and_an_unusable_configuration_is_refused_before_reading() {
    let Some(gateway) = Gateway::start() else {
        return;
    };
    let mine = me(&gateway.dir);
    let pki = Pki::new();
    let input = pki.input(&pki.leaf(HARDWARE_ID, &gateway.spki));
    let root_owned = Owners {
        chain: mine,
        root: Owner { uid: 0, gid: 0 },
    };
    let ran = gateway.install(&gateway.options(), &root_owned, input.as_bytes());
    if mine.uid == 0 {
        // As root: the root's file belongs to root, the chain to the chain's owner.
        ran.installed();
        let root = std::fs::metadata(gateway.root_path()).unwrap();
        assert_eq!((root.uid(), root.gid()), (0, 0));
    } else {
        // The configuration belongs to this user, not to root: it is not read, nothing is
        // written. (The clean-up when an owner cannot be set is a unit test.)
        ran.failed(Exit::Configuration, "must belong to root (uid 0)");
        assert!(gateway.files().is_empty(), "{:?}", gateway.files());
    }

    let outer = gateway.root.join("outer-ca.pem");
    let missing = gateway.root.join("missing");
    // Paths that differ as text but name one file: through a symbolic link to a directory, and
    // through `..`.
    let link = gateway.root.join("link-to-etc");
    std::os::unix::fs::symlink(&gateway.etc, &link).unwrap();
    let aliased = |certificate: PathBuf, crl: &str| {
        format!(
            "[cloud]\nurl = \"wss://gw.trustiot.example/mqtt\"\ntrust = [\"{}\"]\n\n\
             [broker]\nca = \"{}\"\n{crl}\n\n\
             [identity]\ncertificate = \"{}\"\nkey = {{ source = \"tpm\" }}\n",
            outer.display(),
            gateway.root_path().display(),
            certificate.display(),
        )
    };
    let crl_on_chain = format!("crl = \"{}\"", gateway.chain_path().display());
    for (name, text, expected) in [
        (
            "linked.toml",
            aliased(link.join("broker-ca.pem"), ""),
            "is a symbolic link",
        ),
        (
            "dotdot.toml",
            aliased(
                gateway.state.join("..").join("etc").join("broker-ca.pem"),
                "",
            ),
            "\"..\"",
        ),
        (
            "same.toml",
            aliased(gateway.root_path(), ""),
            "identity.certificate and broker.ca are the same file",
        ),
        (
            "crl-on-chain.toml",
            aliased(gateway.chain_path(), &crl_on_chain),
            "broker.crl and identity.certificate are the same file",
        ),
        (
            "file-key.toml",
            config_text(
                &outer,
                &gateway.etc,
                &gateway.state,
                "key = { source = \"file\", path = \"/nonexistent/key.pem\" }",
                "",
            ),
            "not \"tpm\"",
        ),
        (
            "no-state.toml",
            config_text(
                &outer,
                &gateway.etc,
                &missing,
                "key = { source = \"tpm\" }",
                "",
            ),
            "does not exist",
        ),
        (
            "no-etc.toml",
            config_text(
                &outer,
                &missing,
                &gateway.state,
                "key = { source = \"tpm\" }",
                "",
            ),
            "does not exist",
        ),
    ] {
        let config = gateway.write(name, text);
        let before = gateway.files();
        let mut stdin = Unread;
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let exit = run(
            &gateway.options_with(&config),
            &Owners {
                chain: mine,
                root: mine,
            },
            &mut |_| panic!("no TPM connection may be opened"),
            &mut stdin,
            &mut stdout,
            &mut stderr,
        );
        let stderr = String::from_utf8(stderr).unwrap();
        assert_eq!(exit, Exit::Configuration, "{name}: {stderr}");
        assert!(stdout.is_empty());
        assert!(stderr.contains(expected), "{name}: {stderr}");
        assert_eq!(gateway.files(), before, "{name}");
    }
    let missing_config = gateway.root.join("none.toml");
    let ran = gateway.install(
        &gateway.options_with(&missing_config),
        &Owners {
            chain: mine,
            root: mine,
        },
        b"",
    );
    ran.failed(Exit::Configuration, "none.toml");
}

/// The DER of the one PEM block of `text`.
fn der_of(text: &str) -> Vec<u8> {
    let body: String = text
        .lines()
        .filter(|line| !line.starts_with("-----"))
        .collect();
    base64::engine::general_purpose::STANDARD
        .decode(body.as_bytes())
        .unwrap()
}

/// `der` as one PEM `X509 CRL` block in lines of 64: what the agent writes.
fn crl_pem(der: &[u8]) -> String {
    pem(der)
        .replace("BEGIN CERTIFICATE", "BEGIN X509 CRL")
        .replace("END CERTIFICATE", "END X509 CRL")
}

#[test]
fn a_root_crl_sent_is_installed_only_forward_and_all_or_nothing() {
    let Some(gateway) = Gateway::start() else {
        return;
    };
    let mine = me(&gateway.dir);
    let owners = Owners {
        chain: mine,
        root: mine,
    };
    // broker.crl in the agent's state directory, as in agent.example.toml.
    let crl_path = gateway.state.join("root-crl.pem");
    let outer = gateway.root.join("outer-ca.pem");
    let config = gateway.write(
        "agent-crl.toml",
        config_text(
            &outer,
            &gateway.etc,
            &gateway.state,
            "key = { source = \"tpm\" }",
            &format!("crl = \"{}\"", crl_path.display()),
        ),
    );
    let options = gateway.options_with(&config);
    let pki = Pki::new();
    let leaf = pki.leaf(HARDWARE_ID, &gateway.spki);
    let with = |crl: &str| format!("{}{crl}", pki.input(&leaf));
    let report = |crl_outcome: &str, root_outcome: &str, chain_outcome: &str| {
        format!(
            "{}: {chain_outcome}\n{}: {root_outcome}\n{}: {crl_outcome}\n",
            gateway.chain_path().display(),
            gateway.root_path().display(),
            crl_path.display()
        )
    };

    // Number 2, revoking another issuing CA of this root: three files written.
    let (other_issuing, other_issuer) = pki.another_issuing();
    let crl_2 = pki.crl_numbered(2, Some(&other_issuing));
    let ran = gateway.install(&options, &owners, with(&crl_2).as_bytes());
    assert_eq!(
        ran.installed(),
        report("written", "written", "written"),
        "{}",
        ran.stderr
    );
    // Canonical PEM of the CRL sent, owned like its directory, mode 0644.
    assert_eq!(
        std::fs::read_to_string(&crl_path).unwrap(),
        crl_pem(&der_of(&crl_2))
    );
    let metadata = std::fs::symlink_metadata(&crl_path).unwrap();
    assert!(metadata.is_file());
    assert_eq!(metadata.mode() & 0o7777, FILE_MODE);
    assert_eq!(
        metadata.uid(),
        std::fs::metadata(&gateway.state).unwrap().uid()
    );
    // The agent starts with it and checks the issuing CAs of a chain against it: its own chain
    // passes, a chain through the issuing CA it revokes does not.
    let config_loaded = Config::load(&config).unwrap();
    let setup = Setup::new_with(&config_loaded, &mut gateway.open_tpm())
        .unwrap_or_else(|error| panic!("the agent refuses the installed files: {error}"));
    let RootCrl::Loaded { number, .. } = &setup.root_crl else {
        panic!("the root CRL is not loaded: {:?}", setup.root_crl);
    };
    assert_eq!(number.as_deref(), Some("2"));
    let trust = BrokerTrust::load(&gateway.root_path()).unwrap();
    let validity = |chain: &[Vec<u8>]| {
        let chain: Vec<_> = chain
            .iter()
            .map(|der| rustls_pki_types::CertificateDer::from(der.clone()))
            .collect();
        let (_, leaf) = X509Certificate::from_der(&chain[0]).unwrap();
        let millis = |time: x509_parser::time::ASN1Time| {
            protocol::UnixMillis::new(u64::try_from(time.timestamp()).unwrap() * 1000)
        };
        check_chain_under(
            &trust,
            &setup.root_crl,
            &chain,
            (
                millis(leaf.validity().not_before),
                millis(leaf.validity().not_after),
            ),
            rustls_pki_types::UnixTime::now(),
        )
    };
    validity(&[leaf.clone(), pki.issuing.clone()]).unwrap();
    let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
    params.distinguished_name = rcgen::DistinguishedName::new();
    params
        .distinguished_name
        .push(DnType::CommonName, HARDWARE_ID);
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    let revoked_leaf = params
        .signed_by(&PublicPoint(gateway.spki[26..].to_vec()), &other_issuer)
        .unwrap()
        .der()
        .to_vec();
    let refused = validity(&[revoked_leaf, other_issuing.clone()]).unwrap_err();
    assert!(
        refused.to_string().to_lowercase().contains("revoked"),
        "{refused}"
    );
    drop(setup);
    let installed = gateway.files();
    // The chain, the root, the root CRL and the marker of the highest root CRL number accepted
    // (Laysi/trustiot-v2#563), owned and with the mode of the root CRL; no staged file is left.
    assert_eq!(installed.len(), 4, "{installed:?}");
    assert_eq!(marker_number(&crl_path), "2");
    let marker = std::fs::symlink_metadata(marker_path(&crl_path)).unwrap();
    assert_eq!(
        (marker.mode() & 0o7777, marker.uid()),
        (FILE_MODE, metadata.uid())
    );

    // The same again: unchanged, same inodes.
    let ran = gateway.install(&options, &owners, with(&crl_2).as_bytes());
    assert_eq!(
        ran.installed(),
        report("unchanged", "unchanged", "unchanged")
    );
    assert_eq!(gateway.files(), installed);

    // A lower number (an older answer, or a repeated registration while the gateway already
    // fetched a newer one): the higher one is kept.
    let ran = gateway.install(
        &options,
        &owners,
        with(&pki.crl_numbered(1, None)).as_bytes(),
    );
    assert_eq!(ran.installed(), report("newer", "unchanged", "unchanged"));
    assert!(
        ran.stderr
            .contains("holds root CRL number 2, newer than the one sent (number 1)"),
        "{}",
        ran.stderr
    );
    assert_eq!(gateway.files(), installed);

    // Refused, with nothing written: the same number with other content, another root's CRL,
    // a tampered one, one revoking the gateway's issuing CA, misplaced or repeated CRL blocks.
    let mut tampered = der_of(&pki.crl_numbered(3, None));
    let last = tampered.len() - 1;
    tampered[last] ^= 0x01;
    let other = Pki::new();
    let crl_3 = pki.crl_numbered(3, None);
    let refusals: Vec<(String, &str)> = vec![
        (
            with(&pki.crl_numbered(2, None)),
            "the same number 2 as the one sent but other content",
        ),
        (with(&other.crl_numbered(5, None)), "the root CRL sent"),
        (with(&crl_pem(&tampered)), "the root CRL sent"),
        (
            with(&pki.crl_numbered(3, Some(&pki.issuing))),
            "does not pass as a client certificate",
        ),
        (format!("{crl_3}{}", pki.input(&leaf)), "after the root CRL"),
        (format!("{}{crl_3}", with(&crl_3)), "after the root CRL"),
        (
            format!("{}{crl_3}{}", pem(&leaf), pem(&pki.issuing)),
            "after the root CRL",
        ),
    ];
    for (index, (input, expected)) in refusals.iter().enumerate() {
        let ran = gateway.install(&options, &owners, input.as_bytes());
        ran.failed(Exit::Refused, expected);
        assert!(
            ran.stderr.contains("nothing was written"),
            "{index}: {}",
            ran.stderr
        );
        assert_eq!(gateway.files(), installed, "{index}: {expected}");
    }

    // A higher number replaces it.
    let ran = gateway.install(&options, &owners, with(&crl_3).as_bytes());
    assert_eq!(ran.installed(), report("written", "unchanged", "unchanged"));
    assert_eq!(
        std::fs::read_to_string(&crl_path).unwrap(),
        crl_pem(&der_of(&crl_3))
    );
    assert_eq!(marker_number(&crl_path), "3");

    // broker.crl deleted (or an older one put back by hand): an install sending an older root
    // CRL writes it, but never lowers the marker, so the agent withholds every connection until
    // it fetches a root CRL at least as new as number 3; sending 3 again ends that.
    std::fs::remove_file(&crl_path).unwrap();
    let ran = gateway.install(&options, &owners, with(&crl_2).as_bytes());
    assert_eq!(ran.installed(), report("written", "unchanged", "unchanged"));
    assert_eq!(marker_number(&crl_path), "3");
    let setup = Setup::new_with(&Config::load(&config).unwrap(), &mut gateway.open_tpm()).unwrap();
    assert!(
        matches!(&setup.root_crl, RootCrl::Withheld { number: Some(number), .. } if number == "3"),
        "{:?}",
        setup.root_crl
    );
    drop(setup);
    let ran = gateway.install(&options, &owners, with(&crl_3).as_bytes());
    assert_eq!(ran.installed(), report("written", "unchanged", "unchanged"));

    // Without a root CRL on standard input (an older workstation, or a platform that has none
    // yet): two lines, the installed root CRL is only read.
    let ran = gateway.install(&options, &owners, pki.input(&leaf).as_bytes());
    assert_eq!(
        ran.installed(),
        format!(
            "{}: unchanged\n{}: unchanged\n",
            gateway.chain_path().display(),
            gateway.root_path().display()
        )
    );

    // A new root with its own root CRL number 1: the old root's number 3 goes with the old root.
    let new = Pki::new();
    let new_leaf = new.leaf(HARDWARE_ID, &gateway.spki);
    let new_crl = new.crl_numbered(1, None);
    let ran = gateway.install(
        &options,
        &owners,
        format!("{}{new_crl}", new.input(&new_leaf)).as_bytes(),
    );
    assert_eq!(ran.installed(), report("written", "written", "written"));
    assert_eq!(
        std::fs::read_to_string(&crl_path).unwrap(),
        crl_pem(&der_of(&new_crl))
    );
    let config_loaded = Config::load(&config).unwrap();
    let setup = Setup::new_with(&config_loaded, &mut gateway.open_tpm()).unwrap();
    assert!(
        matches!(&setup.root_crl, RootCrl::Loaded { number, .. } if number.as_deref() == Some("1")),
        "{:?}",
        setup.root_crl
    );
    drop(setup);
    // The marker goes with the old root too: the new root's number 1.
    assert_eq!(marker_number(&crl_path), "1");

    // A root CRL sent while broker.crl is not configured, or its directory does not exist: never
    // dropped, status 9, nothing written.
    let before = gateway.files();
    let missing = gateway.root.join("missing");
    for (name, crl_line) in [
        ("no-crl.toml", String::new()),
        (
            "crl-dir-missing.toml",
            format!("crl = \"{}\"", missing.join("root-crl.pem").display()),
        ),
    ] {
        let config = gateway.write(
            name,
            config_text(
                &outer,
                &gateway.etc,
                &gateway.state,
                "key = { source = \"tpm\" }",
                &crl_line,
            ),
        );
        let ran = gateway.install(
            &gateway.options_with(&config),
            &owners,
            format!("{}{new_crl}", new.input(&new_leaf)).as_bytes(),
        );
        ran.failed(Exit::Configuration, "cannot be installed");
        assert!(
            ran.stderr.contains("nothing was written"),
            "{name}: {}",
            ran.stderr
        );
        assert_eq!(gateway.files(), before, "{name}");
    }
}

/// Standard input the run must not read.
struct Unread;

impl io::Read for Unread {
    fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
        panic!("standard input was read")
    }
}

/// How long the test waits for the install to wait on the directory lock: generous, because the
/// TPM checks before it run against a container on a loaded machine.
const LOCK_WAIT_BOUND: std::time::Duration = std::time::Duration::from_secs(300);

/// Whether a process waits for a `flock` on the file with inode `inode`.
fn flock_waiter_on(inode: u64) -> bool {
    flock_waiters_on(inode) > 0
}

/// How many wait for a `flock` on the file with inode `inode` (`/proc/locks` lists a waiter as
/// `N: -> FLOCK ... <major>:<minor>:<inode> ...`).
fn flock_waiters_on(inode: u64) -> usize {
    std::fs::read_to_string("/proc/locks")
        .unwrap_or_default()
        .lines()
        .filter(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            fields.get(1) == Some(&"->")
                && fields.get(2) == Some(&"FLOCK")
                && fields
                    .get(6)
                    .and_then(|id| id.rsplit(':').next())
                    .is_some_and(|number| number == inode.to_string())
        })
        .count()
}

/// The number in the marker of the highest root CRL number accepted, next to `crl`.
fn marker_number(crl: &Path) -> String {
    let bytes = std::fs::read(marker_path(crl)).unwrap();
    match Marker::parse(&bytes) {
        Marker::At { number, .. } => number,
        other => panic!("no usable marker: {other:?}"),
    }
}

/// A root CRL source for the agent's updater that always answers `0`.
struct Served(String);

impl Source for Served {
    async fn fetch(&self) -> io::Result<Fetched> {
        Ok(Fetched::Crl(self.0.clone().into_bytes()))
    }

    fn describe(&self) -> String {
        "the stand-in".to_owned()
    }
}

#[test]
fn the_agent_s_root_crl_update_and_an_install_never_roll_the_root_crl_back() {
    // Laysi/trustiot-v2#563: the agent writes broker.crl under the same directory lock as
    // `provision install` and by the same forward-only rule. Both wait here for the lock while
    // the install sends root CRL 2 and the agent fetched 3; whichever goes first, the other
    // sees its file, and 3 is what remains, with the marker at 3.
    let Some(gateway) = Gateway::start() else {
        return;
    };
    let mine = me(&gateway.dir);
    let owners = Owners {
        chain: mine,
        root: mine,
    };
    let crl_path = gateway.state.join("root-crl.pem");
    let outer = gateway.root.join("outer-ca.pem");
    let config = gateway.write(
        "agent-crl.toml",
        config_text(
            &outer,
            &gateway.etc,
            &gateway.state,
            "key = { source = \"tpm\" }",
            &format!("crl = \"{}\"", crl_path.display()),
        ),
    );
    let options = gateway.options_with(&config);
    let pki = Pki::new();
    let leaf = pki.leaf(HARDWARE_ID, &gateway.spki);
    let with = |crl: &str| format!("{}{crl}", pki.input(&leaf));
    gateway
        .install(
            &options,
            &owners,
            with(&pki.crl_numbered(1, None)).as_bytes(),
        )
        .installed();
    let served = pki.crl_numbered(3, None);
    let setup = Setup::new_with(&Config::load(&config).unwrap(), &mut gateway.open_tpm()).unwrap();
    let updater = setup
        .root_crl_updater
        .unwrap()
        .with_source(Served(served.clone()));

    let state = gateway.state.clone();
    let (held_tx, held_rx) = std::sync::mpsc::channel();
    let holder = std::thread::spawn(move || {
        let directory = std::fs::File::open(&state).unwrap();
        directory.lock().unwrap();
        held_tx.send(()).unwrap();
        let inode = std::fs::metadata(&state).unwrap().ino();
        let started = std::time::Instant::now();
        while flock_waiters_on(inode) < 2 {
            assert!(
                started.elapsed() < LOCK_WAIT_BOUND,
                "the install and the agent never both waited for the lock of the state directory"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        drop(directory);
    });
    held_rx.recv().unwrap();
    let agent = std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(updater.update())
    });
    let ran = gateway.install(
        &options,
        &owners,
        with(&pki.crl_numbered(2, None)).as_bytes(),
    );
    holder.join().unwrap();
    let updated = agent.join().unwrap();
    // The install wrote 2 before the agent took 3, or found 3 and kept it.
    let installed = ran.installed();
    assert!(
        installed.ends_with(&format!("{}: newer\n", crl_path.display()))
            || installed.ends_with(&format!("{}: written\n", crl_path.display())),
        "{installed}"
    );
    assert_eq!(
        updated,
        Ok(Update::Adopted {
            number: Some("3".to_owned())
        })
    );
    assert_eq!(
        std::fs::read_to_string(&crl_path).unwrap(),
        crl_pem(&der_of(&served))
    );
    assert_eq!(marker_number(&crl_path), "3");
    let setup = Setup::new_with(&Config::load(&config).unwrap(), &mut gateway.open_tpm()).unwrap();
    assert!(
        matches!(&setup.root_crl, RootCrl::Loaded { number, .. } if number.as_deref() == Some("3")),
        "{:?}",
        setup.root_crl
    );
}

#[test]
fn an_install_plans_the_root_crl_only_under_the_lock_so_two_never_roll_it_back() {
    // Laysi/trustiot-v2#832: an install reads broker.crl to keep a newer one; were it read before
    // the lock, two installs could both read the same old file and the lower number could be
    // renamed in last. Here another holder of the lock writes a newer root CRL while this install
    // waits for it: the install must see it and keep it.
    let Some(gateway) = Gateway::start() else {
        return;
    };
    let mine = me(&gateway.dir);
    let owners = Owners {
        chain: mine,
        root: mine,
    };
    let crl_path = gateway.state.join("root-crl.pem");
    let outer = gateway.root.join("outer-ca.pem");
    let config = gateway.write(
        "agent-crl.toml",
        config_text(
            &outer,
            &gateway.etc,
            &gateway.state,
            "key = { source = \"tpm\" }",
            &format!("crl = \"{}\"", crl_path.display()),
        ),
    );
    let options = gateway.options_with(&config);
    let pki = Pki::new();
    let leaf = pki.leaf(HARDWARE_ID, &gateway.spki);
    let with = |crl: &str| format!("{}{crl}", pki.input(&leaf));
    gateway
        .install(
            &options,
            &owners,
            with(&pki.crl_numbered(1, None)).as_bytes(),
        )
        .installed();

    // The lock of the state directory (identity.certificate's and broker.crl's) is held by
    // another process until this install waits for it; then root CRL 3 is put in place and the
    // lock released.
    let crl_3 = crl_pem(&der_of(&pki.crl_numbered(3, None)));
    let state = gateway.state.clone();
    let target = crl_path.clone();
    let expected = crl_3.clone();
    let (held_tx, held_rx) = std::sync::mpsc::channel();
    let holder = std::thread::spawn(move || {
        let directory = std::fs::File::open(&state).unwrap();
        directory.lock().unwrap();
        held_tx.send(()).unwrap();
        let inode = std::fs::metadata(&state).unwrap().ino();
        let started = std::time::Instant::now();
        while !flock_waiter_on(inode) {
            assert!(
                started.elapsed() < LOCK_WAIT_BOUND,
                "the install never waited for the lock of the state directory"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let next = state.join(".root-crl-3");
        std::fs::write(&next, &expected).unwrap();
        std::fs::set_permissions(&next, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::rename(&next, &target).unwrap();
        drop(directory);
    });
    held_rx.recv().unwrap();
    let ran = gateway.install(
        &options,
        &owners,
        with(&pki.crl_numbered(2, None)).as_bytes(),
    );
    holder.join().unwrap();
    assert!(
        ran.installed()
            .ends_with(&format!("{}: newer\n", crl_path.display())),
        "{}",
        ran.stdout
    );
    assert_eq!(
        std::fs::read_to_string(&crl_path).unwrap(),
        crl_3,
        "root CRL 3, written while the install waited, is kept"
    );
}
