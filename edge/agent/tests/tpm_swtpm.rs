//! `agent::tpm` against a real TPM 2.0 implementation: swtpm (libtpms) in a throwaway container
//! (`testkit::swtpm`), provisioned at test time by tpm2-tools with the key template of the
//! 2026-10-12 PoC.
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
//! Without podman or docker the test skips, unless `CI` is set, where it fails. Provisioning the
//! key with the agent's own code is `tests/tpm_provision.rs`.

#![allow(
    clippy::unwrap_used,
    reason = "test helpers unwrap like the tests themselves (clippy.toml allows it in tests)"
)]

use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

use agent::tpm::{TpmChannel, TpmError, TpmKey, TpmTransport};
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
use testkit::swtpm::{Session, Swtpm};

/// The key provisioned with the PoC's template (ECC P-256, ECDSA-SHA256, fixedtpm, fixedparent,
/// sensitivedataorigin, userwithauth, sign).
const GOOD_KEY: u32 = 0x8100_0100;
const RSA_KEY: u32 = 0x8100_0101;
const NULL_SCHEME_KEY: u32 = 0x8100_0102;
const RESTRICTED_KEY: u32 = 0x8100_0103;
const PASSWORD_KEY: u32 = 0x8100_0104;
const P384_KEY: u32 = 0x8100_0105;
const EMPTY_HANDLE: u32 = 0x8100_01ff;

/// Provisions the keys with tpm2-tools under the SRK (`Swtpm::create_standard_srk`), then prints
/// the good key's SubjectPublicKeyInfo (DER, hex) as tpm2-tools computes it.
const PROVISION_SCRIPT: &str = r#"key() {
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

/// How long a socket read may wait for the TLS peer before the test fails instead of hanging.
/// Nothing here should come close.
const IO_BOUND: Duration = Duration::from_secs(60);

/// TPM2_Sign's command code, counted by [`CountingSession`].
const TPM_CC_SIGN: u32 = 0x0000_015d;

/// A provisioned swtpm and the good key's SubjectPublicKeyInfo according to tpm2-tools.
struct Provisioned {
    swtpm: Swtpm,
    tools_spki: Vec<u8>,
}

fn start_swtpm() -> Option<Provisioned> {
    let swtpm = Swtpm::start()?;
    swtpm.create_standard_srk();
    let stdout = swtpm.tools(PROVISION_SCRIPT);
    let spki_hex = stdout
        .lines()
        .find_map(|line| line.strip_prefix("spki="))
        .unwrap_or_else(|| panic!("no SPKI in the provisioning output: {stdout}"));
    let tools_spki = spki_hex
        .as_bytes()
        .chunks(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect();
    Some(Provisioned { swtpm, tools_spki })
}

/// A swtpm session that counts the TPM2_Sign commands it carries.
struct CountingSession {
    session: Session,
    signs: Arc<AtomicUsize>,
}

impl TpmChannel for CountingSession {
    fn transact(&mut self, command: &[u8]) -> io::Result<Vec<u8>> {
        if command.get(6..10) == Some(&TPM_CC_SIGN.to_be_bytes()[..]) {
            self.signs.fetch_add(1, Ordering::SeqCst);
        }
        self.session.transact(command)
    }
}

impl Provisioned {
    /// Opens `handle` in a new session; returns the key and its TPM2_Sign counter.
    fn open(&self, handle: u32) -> (Result<TpmKey, TpmError>, Arc<AtomicUsize>) {
        let signs = Arc::new(AtomicUsize::new(0));
        let session = CountingSession {
            session: self.swtpm.session(),
            signs: Arc::clone(&signs),
        };
        (
            TpmKey::open(TpmTransport::Channel(Box::new(session)), handle),
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
    drop(swtpm);
}
