//! A gateway PKI made at run time for the MQTT broker's tests (Laysi/trustiot-v2#84): a root and
//! an issuing CA as the certs service has them, gateway certificates (valid, expired, without
//! clientAuth, revoked), a foreign CA, CRLs, and the broker's server certificates. Every key is
//! generated for the test and never leaves its scratch directory.

use std::path::Path;

use rcgen::{
    BasicConstraints, CertificateParams, CertificateRevocationListParams, CertifiedIssuer, DnType,
    ExtendedKeyUsagePurpose, IsCa, KeyIdMethod, KeyPair, KeyUsagePurpose, PKCS_ECDSA_P256_SHA256,
    RevokedCertParams, SerialNumber, date_time_ymd,
};

/// A CA and the PEM of its certificate.
pub struct Ca {
    pub issuer: CertifiedIssuer<'static, KeyPair>,
    pub pem: String,
}

/// A certificate and its key, PEM.
pub struct Leaf {
    pub cert: String,
    pub key: String,
}

fn key() -> KeyPair {
    KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).expect("a P-256 key")
}

fn ca_params(name: &str) -> CertificateParams {
    let mut params = CertificateParams::default();
    params.distinguished_name.push(DnType::CommonName, name);
    params.is_ca = IsCa::Ca(BasicConstraints::Constrained(1));
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    params
}

/// A self-signed CA.
pub fn root(name: &str) -> Ca {
    let issuer = CertifiedIssuer::self_signed(ca_params(name), key()).expect("a root CA");
    let pem = issuer.pem();
    Ca { issuer, pem }
}

/// A CA signed by `parent`.
pub fn intermediate(name: &str, parent: &Ca) -> Ca {
    let issuer =
        CertifiedIssuer::signed_by(ca_params(name), key(), &parent.issuer).expect("an issuing CA");
    let pem = issuer.pem();
    Ca { issuer, pem }
}

/// What kind of gateway certificate to make.
#[derive(Clone, Copy, Debug)]
pub enum Kind {
    /// clientAuth, valid now.
    Valid,
    /// clientAuth, expired at the start of 2021.
    Expired,
    /// serverAuth only: not usable as a client certificate.
    ServerAuthOnly,
}

/// A gateway certificate with CN `gateway` and serial `serial`, signed by `issuer`.
pub fn gateway(gateway: &str, serial: u64, kind: Kind, issuer: &Ca) -> Leaf {
    let key = key();
    let mut params = CertificateParams::default();
    params.distinguished_name.push(DnType::CommonName, gateway);
    params.serial_number = Some(SerialNumber::from(serial));
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    params.extended_key_usages = vec![match kind {
        Kind::ServerAuthOnly => ExtendedKeyUsagePurpose::ServerAuth,
        Kind::Valid | Kind::Expired => ExtendedKeyUsagePurpose::ClientAuth,
    }];
    if matches!(kind, Kind::Expired) {
        params.not_before = date_time_ymd(2020, 1, 1);
        params.not_after = date_time_ymd(2021, 1, 1);
    }
    let cert = params
        .signed_by(&key, &issuer.issuer)
        .expect("a gateway certificate");
    Leaf {
        cert: cert.pem(),
        key: key.serialize_pem(),
    }
}

/// A server certificate for `localhost`, signed by `issuer`; `cert` holds the chain.
pub fn server(issuer: &Ca) -> Leaf {
    let key = key();
    let mut params = CertificateParams::new(vec!["localhost".to_owned()]).expect("a name");
    params
        .distinguished_name
        .push(DnType::CommonName, "mqtt-broker");
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let cert = params
        .signed_by(&key, &issuer.issuer)
        .expect("a server certificate");
    Leaf {
        cert: format!("{}{}", cert.pem(), issuer.pem),
        key: key.serialize_pem(),
    }
}

/// A CRL of `issuer` that revokes `serials`, valid from the start of 2026 to 2099.
pub fn crl(issuer: &Ca, number: u64, serials: &[u64]) -> String {
    let crl = CertificateRevocationListParams {
        this_update: date_time_ymd(2026, 1, 1),
        next_update: date_time_ymd(2099, 1, 1),
        crl_number: SerialNumber::from(number),
        issuing_distribution_point: None,
        revoked_certs: serials
            .iter()
            .map(|serial| RevokedCertParams {
                serial_number: SerialNumber::from(*serial),
                revocation_time: date_time_ymd(2026, 1, 2),
                reason_code: None,
                invalidity_date: None,
            })
            .collect(),
        key_identifier_method: KeyIdMethod::Sha256,
    }
    .signed_by(&issuer.issuer)
    .expect("a CRL");
    crl.pem().expect("PEM")
}

/// Writes `leaf` as `<name>.pem` and `<name>.key` into `dir`, readable by every user (the
/// broker's container runs as another one).
pub fn write(dir: &Path, name: &str, leaf: &Leaf) {
    std::fs::write(dir.join(format!("{name}.pem")), &leaf.cert).expect("write a certificate");
    std::fs::write(dir.join(format!("{name}.key")), &leaf.key).expect("write a key");
}

/// Mounts `leaf` the way the kubelet mounts a Secret of type kubernetes.io/tls: `tls.crt` and
/// `tls.key` are symlinks through `..data` into a timestamped directory, and a new version is a
/// new directory and a rename of `..data`. `version` names the directory.
pub fn mount_secret(dir: &Path, version: u32, leaf: &Leaf) {
    let data = format!("..2026_10_22_00_00_{version:02}.000000001");
    let target = dir.join(&data);
    std::fs::create_dir_all(&target).expect("the Secret's data directory");
    std::fs::write(target.join("tls.crt"), &leaf.cert).expect("tls.crt");
    std::fs::write(target.join("tls.key"), &leaf.key).expect("tls.key");
    let temporary = dir.join("..data_tmp");
    let _ = std::fs::remove_file(&temporary);
    std::os::unix::fs::symlink(&data, &temporary).expect("..data_tmp");
    std::fs::rename(&temporary, dir.join("..data")).expect("..data");
    for name in ["tls.crt", "tls.key"] {
        let link = dir.join(name);
        if std::fs::symlink_metadata(&link).is_err() {
            std::os::unix::fs::symlink(Path::new("..data").join(name), &link).expect("a link");
        }
    }
}
