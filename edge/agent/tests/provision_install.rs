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
//!   status 9 before anything is read.
//!
//! The executable itself (arguments, help, no installation) is tested in `tests/binary.rs`.
//! Without podman or docker the tests skip, unless `CI` is set, where they fail.

#![allow(
    clippy::unwrap_used,
    reason = "test helpers unwrap like the tests themselves (clippy.toml allows it in tests)"
)]

use std::io;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};

use agent::config::Config;
use agent::provision::install::{Exit, FILE_MODE, MAX_INPUT_BYTES, Options, Owner, Owners, run};
use agent::setup::Setup;
use agent::tls::Validity;
use agent::tpm::{DEFAULT_KEY_HANDLE, TpmChannel, TpmTransport, ensure_key};
use base64::Engine as _;
use protocol::GatewayId;
use rcgen::{
    BasicConstraints, CertificateParams, CertificateRevocationListParams, DnType,
    ExtendedKeyUsagePurpose, IsCa, Issuer, KeyIdMethod, KeyPair, KeyUsagePurpose,
    PublicKeyData as _, SerialNumber,
};
use testkit::mqtt::ScratchDir;
use testkit::swtpm::{Session, Swtpm};

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
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
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

    /// An empty root CRL signed by this root, PEM.
    fn crl(&self) -> String {
        CertificateRevocationListParams {
            this_update: rcgen::date_time_ymd(2020, 1, 1),
            next_update: rcgen::date_time_ymd(2099, 1, 1),
            crl_number: SerialNumber::from(1_u64),
            issuing_distribution_point: None,
            revoked_certs: Vec::new(),
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
        let etc = dir.path().join("etc");
        let state = dir.path().join("state");
        std::fs::create_dir(&etc).unwrap();
        std::fs::create_dir(&state).unwrap();
        // The outer TLS's trust: any CA certificate; this test never connects.
        let outer = dir.write("outer-ca.pem", pem(&Pki::new().root));
        let config = dir.write(
            "agent.toml",
            config_text(&outer, &etc, &state, "key = { source = \"tpm\" }", ""),
        );
        Some(Self {
            swtpm,
            spki,
            dir,
            config,
            etc,
            state,
        })
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
    fn files(&self) -> Vec<(String, u64, u32, u32, u32, Vec<u8>)> {
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

    // A root CRL from another root: with the new root the agent would not start.
    let crl = gateway.state.join("root-crl.pem");
    std::fs::write(&crl, other.crl()).unwrap();
    let outer = gateway.dir.path().join("outer-ca.pem");
    let with_crl = gateway.dir.write(
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
    // With that root's CRL it installs, and is then unchanged.
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
        // The chain was staged, then the root's owner could not be set: nothing renamed, and
        // no staged file left.
        ran.failed(Exit::Failure, "needs root");
        assert!(
            ran.stderr.contains("nothing was replaced"),
            "{}",
            ran.stderr
        );
        assert!(gateway.files().is_empty(), "{:?}", gateway.files());
    }

    let outer = gateway.dir.path().join("outer-ca.pem");
    let missing = gateway.dir.path().join("missing");
    // Paths that differ as text but name one file: through a symbolic link to a directory, and
    // through `..`.
    let link = gateway.dir.path().join("link-to-etc");
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
            "identity.certificate and broker.ca are the same file",
        ),
        (
            "dotdot.toml",
            aliased(
                gateway.state.join("..").join("etc").join("broker-ca.pem"),
                "",
            ),
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
        let config = gateway.dir.write(name, text);
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
    let missing_config = gateway.dir.path().join("none.toml");
    let ran = gateway.install(&gateway.options_with(&missing_config), &root_owned, b"");
    ran.failed(Exit::Configuration, "none.toml");
}

/// Standard input the run must not read.
struct Unread;

impl io::Read for Unread {
    fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
        panic!("standard input was read")
    }
}
