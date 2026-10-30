//! The gateway's way to the broker, for tests (Laysi/trustiot-v2#86): a test PKI, an in-process
//! WebSocket front that stands in for the ingress and the MQTT relay (mqtt-bridge), and a
//! Mosquitto 2.1 container with the production listener settings (mTLS, the certificate's CN as
//! the user name, dynamic security with the protocol's gateway role).
//!
//! - [`TestPki`]: two CAs made at test time, never written to the repository: a "public" CA for
//!   the front's certificate (the outer TLS) and our broker CA, which signs the broker's and the
//!   gateways' certificates (the inner mTLS). [`Authority::issuing`] adds an issuing CA under a
//!   root and [`Authority::crl`] a CRL, for the inner TLS's chain and root CRL checks.
//! - [`front::WsFront`]: outer TLS, WebSocket on `/mqtt` with subprotocol `mqtt`, and a byte
//!   relay to the broker; it records what each connection negotiated and can be stopped and
//!   started again on the same port.
//! - [`mosquitto::Mosquitto`]: the broker in a throwaway container (podman or docker), with a
//!   cloud account for subscribing and publishing from inside the container.
//!
//! How to use them is in the crate README, section 「MQTT 測試環境」.

use std::hash::BuildHasher;
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use rcgen::{
    BasicConstraints, CertificateParams, CertificateRevocationListParams, DistinguishedName,
    DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyIdMethod, KeyPair, KeyUsagePurpose,
    RevokedCertParams, SerialNumber,
};
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, PrivateKeyDer};

pub mod front;
pub mod mosquitto;

/// The outer TLS host name the front's certificate is for; it resolves to the loopback
/// interface.
pub const FRONT_HOST: &str = "localhost";
/// The broker's name in its certificate; the inner TLS checks it, nothing resolves it. The
/// agent's fixed name (`agent::tls::BROKER_NAME`, Laysi/trustiot-v2#557).
pub const BROKER_NAME: &str = "mqtt-broker.trustiot.internal";

/// A CA made for one test: a root ([`Authority::new`]) or an issuing CA under one
/// ([`Authority::issuing`]).
pub struct Authority {
    /// The CA certificate in PEM.
    pub pem: String,
    serial: u64,
    issuer: Issuer<'static, KeyPair>,
}

impl std::fmt::Debug for Authority {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Authority").finish_non_exhaustive()
    }
}

/// A certificate and its private key, in PEM.
#[derive(Clone)]
pub struct Leaf {
    /// The certificate.
    pub cert_pem: String,
    /// The private key (PKCS#8).
    pub key_pem: String,
}

impl std::fmt::Debug for Leaf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Leaf").finish_non_exhaustive()
    }
}

impl Authority {
    /// A self-signed CA named `name` with an ECDSA P-256 key.
    ///
    /// # Panics
    ///
    /// When key or certificate generation fails.
    #[must_use]
    pub fn new(name: &str) -> Self {
        let key = KeyPair::generate().expect("CA key");
        let (params, serial) = Self::ca_params(name, BasicConstraints::Unconstrained);
        let pem = params.self_signed(&key).expect("CA certificate").pem();
        Self {
            pem,
            serial,
            issuer: Issuer::new(params, key),
        }
    }

    /// An issuing CA named `name` under this CA (path length 0, keyCertSign and cRLSign), as the
    /// root CA signs the issuing CA that signs the broker's and the gateways' certificates.
    ///
    /// # Panics
    ///
    /// When key or certificate generation fails.
    #[must_use]
    pub fn issuing(&self, name: &str) -> Self {
        let key = KeyPair::generate().expect("issuing CA key");
        let (params, serial) = Self::ca_params(name, BasicConstraints::Constrained(0));
        let pem = params
            .signed_by(&key, &self.issuer)
            .expect("issuing CA certificate")
            .pem();
        Self {
            pem,
            serial,
            issuer: Issuer::new(params, key),
        }
    }

    fn ca_params(name: &str, constraints: BasicConstraints) -> (CertificateParams, u64) {
        let mut params = CertificateParams::new(Vec::<String>::new()).expect("CA params");
        params.is_ca = IsCa::Ca(constraints);
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        params.distinguished_name = DistinguishedName::new();
        params.distinguished_name.push(DnType::CommonName, name);
        // A known serial, so that a CRL can list this CA. Random per CA, like a real CA's.
        let serial = std::hash::RandomState::new().hash_one(name) >> 1;
        params.serial_number = Some(SerialNumber::from(serial));
        (params, serial)
    }

    /// A certificate for `names` (DNS names or IP addresses) with exactly the extended key
    /// usages `usages` (none: no EKU extension), for checking which certificates a TLS client
    /// accepts as a server's.
    ///
    /// # Panics
    ///
    /// When key or certificate generation fails.
    #[must_use]
    pub fn leaf(&self, names: &[&str], usages: &[ExtendedKeyUsagePurpose]) -> Leaf {
        let names: Vec<String> = names.iter().map(|name| (*name).to_owned()).collect();
        let mut params = CertificateParams::new(names).expect("leaf params");
        params.extended_key_usages = usages.to_vec();
        self.sign(params)
    }

    /// A CRL signed by this CA in PEM, with the CRL number `number`, revoking the CAs in
    /// `revoked` (which this CA signed), issued 2020-01-01 and with the nextUpdate
    /// `next_update` (year, month, day; a past date makes an expired CRL).
    ///
    /// # Panics
    ///
    /// When rcgen refuses the parameters, for example a nextUpdate before 2020-01-02.
    #[must_use]
    pub fn crl(&self, number: u64, revoked: &[&Authority], next_update: (i32, u8, u8)) -> String {
        let this_update = rcgen::date_time_ymd(2020, 1, 1);
        let params = CertificateRevocationListParams {
            this_update,
            next_update: rcgen::date_time_ymd(next_update.0, next_update.1, next_update.2),
            crl_number: SerialNumber::from(number),
            issuing_distribution_point: None,
            revoked_certs: revoked
                .iter()
                .map(|authority| RevokedCertParams {
                    serial_number: SerialNumber::from(authority.serial),
                    revocation_time: this_update,
                    reason_code: None,
                    invalidity_date: None,
                })
                .collect(),
            key_identifier_method: KeyIdMethod::Sha256,
        };
        params
            .signed_by(&self.issuer)
            .expect("CRL")
            .pem()
            .expect("CRL PEM")
    }

    /// A server certificate for `names` (DNS names or IP addresses), for TLS servers.
    ///
    /// # Panics
    ///
    /// When key or certificate generation fails.
    #[must_use]
    pub fn server(&self, names: &[&str]) -> Leaf {
        let names: Vec<String> = names.iter().map(|name| (*name).to_owned()).collect();
        let mut params = CertificateParams::new(names).expect("server params");
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        self.sign(params)
    }

    /// A client certificate with the subject CN `common_name` and EKU clientAuth, as the CA
    /// signs gateway certificates.
    ///
    /// # Panics
    ///
    /// When key or certificate generation fails.
    #[must_use]
    pub fn client(&self, common_name: &str) -> Leaf {
        let mut params = CertificateParams::new(Vec::<String>::new()).expect("client params");
        params.distinguished_name = DistinguishedName::new();
        params
            .distinguished_name
            .push(DnType::CommonName, common_name);
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        self.sign(params)
    }

    /// [`Self::client`] valid only from `not_before` to `not_after` (year, month, day), for
    /// example an expired certificate.
    ///
    /// # Panics
    ///
    /// When key or certificate generation fails.
    #[must_use]
    pub fn client_valid(
        &self,
        common_name: &str,
        not_before: (i32, u8, u8),
        not_after: (i32, u8, u8),
    ) -> Leaf {
        let mut params = CertificateParams::new(Vec::<String>::new()).expect("client params");
        params.distinguished_name = DistinguishedName::new();
        params
            .distinguished_name
            .push(DnType::CommonName, common_name);
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        params.not_before = rcgen::date_time_ymd(not_before.0, not_before.1, not_before.2);
        params.not_after = rcgen::date_time_ymd(not_after.0, not_after.1, not_after.2);
        self.sign(params)
    }

    /// A gateway certificate for an existing key, as the certs service signs one at registration
    /// or renewal (services/certs/README.md, 「閘道器憑證」): subject `CN=<common_name>` only,
    /// `cA=FALSE`, keyUsage digitalSignature, EKU clientAuth only, for `public_key`, valid from
    /// `not_before` to `not_after` (to the second). Returns the certificate in PEM; the private
    /// key stays with whoever holds it (the gateway's TPM, or a test's software key).
    ///
    /// # Panics
    ///
    /// When rcgen refuses the parameters, for example a time before 1970.
    #[must_use]
    pub fn client_for_key(
        &self,
        common_name: &str,
        public_key: &impl rcgen::PublicKeyData,
        not_before: std::time::SystemTime,
        not_after: std::time::SystemTime,
    ) -> String {
        let since_epoch = |at: std::time::SystemTime| {
            let since = at
                .duration_since(std::time::UNIX_EPOCH)
                .expect("a time after 1970");
            rcgen::date_time_ymd(1970, 1, 1) + std::time::Duration::from_secs(since.as_secs())
        };
        let mut params = CertificateParams::new(Vec::<String>::new()).expect("client params");
        params.distinguished_name = DistinguishedName::new();
        params
            .distinguished_name
            .push(DnType::CommonName, common_name);
        params.is_ca = IsCa::ExplicitNoCa;
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        params.not_before = since_epoch(not_before);
        params.not_after = since_epoch(not_after);
        params
            .signed_by(public_key, &self.issuer)
            .expect("client certificate")
            .pem()
    }

    fn sign(&self, params: CertificateParams) -> Leaf {
        let key = KeyPair::generate().expect("leaf key");
        let certificate = params
            .signed_by(&key, &self.issuer)
            .expect("leaf certificate");
        Leaf {
            cert_pem: certificate.pem(),
            key_pem: key.serialize_pem(),
        }
    }
}

impl Leaf {
    /// The certificate in DER.
    ///
    /// # Panics
    ///
    /// When the PEM does not parse (never for a [`Leaf`] made here).
    #[must_use]
    pub fn cert_der(&self) -> CertificateDer<'static> {
        CertificateDer::from_pem_slice(self.cert_pem.as_bytes()).expect("certificate PEM")
    }

    /// The private key in DER.
    ///
    /// # Panics
    ///
    /// When the PEM does not parse (never for a [`Leaf`] made here).
    #[must_use]
    pub fn key_der(&self) -> PrivateKeyDer<'static> {
        PrivateKeyDer::from_pem_slice(self.key_pem.as_bytes()).expect("key PEM")
    }
}

/// The certificates of one test: the outer "public" CA with the front's certificate, and the
/// broker CA with the broker's certificate. Gateway certificates come from
/// [`TestPki::gateway`].
#[derive(Debug)]
pub struct TestPki {
    /// Signs the front's certificate; the agent's outer trust.
    pub public_ca: Authority,
    /// The front's certificate for [`FRONT_HOST`].
    pub front: Leaf,
    /// Our CA: signs the broker's and the gateways' certificates; the agent's inner trust and
    /// the broker's client CA.
    pub broker_ca: Authority,
    /// The broker's certificate for [`BROKER_NAME`].
    pub broker: Leaf,
}

impl TestPki {
    /// Fresh CAs and certificates.
    #[must_use]
    pub fn new() -> Self {
        let public_ca = Authority::new("testkit public CA");
        let front = public_ca.server(&[FRONT_HOST]);
        let broker_ca = Authority::new("testkit broker CA");
        let broker = broker_ca.server(&[BROKER_NAME]);
        Self {
            public_ca,
            front,
            broker_ca,
            broker,
        }
    }

    /// A gateway certificate with the CN `gateway`, signed by the broker CA.
    #[must_use]
    pub fn gateway(&self, gateway: &str) -> Leaf {
        self.broker_ca.client(gateway)
    }

    /// The front's outer TLS server: TLS 1.3, ALPN `http/1.1`, the front certificate. Follows
    /// the client's cipher suite order, as Cloudflare and Envoy do.
    ///
    /// # Panics
    ///
    /// When rustls refuses the certificate.
    #[must_use]
    pub fn front_tls(&self) -> Arc<rustls::ServerConfig> {
        front_tls(&self.front, &[&rustls::version::TLS13])
    }
}

impl Default for TestPki {
    fn default() -> Self {
        Self::new()
    }
}

/// A TLS server for `leaf` with the given protocol versions, ALPN `http/1.1`, following the
/// client's cipher suite order, with session resumption.
///
/// # Panics
///
/// When rustls refuses the settings or the certificate.
#[must_use]
pub fn front_tls(
    leaf: &Leaf,
    versions: &[&'static rustls::SupportedProtocolVersion],
) -> Arc<rustls::ServerConfig> {
    let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(versions)
    .expect("protocol versions")
    .with_no_client_auth()
    .with_single_cert(vec![leaf.cert_der()], leaf.key_der())
    .expect("front certificate");
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    config.ignore_client_order = false;
    Arc::new(config)
}

/// A TLS 1.3 server for `leaf` that requires a client certificate from `client_ca`, like the
/// broker's mTLS listener.
///
/// # Panics
///
/// When rustls refuses the settings or the certificates.
#[must_use]
pub fn mtls_server(leaf: &Leaf, client_ca: &Authority) -> Arc<rustls::ServerConfig> {
    mtls_server_with_versions(leaf, client_ca, &[&rustls::version::TLS13])
}

/// [`mtls_server`] that sends `chain` (the issuing CAs, leaf side first) after the leaf, as the
/// broker sends its issuing CA.
///
/// # Panics
///
/// When rustls refuses the settings or the certificates.
#[must_use]
pub fn mtls_server_with_chain(
    leaf: &Leaf,
    chain: &[&Authority],
    client_ca: &Authority,
) -> Arc<rustls::ServerConfig> {
    let certificates = std::iter::once(leaf.cert_der())
        .chain(chain.iter().map(|authority| {
            CertificateDer::from_pem_slice(authority.pem.as_bytes()).expect("CA PEM")
        }))
        .collect();
    mtls_server_config(certificates, leaf, client_ca, &[&rustls::version::TLS13])
}

/// [`mtls_server`] with the given protocol versions (for example only TLS 1.2, to see a client
/// refuse it).
///
/// # Panics
///
/// When rustls refuses the settings or the certificates.
#[must_use]
pub fn mtls_server_with_versions(
    leaf: &Leaf,
    client_ca: &Authority,
    versions: &[&'static rustls::SupportedProtocolVersion],
) -> Arc<rustls::ServerConfig> {
    mtls_server_config(vec![leaf.cert_der()], leaf, client_ca, versions)
}

fn mtls_server_config(
    certificates: Vec<CertificateDer<'static>>,
    leaf: &Leaf,
    client_ca: &Authority,
    versions: &[&'static rustls::SupportedProtocolVersion],
) -> Arc<rustls::ServerConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(CertificateDer::from_pem_slice(client_ca.pem.as_bytes()).expect("CA PEM"))
        .expect("client CA");
    let verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(
        Arc::new(roots),
        Arc::clone(&provider),
    )
    .build()
    .expect("client verifier");
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(versions)
        .expect("protocol versions")
        .with_client_cert_verifier(verifier)
        .with_single_cert(certificates, leaf.key_der())
        .expect("server certificate");
    Arc::new(config)
}

/// A directory for one test's files (configuration, certificates, keys), removed when dropped.
#[derive(Debug)]
pub struct ScratchDir {
    path: PathBuf,
}

impl ScratchDir {
    /// A new, empty directory under the system's temporary directory.
    ///
    /// # Panics
    ///
    /// When the directory cannot be created.
    #[must_use]
    pub fn new(kind: &str) -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "testkit-{kind}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("scratch directory");
        Self { path }
    }

    /// The directory.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Writes `contents` to `name` in the directory and returns its path.
    ///
    /// # Panics
    ///
    /// When the file cannot be written.
    pub fn write(&self, name: &str, contents: impl AsRef<[u8]>) -> PathBuf {
        let path = self.path.join(name);
        std::fs::write(&path, contents).expect("scratch file");
        path
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Stops `listener` listening for every copy of its descriptor, so that its port refuses
/// connections and can be bound again at once. Dropping a listener closes only this process's
/// copy: a child process that another test is spawning holds copies of every descriptor between
/// fork and exec (close-on-exec closes them only at exec), and while it does, the socket keeps
/// listening: binding the port again fails with `AddrInUse` and connections still succeed.
/// Shutting a listening socket down takes it out of LISTEN on Linux, whoever holds it; elsewhere
/// it fails, and the drop that follows closes the socket as before.
pub(crate) fn stop_listening(listener: &impl AsFd) {
    // Best effort: on an error the socket is left to the drop that follows.
    let _ = socket2::SockRef::from(listener).shutdown(std::net::Shutdown::Both);
}

#[cfg(test)]
mod tests {
    use std::io::ErrorKind;
    use std::net::{SocketAddr, TcpListener, TcpStream};

    use super::stop_listening;

    /// Stops `listener` while a duplicate of its descriptor stays open, as a child process
    /// between fork and exec would hold one, and checks the port is free all the same.
    fn stop_with_a_copy_open(listener: TcpListener, what: &str) {
        let address = listener.local_addr().unwrap();
        let copy = listener.try_clone().unwrap();
        stop_listening(&listener);
        drop(listener);
        let refused = TcpStream::connect(address).map(|_| ()).unwrap_err();
        assert_eq!(refused.kind(), ErrorKind::ConnectionRefused, "{what}");
        TcpListener::bind(address).unwrap_or_else(|error| panic!("{what}: rebind: {error}"));
        drop(copy);
    }

    #[test]
    fn a_stopped_listener_frees_its_port_while_a_copy_of_its_descriptor_is_open() {
        // A port the system picked (`WsFront::start`, the Mosquitto port probe).
        let picked = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address: SocketAddr = picked.local_addr().unwrap();
        stop_with_a_copy_open(picked, "a picked port");
        // The same port bound by number (`WsFront::restart`).
        let named = TcpListener::bind(address).unwrap();
        stop_with_a_copy_open(named, "a port bound by number");
    }
}
