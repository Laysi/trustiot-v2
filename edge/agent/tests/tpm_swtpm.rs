//! `agent::tpm` against a real TPM 2.0 implementation: swtpm (libtpms) in a throwaway container,
//! provisioned at test time by tpm2-tools with the key template of the 2026-10-12 PoC.
//!
//! - TPM2_ReadPublic gives the same SubjectPublicKeyInfo as tpm2-tools (a second, independent
//!   implementation), and TPM2_Sign signatures verify with it in software;
//! - one real rustls TLS 1.3 mTLS handshake over a loopback socket: the server requires a client
//!   certificate from a test CA, issued for the TPM's public key, and the client's
//!   CertificateVerify is signed by the TPM (exactly one TPM2_Sign); a resumed handshake needs
//!   none; a certificate for another key fails on the server;
//! - keys the agent must not use are refused: RSA, ECC without a fixed scheme, a restricted
//!   key, P-384, a key with a password, an empty handle.
//!
//! The image is built from `tests/swtpm/Containerfile` (Debian's swtpm and tpm2-tools) with an
//! empty build context and kept as `localhost/trustiot-agent-swtpm:<hash of the Containerfile>`,
//! so later runs reuse it. swtpm's raw command port is published on the host's loopback
//! interface only. Without podman or docker the test skips, unless `CI` is set, where it fails.

#![allow(
    clippy::unwrap_used,
    reason = "test helpers unwrap like the tests themselves (clippy.toml allows it in tests)"
)]

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

use agent::tpm::{MAX_RESPONSE_BYTES, TpmChannel, TpmError, TpmKey, TpmTransport};
use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
};
use ring::signature::{ECDSA_P256_SHA256_ASN1, UnparsedPublicKey};
use rustls::client::ResolvesClientCert;
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName};
use rustls::server::WebPkiClientVerifier;
use rustls::sign::CertifiedKey;
use rustls::{
    ClientConfig, ClientConnection, HandshakeKind, RootCertStore, ServerConfig, ServerConnection,
    SignatureScheme, StreamOwned,
};
use testkit::container::{Container, container_runtime, runtime_output, unavailable, unique_name};

const CONTAINERFILE: &str = include_str!("swtpm/Containerfile");
/// swtpm's raw TPM command port inside the container.
const TPM_PORT: &str = "2321/tcp";

/// The key provisioned with the PoC's template (ECC P-256, ECDSA-SHA256, fixedtpm, fixedparent,
/// sensitivedataorigin, userwithauth, sign).
const GOOD_KEY: u32 = 0x8100_0100;
const RSA_KEY: u32 = 0x8100_0101;
const NULL_SCHEME_KEY: u32 = 0x8100_0102;
const RESTRICTED_KEY: u32 = 0x8100_0103;
const PASSWORD_KEY: u32 = 0x8100_0104;
const P384_KEY: u32 = 0x8100_0105;
const EMPTY_HANDLE: u32 = 0x8100_01ff;

/// Runs swtpm in the foreground (PID 1). `not-need-init,startup-clear`: it runs TPM2_Startup
/// itself, as the gateway's kernel does.
const SWTPM_SCRIPT: &str = "mkdir -p /run/swtpm && exec swtpm socket --tpm2 \
    --tpmstate dir=/run/swtpm \
    --server type=tcp,port=2321,bindaddr=0.0.0.0 \
    --ctrl type=tcp,port=2322,bindaddr=127.0.0.1 \
    --flags not-need-init,startup-clear";

/// Provisions the keys with tpm2-tools inside the container, then prints the good key's
/// SubjectPublicKeyInfo (DER, hex) as tpm2-tools computes it. Waits up to 60 s for swtpm.
const PROVISION_SCRIPT: &str = r#"set -eu
export TPM2TOOLS_TCTI=swtpm:host=127.0.0.1,port=2321
cd "$(mktemp -d)"
tries=0
until tpm2_getcap properties-fixed > /dev/null 2>&1; do
    tries=$((tries + 1))
    if [ "$tries" -ge 600 ]; then echo "swtpm did not answer on port 2321 within 60 s" >&2; exit 1; fi
    sleep 0.1
done
# The storage root key, as the gateway has it at 0x81000001.
tpm2_createprimary -Q -C o -g sha256 -G ecc256 -c primary.ctx
tpm2_evictcontrol -Q -C o -c primary.ctx 0x81000001
tpm2_flushcontext -t
key() {
    handle="$1"; shift
    tpm2_create -Q -C 0x81000001 "$@" -u key.pub -r key.priv
    tpm2_load -Q -C 0x81000001 -u key.pub -r key.priv -c key.ctx
    tpm2_evictcontrol -Q -C o -c key.ctx "$handle"
    tpm2_flushcontext -t
}
signing="fixedtpm|fixedparent|sensitivedataorigin|userwithauth|sign"
key 0x81000100 -G ecc256:ecdsa-sha256:null -a "$signing"
key 0x81000101 -G rsa2048:rsassa-sha256:null -a "$signing"
key 0x81000102 -G ecc256:null:null -a "$signing"
key 0x81000103 -G ecc256:ecdsa-sha256:null -a "$signing|restricted"
# A random password the agent does not know; noda keeps the failed try off the lockout counter.
key 0x81000104 -G ecc256:ecdsa-sha256:null -a "$signing|noda" -p "$(od -An -N16 -tx1 /dev/urandom | tr -d ' \n')"
key 0x81000105 -G ecc384:ecdsa-sha384:null -a "$signing"
tpm2_readpublic -Q -c 0x81000100 -f der -o spki.der
printf 'spki=%s\n' "$(od -An -v -tx1 spki.der | tr -d ' \n')"
"#;

/// How long a socket read may wait for swtpm or the TLS peer before the test fails instead of
/// hanging. Nothing here should come close.
const IO_BOUND: Duration = Duration::from_secs(60);

/// TPM2_Sign's command code, counted by [`CountingSocket`].
const TPM_CC_SIGN: u32 = 0x0000_015d;

/// The image, built once per Containerfile content.
fn swtpm_image(runtime: &'static str) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, CONTAINERFILE.as_bytes());
    let hash: String = digest.as_ref()[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let image = format!("localhost/trustiot-agent-swtpm:{hash}");
    let exists = Command::new(runtime)
        .args(["image", "inspect", &image])
        .output()
        .unwrap()
        .status
        .success();
    if exists {
        return image;
    }
    // No file of the repository reaches the builder: the context is an empty directory.
    let context = Path::new(env!("CARGO_TARGET_TMPDIR")).join(unique_name("swtpm-context"));
    std::fs::create_dir_all(&context).unwrap();
    let containerfile = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/swtpm/Containerfile");
    let built = Command::new(runtime)
        .args(["build", "--quiet", "--file"])
        .arg(&containerfile)
        .args(["--tag", &image])
        .arg(&context)
        .output()
        .unwrap();
    let _ = std::fs::remove_dir(&context);
    assert!(
        built.status.success(),
        "building the swtpm image failed: {}",
        String::from_utf8_lossy(&built.stderr)
    );
    image
}

/// A provisioned swtpm: the container, the host address of its command port, and the good key's
/// SubjectPublicKeyInfo according to tpm2-tools.
struct Swtpm {
    container: Container,
    address: SocketAddr,
    tools_spki: Vec<u8>,
}

fn start_swtpm() -> Option<Swtpm> {
    let Some(runtime) = container_runtime() else {
        unavailable("podman or docker");
        return None;
    };
    let image = swtpm_image(runtime);
    let container = Container {
        runtime,
        name: unique_name("swtpm"),
    };
    let _id = runtime_output(
        runtime,
        &[
            "run",
            "--detach",
            "--name",
            &container.name,
            "--publish",
            "127.0.0.1::2321",
            "--memory",
            "256m",
            "--cap-drop",
            "ALL",
            "--security-opt",
            "no-new-privileges",
            "--entrypoint",
            "/bin/sh",
            &image,
            "-c",
            SWTPM_SCRIPT,
        ],
    );
    let output = Command::new(runtime)
        .args(["exec", &container.name, "/bin/sh", "-c", PROVISION_SCRIPT])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "provisioning the swtpm keys failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let spki_hex = stdout
        .lines()
        .find_map(|line| line.strip_prefix("spki="))
        .unwrap_or_else(|| panic!("no SPKI in the provisioning output: {stdout}"));
    let tools_spki = spki_hex
        .as_bytes()
        .chunks(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect();
    let address = container.host_address(TPM_PORT);
    Some(Swtpm {
        container,
        address,
        tools_spki,
    })
}

/// swtpm's command port speaks the TPM 2.0 command stream itself: a command, then a response
/// whose header carries its size. Counts the TPM2_Sign commands it carries.
struct CountingSocket {
    stream: TcpStream,
    signs: Arc<AtomicUsize>,
}

impl TpmChannel for CountingSocket {
    fn transact(&mut self, command: &[u8]) -> io::Result<Vec<u8>> {
        if command.get(6..10) == Some(&TPM_CC_SIGN.to_be_bytes()[..]) {
            self.signs.fetch_add(1, Ordering::SeqCst);
        }
        self.stream.write_all(command)?;
        let mut response = vec![0_u8; 10];
        self.stream.read_exact(&mut response)?;
        let size = u32::from_be_bytes(response[2..6].try_into().unwrap());
        let size = usize::try_from(size).unwrap();
        if !(10..=MAX_RESPONSE_BYTES).contains(&size) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("response size {size}"),
            ));
        }
        response.resize(size, 0);
        self.stream.read_exact(&mut response[10..])?;
        Ok(response)
    }
}

impl Swtpm {
    /// Opens `handle` over a new connection; returns the key and its TPM2_Sign counter.
    fn open(&self, handle: u32) -> (Result<TpmKey, TpmError>, Arc<AtomicUsize>) {
        let stream = TcpStream::connect(self.address).unwrap();
        stream.set_read_timeout(Some(IO_BOUND)).unwrap();
        stream.set_nodelay(true).unwrap();
        let signs = Arc::new(AtomicUsize::new(0));
        let socket = CountingSocket {
            stream,
            signs: Arc::clone(&signs),
        };
        (
            TpmKey::open(TpmTransport::Channel(Box::new(socket)), handle),
            signs,
        )
    }
}

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// A public key for rcgen to put in a certificate: the uncompressed point after the SPKI's
/// 26-byte P-256 prefix.
struct PublicPoint(Vec<u8>);

impl rcgen::PublicKeyData for PublicPoint {
    fn der_bytes(&self) -> &[u8] {
        &self.0
    }

    fn algorithm(&self) -> &'static rcgen::SignatureAlgorithm {
        &rcgen::PKCS_ECDSA_P256_SHA256
    }
}

/// A test CA, the broker's certificate and key, and client certificates on demand.
struct Pki {
    issuer: Issuer<'static, KeyPair>,
    ca: CertificateDer<'static>,
    server_chain: Vec<CertificateDer<'static>>,
    server_key: PrivateKeyDer<'static>,
}

impl Pki {
    fn new() -> Self {
        let ca_key = KeyPair::generate().unwrap();
        let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
        ca_params
            .distinguished_name
            .push(DnType::CommonName, "TrustIoT test CA");
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let ca = ca_params.self_signed(&ca_key).unwrap().der().clone();
        let issuer = Issuer::new(ca_params, ca_key);
        let server_key = KeyPair::generate().unwrap();
        let server = CertificateParams::new(vec!["broker.test".to_owned()])
            .unwrap()
            .signed_by(&server_key, &issuer)
            .unwrap();
        Self {
            issuer,
            ca,
            server_chain: vec![server.der().clone()],
            server_key: PrivatePkcs8KeyDer::from(server_key.serialize_der()).into(),
        }
    }

    /// A client certificate (CN `gw-test-01`, EKU clientAuth) for the key with this SPKI.
    fn client_certificate(&self, spki: &[u8]) -> CertificateDer<'static> {
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params
            .distinguished_name
            .push(DnType::CommonName, "gw-test-01");
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        params
            .signed_by(&PublicPoint(spki[26..].to_vec()), &self.issuer)
            .unwrap()
            .der()
            .clone()
    }

    fn roots(&self) -> Arc<RootCertStore> {
        let mut roots = RootCertStore::empty();
        roots.add(self.ca.clone()).unwrap();
        Arc::new(roots)
    }

    /// TLS 1.3 only, a client certificate from the test CA required.
    fn server_config(&self) -> Arc<ServerConfig> {
        let verifier = WebPkiClientVerifier::builder_with_provider(self.roots(), provider())
            .build()
            .unwrap();
        Arc::new(
            ServerConfig::builder_with_provider(provider())
                .with_protocol_versions(&[&rustls::version::TLS13])
                .unwrap()
                .with_client_cert_verifier(verifier)
                .with_single_cert(self.server_chain.clone(), self.server_key.clone_key())
                .unwrap(),
        )
    }

    fn client_config(&self, key: Arc<CertifiedKey>) -> Arc<ClientConfig> {
        Arc::new(
            ClientConfig::builder_with_provider(provider())
                .with_protocol_versions(&[&rustls::version::TLS13])
                .unwrap()
                .with_root_certificates(self.roots())
                .with_client_cert_resolver(Arc::new(FixedClientCert(key))),
        )
    }
}

/// Always offers the one client certificate.
#[derive(Debug)]
struct FixedClientCert(Arc<CertifiedKey>);

impl ResolvesClientCert for FixedClientCert {
    fn resolve(&self, _: &[&[u8]], _: &[SignatureScheme]) -> Option<Arc<CertifiedKey>> {
        Some(Arc::clone(&self.0))
    }

    fn has_certs(&self) -> bool {
        true
    }
}

/// What one connection showed: how the client's handshake went, and the client certificate the
/// server saw (or the server's error).
struct Exchange {
    client: Result<HandshakeKind, String>,
    server: Result<CertificateDer<'static>, String>,
}

/// One TLS connection over a loopback socket: the client sends `ping`, the server answers
/// `pong`.
fn exchange(server_config: Arc<ServerConfig>, client_config: Arc<ClientConfig>) -> Exchange {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || -> Result<CertificateDer<'static>, String> {
        let (socket, _) = listener.accept().map_err(|e| e.to_string())?;
        socket.set_read_timeout(Some(IO_BOUND)).unwrap();
        let connection = ServerConnection::new(server_config).map_err(|e| e.to_string())?;
        let mut tls = StreamOwned::new(connection, socket);
        let mut ping = [0_u8; 4];
        tls.read_exact(&mut ping).map_err(|e| e.to_string())?;
        assert_eq!(&ping, b"ping");
        tls.write_all(b"pong").map_err(|e| e.to_string())?;
        tls.flush().map_err(|e| e.to_string())?;
        let peer = tls
            .conn
            .peer_certificates()
            .and_then(|chain| chain.first())
            .ok_or("no client certificate")?
            .clone()
            .into_owned();
        tls.conn.send_close_notify();
        let _ = tls.flush();
        Ok(peer)
    });
    let client = (|| -> Result<HandshakeKind, String> {
        let socket = TcpStream::connect(address).map_err(|e| e.to_string())?;
        socket.set_read_timeout(Some(IO_BOUND)).unwrap();
        socket.set_nodelay(true).unwrap();
        let name = ServerName::try_from("broker.test").unwrap();
        let connection = ClientConnection::new(client_config, name).map_err(|e| e.to_string())?;
        let mut tls = StreamOwned::new(connection, socket);
        tls.write_all(b"ping").map_err(|e| e.to_string())?;
        let mut pong = [0_u8; 4];
        tls.read_exact(&mut pong).map_err(|e| e.to_string())?;
        assert_eq!(&pong, b"pong");
        // Read to close_notify, so the session tickets that came with it are stored.
        let mut rest = Vec::new();
        let _ = tls.read_to_end(&mut rest);
        tls.conn
            .handshake_kind()
            .ok_or_else(|| "the handshake did not finish".to_owned())
    })();
    Exchange {
        client,
        server: server.join().unwrap(),
    }
}

fn verifies(spki: &[u8], message: &[u8], signature: &[u8]) -> bool {
    UnparsedPublicKey::new(&ECDSA_P256_SHA256_ASN1, &spki[26..])
        .verify(message, signature)
        .is_ok()
}

#[test]
fn the_tpm_key_signs_and_completes_an_mtls_handshake_and_unusable_keys_are_refused() {
    let Some(swtpm) = start_swtpm() else {
        return;
    };

    // ReadPublic and the self-test signature at open.
    let (key, signs) = swtpm.open(GOOD_KEY);
    let key = key.unwrap();
    assert_eq!(
        signs.load(Ordering::SeqCst),
        1,
        "open signs one test message"
    );
    assert_eq!(
        key.public_key_spki_der(),
        &swtpm.tools_spki[..],
        "ReadPublic and tpm2-tools disagree on the public key"
    );
    let spki = key.public_key_spki_der().to_vec();

    // Signatures made in the TPM verify in software; ECDSA's random nonce makes each differ.
    let first = key.sign(b"first message").unwrap();
    let again = key.sign(b"first message").unwrap();
    assert!(verifies(&spki, b"first message", &first));
    assert!(verifies(&spki, b"first message", &again));
    assert_ne!(first, again);
    assert!(!verifies(&spki, b"second message", &first));

    // A real TLS 1.3 handshake with the certificate for the TPM key.
    let pki = Pki::new();
    let certificate = pki.client_certificate(&spki);
    let certified = Arc::new(CertifiedKey::new(
        vec![certificate.clone()],
        Arc::new(key.clone()),
    ));
    certified
        .keys_match()
        .expect("the certificate is for the TPM key");
    let client_config = pki.client_config(certified);
    let before = signs.load(Ordering::SeqCst);
    let full = exchange(pki.server_config(), Arc::clone(&client_config));
    assert_eq!(full.client, Ok(HandshakeKind::Full));
    assert_eq!(full.server, Ok(certificate.clone()));
    assert_eq!(
        signs.load(Ordering::SeqCst) - before,
        1,
        "the client's CertificateVerify is one TPM2_Sign"
    );

    // Resumption (kept on, decision 6) skips CertificateVerify, and so the TPM.
    let server_config = pki.server_config();
    let first_on_server = exchange(Arc::clone(&server_config), Arc::clone(&client_config));
    assert_eq!(first_on_server.client, Ok(HandshakeKind::Full));
    let before = signs.load(Ordering::SeqCst);
    let resumed = exchange(server_config, client_config);
    assert_eq!(resumed.client, Ok(HandshakeKind::Resumed));
    assert_eq!(resumed.server, Ok(certificate));
    assert_eq!(
        signs.load(Ordering::SeqCst),
        before,
        "a resumed handshake signs nothing"
    );

    // A certificate for another key with the TPM as signer: the server rejects the signature.
    let other = KeyPair::generate().unwrap();
    let mismatched = Arc::new(CertifiedKey::new(
        vec![pki.client_certificate(&rcgen::PublicKeyData::subject_public_key_info(&other))],
        Arc::new(key.clone()),
    ));
    assert!(mismatched.keys_match().is_err());
    let refused = exchange(pki.server_config(), pki.client_config(mismatched));
    let server_error = refused.server.unwrap_err();
    assert!(
        server_error.contains("BadSignature") || server_error.contains("signature"),
        "the server should reject the CertificateVerify: {server_error}"
    );
    assert!(refused.client.is_err());

    // One connection to swtpm at a time: close this one before the next opens.
    drop(key);

    for (handle, expected) in [
        (RSA_KEY, "not an ECC key"),
        (NULL_SCHEME_KEY, "hash none"),
        (RESTRICTED_KEY, "have [restricted]"),
        (P384_KEY, "with hash 0x000c"),
    ] {
        match swtpm.open(handle).0 {
            Err(TpmError::UnsuitableKey(reason)) => {
                assert!(reason.contains(expected), "{handle:#010x}: {reason}");
            }
            other => panic!("{handle:#010x} should be unsuitable, got {other:?}"),
        }
    }
    // The key is fine but needs a password: the self-test signature fails at open.
    match swtpm.open(PASSWORD_KEY).0 {
        Err(TpmError::Response(code)) => {
            // A noDA key answers a wrong password with TPM_RC_BAD_AUTH, not TPM_RC_AUTH_FAIL.
            assert_eq!(code.to_string(), "TPM_RC_BAD_AUTH (0x000009a2, session 1)");
        }
        other => panic!("the password key should fail authorization, got {other:?}"),
    }
    match swtpm.open(EMPTY_HANDLE).0 {
        Err(TpmError::Response(code)) => {
            assert_eq!(code.to_string(), "TPM_RC_HANDLE (0x0000018b, handle 1)");
        }
        other => panic!("an empty handle should be refused, got {other:?}"),
    }
    drop(swtpm.container);
}
