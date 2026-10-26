//! The MQTT broker's server certificate (Laysi/trustiot-v2#535; deploy/chart/README.md, "MQTT
//! broker"; services/certs/README.md, "broker 伺服器憑證"). Nobody creates, copies or renews it:
//!
//! - the chart declares the Secret `<release>-mqtt-broker-tls` without any data, and only the
//!   broker's pod mounts it (`tls.crt`: the server certificate, then the issuing CA certificate
//!   that signed it; `tls.key`: its private key, PKCS #8 PEM);
//! - [`converge`], run as `migrate mqtt-broker-certificate` by the chart's CronJob with a
//!   ServiceAccount of its own, reads the Secret and keeps what it holds while it is current
//!   ([`decide`]). Otherwise it generates a new ECDSA P-256 key in memory, sends its certificate
//!   signing request to the certs service's broker certificate port (which the mesh opens to this
//!   workload alone), checks the answer ([`check_issued`]) and writes the chain and the key with
//!   one JSON merge patch that carries the Secret's `resourceVersion`;
//! - every renewal makes a new key. A certificate is renewed once half of its validity has passed
//!   (services/certs/README.md, 「效期與換發」), at once when `mqttBroker.certificateGeneration`
//!   is higher than the generation the Secret records, and whenever the Secret does not hold a
//!   usable certificate for the broker's names;
//! - the Secret's data is trusted only while it records the Secret's own uid (the rules of
//!   src/passwords.rs): an object someone else made, restored or emptied gets a new certificate
//!   and key, and the patch that binds it removes the metadata neither the chart nor this run
//!   sets.
//!
//! The broker's trust sidecar (src/mqtt_trust.rs) opens the gateway listener once the Secret
//! holds a usable pair, and has the broker reload a renewed one (SIGHUP), which keeps every
//! connection.
//!
//! When the certs service cannot sign for the whole `wait` (HTTP 503: no usable issuing CA before
//! the CA ceremony, or its database is down; 429 or another 5xx; or no answer, as while it starts
//! after an install), a run on the chart's empty Secret, which carries none of the annotations a
//! fill writes ([`filled_before`]), ends with a warning and exit code 0: the broker keeps its
//! gateway port closed, and the next run tries again. Every other run that cannot renew fails, so
//! a failed Job shows that the broker's certificate is ageing (a usable one due for renewal) or
//! already of no use (expired, for other names, an unusable pair, data this CronJob did not
//! write, or emptied after a fill). A refusal (another 4xx) or an answer it cannot use always
//! fails the run.
//!
//! Only this CronJob's ServiceAccount may `get` and `patch` the Secret (the chart's Role names it),
//! and only its pod mounts that ServiceAccount's token. Nothing here logs the private key, the
//! Secret's data or the request: logs and errors name the serial number, the issuing CA's key id,
//! the names and the validity. The key, the patch that carries it and the Secret as read are wiped
//! from memory when dropped; what the HTTP client and TLS keep in their own buffers is not, which
//! the Job bounds by exiting once the run is done.

use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use contract::certs::{BROKER_SERVER_NAME, BrokerCertificate};
use rcgen::{
    CertificateParams, DistinguishedName, DnType, KeyPair, PKCS_ECDSA_P256_SHA256,
    PublicKeyData as _,
};
use rustls::pki_types::CertificateDer;
use rustls::pki_types::pem::PemObject as _;
use serde_json::{Map, Value, json};
use x509_parser::extensions::GeneralName;
use zeroize::{Zeroize as _, Zeroizing};

use crate::config::MqttCertificateSettings;
use crate::http::{self, Auth, Client};
use crate::mqtt_trust::{SERVER_CERTIFICATE, SERVER_KEY, check_pair};
use crate::passwords::{
    self, Access, Answer, Api, Foreign, Hints, MAX_CONFLICTS, Secret, binding_metadata,
};
use crate::signing_keys::unix_now;
use crate::{Error, Report};

/// The certs service's broker certificate issuance, on its broker port only
/// (services/certs/src/broker.rs).
pub const ISSUE_PATH: &str = "/internal/certs/v1/broker-certificate";

/// The media type of the request: a PKCS #10 certificate signing request, DER (RFC 5967).
pub const REQUEST_TYPE: &str = "application/pkcs10";

/// The annotation that records which `mqttBroker.certificateGeneration` the certificate in the
/// Secret was issued for.
pub const GENERATION_ANNOTATION: &str = "trustiot.io/certificate-generation";

/// How often an unanswered or refused issuance is tried again within the wait.
const RETRY_EVERY: Duration = Duration::from_secs(5);

/// Room for the patch: the chain (two certificates, a few KiB as PEM) and the key, in base64, plus
/// the metadata. Reserved up front so the buffer never moves while it grows and leaves an unwiped
/// copy of the key behind.
const PATCH_CAPACITY: usize = 64 * 1024;

/// The PEM label of a PKCS #8 private key (RFC 7468, section 10). Kept apart from the
/// encapsulation boundaries in `KeyRequest::key_pem` so this source file does not have the
/// shape of a PEM private key, which secret scanners report (no key is in it).
const PKCS8_KEY_LABEL: &str = "PRIVATE KEY";

/// What the errors about the Secret tell an operator.
const SECRET_HINTS: Hints = Hints {
    missing: "does not exist. The chart declares it without data while the MQTT broker is \
              enabled, and this CronJob fills it: run helm upgrade, which creates it again \
              (deploy/chart/README.md, \"MQTT broker\")",
    refused: "the chart's Role lets the broker certificate CronJob's ServiceAccount get and patch \
              exactly this Secret",
};

/// A certificate as far as the issuer judges it. No key material.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Current {
    /// The serial number, lower-case hex (the certs service's `serial`).
    pub serial: String,
    /// notBefore and notAfter, seconds since the Unix epoch.
    pub not_before: i64,
    pub not_after: i64,
    /// Its DNS subject alternative names, sorted.
    pub names: Vec<String>,
}

impl Current {
    /// When half of its validity has passed: from then on it is renewed.
    #[must_use]
    pub fn renew_from(&self) -> i64 {
        self.not_before + (self.not_after - self.not_before) / 2
    }
}

/// What the Secret holds when a run renews, which decides what a certs service that cannot sign
/// means.
#[derive(Debug, PartialEq, Eq)]
pub enum Holding {
    /// Nothing yet: the Secret the chart declared, without data and without the annotations a
    /// fill writes (before the CA ceremony; or declared again by Helm after it was deleted, which
    /// it cannot tell apart). Waiting for the certs service is normal.
    Nothing,
    /// Data the broker cannot keep serving: an expired certificate, one for other names, an
    /// unusable pair, data in an object this CronJob did not fill, or no data in a Secret whose
    /// annotations record a fill. Gateways cannot connect, or will not once the broker restarts,
    /// so a run that cannot replace it fails.
    Unusable,
    /// A usable certificate that is due for renewal; the broker keeps serving it meanwhile.
    Usable(Current),
}

/// What a run does with the Secret it read.
#[derive(Debug, PartialEq, Eq)]
pub enum Decision {
    /// It holds a usable certificate for the broker's names (the checks of [`check_chain`] and a
    /// matching key), of the requested generation, before half of its validity.
    Keep(Current),
    /// A new key and certificate, why, and what the Secret holds meanwhile.
    Renew { reason: String, holding: Holding },
}

/// What a run came to.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The Secret's certificate is current; nothing changed.
    Kept(Current),
    /// A new key and certificate are in the Secret.
    Issued {
        certificate: Current,
        reason: String,
    },
    /// The Secret was never filled ([`Holding::Nothing`]) and the certs service cannot sign yet
    /// (the CA ceremony is pending, it is still starting, or its database is down): nothing
    /// changed, and the broker keeps its gateway port closed.
    Waiting { reason: String },
}

/// The names a certificate must carry, sorted: [`BROKER_SERVER_NAME`] and the broker Service's
/// name when one is set.
#[must_use]
pub fn expected_names(service_name: &str) -> Vec<String> {
    let mut names = vec![BROKER_SERVER_NAME.to_owned()];
    if !service_name.is_empty() && service_name != BROKER_SERVER_NAME {
        names.push(service_name.to_owned());
    }
    names.sort();
    names
}

/// `bytes` in lower-case hex.
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        // Writing to a String cannot fail.
        let _ = write!(text, "{byte:02x}");
    }
    text
}

/// The first certificate of a PEM chain, as [`Current`], with its DER.
fn leaf_of(chain_pem: &[u8]) -> Result<(Current, Vec<CertificateDer<'static>>), String> {
    let chain: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(chain_pem)
        .collect::<Result<_, _>>()
        .map_err(|_| format!("{SERVER_CERTIFICATE} is not valid PEM"))?;
    let leaf = chain
        .first()
        .ok_or_else(|| format!("{SERVER_CERTIFICATE} holds no certificate"))?;
    let current = describe(leaf)?;
    Ok((current, chain))
}

/// The serial number, validity and DNS names of a certificate.
fn describe(der: &[u8]) -> Result<Current, String> {
    let Ok(([], certificate)) = x509_parser::parse_x509_certificate(der) else {
        return Err("the server certificate does not parse".to_owned());
    };
    let mut names: Vec<String> = match certificate.subject_alternative_name() {
        Ok(Some(extension)) => extension
            .value
            .general_names
            .iter()
            .filter_map(|name| match name {
                GeneralName::DNSName(name) => Some((*name).to_owned()),
                _ => None,
            })
            .collect(),
        Ok(None) => Vec::new(),
        Err(_) => return Err("the server certificate's names do not parse".to_owned()),
    };
    names.sort();
    Ok(Current {
        serial: hex(certificate.raw_serial()),
        not_before: certificate.validity().not_before.timestamp(),
        not_after: certificate.validity().not_after.timestamp(),
        names,
    })
}

/// The generation the Secret records; 0 when it records none it can read.
fn recorded_generation(found: &Secret) -> u64 {
    found
        .metadata
        .annotations
        .get(GENERATION_ANNOTATION)
        .and_then(|text| passwords::parse_generation(text))
        .unwrap_or(0)
}

/// Whether `found` carries an annotation that only a fill writes: the uid binding or the
/// generation. The patch that fills the Secret sets both, and neither the chart nor Helm sets
/// either, so a Secret without data that has one was filled before (and emptied, or created again
/// from a copy of it). A Secret that was deleted and declared again by Helm has neither: it reads
/// as never filled (deploy/chart/README.md, "伺服器憑證").
fn filled_before(found: &Secret) -> bool {
    let annotations = &found.metadata.annotations;
    annotations.contains_key(passwords::UID_ANNOTATION)
        || annotations.contains_key(GENERATION_ANNOTATION)
}

/// What to do with `found` when the run wants a certificate for `names` (sorted) of
/// `requested` generation at `now` (seconds since the Unix epoch). The reasons name the problem,
/// never the data.
pub(crate) fn decide(found: &Secret, names: &[String], requested: u64, now: i64) -> Decision {
    let renew = |reason: &str, holding: Holding| Decision::Renew {
        reason: reason.to_owned(),
        holding,
    };
    if found.data.is_empty() {
        return if filled_before(found) {
            renew(
                "the Secret holds no certificate, but its annotations record that this CronJob \
                 filled it: it was emptied, or deleted and created again from a copy",
                Holding::Unusable,
            )
        } else {
            renew("the Secret holds no certificate yet", Holding::Nothing)
        };
    }
    if !passwords::is_bound(&found.metadata) {
        return renew(
            "the Secret is not the object this CronJob filled (another uid, or none recorded), \
             so its data is not trusted",
            Holding::Unusable,
        );
    }
    let decoded = |key: &str| {
        found
            .data
            .get(key)
            .and_then(|value| STANDARD.decode(value).ok())
            .map(Zeroizing::new)
    };
    let (Some(chain), Some(key)) = (decoded(SERVER_CERTIFICATE), decoded(SERVER_KEY)) else {
        return renew(
            "the Secret lacks tls.crt or tls.key, or one is not base64",
            Holding::Unusable,
        );
    };
    if let Err(problem) = check_pair(&chain, &key) {
        return Decision::Renew {
            reason: format!("the Secret's certificate and key are unusable: {problem}"),
            holding: Holding::Unusable,
        };
    }
    // The checks an answer from the certs service gets, but the issuing CA's id, which the
    // Secret does not record: a Secret this CronJob filled passed them all when it was written,
    // so a failure means it expired or was changed since.
    let current = match check_chain(&chain, names, now, None) {
        Ok((current, _)) => current,
        Err(problem) => {
            return Decision::Renew {
                reason: problem,
                holding: Holding::Unusable,
            };
        }
    };
    let recorded = recorded_generation(found);
    if recorded < requested {
        return Decision::Renew {
            reason: format!(
                "mqttBroker.certificateGeneration is {requested}, the certificate was issued for \
                 {recorded}"
            ),
            holding: Holding::Usable(current),
        };
    }
    if now >= current.renew_from() {
        return renew(
            "half of the certificate's validity has passed",
            Holding::Usable(current),
        );
    }
    Decision::Keep(current)
}

/// A key generated for one run and its certificate signing request. The key's PKCS #8 copy is
/// wiped from memory when dropped (ring's own key object is not).
struct KeyRequest {
    key: KeyPair,
    request: Vec<u8>,
}

impl Drop for KeyRequest {
    fn drop(&mut self) {
        self.key.zeroize();
    }
}

impl KeyRequest {
    /// A new ECDSA P-256 key from the operating system's CSPRNG and its CSR, DER. The CSR's
    /// subject and names do not matter: the CA takes only its public key.
    fn generate() -> Result<Self, String> {
        let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256)
            .map_err(|error| format!("cannot generate a key: {error}"))?;
        let mut params = CertificateParams::default();
        let mut subject = DistinguishedName::new();
        subject.push(DnType::CommonName, BROKER_SERVER_NAME);
        params.distinguished_name = subject;
        let request = params
            .serialize_request(&key)
            .map_err(|error| format!("cannot build the certificate signing request: {error}"))?
            .der()
            .to_vec();
        Ok(Self { key, request })
    }

    /// The key as `tls.key`: PKCS #8, PEM, in a buffer that is wiped when dropped.
    fn key_pem(&self) -> Zeroizing<String> {
        let der = self.key.serialized_der();
        let encoded_len = base64::encoded_len(der.len(), true).unwrap_or(der.len() * 2);
        let mut encoded = Zeroizing::new(String::with_capacity(encoded_len));
        STANDARD.encode_string(der, &mut encoded);
        let mut pem = Zeroizing::new(String::with_capacity(encoded.len() * 2 + 64));
        pem.push_str("-----BEGIN ");
        pem.push_str(PKCS8_KEY_LABEL);
        pem.push_str("-----\n");
        for line in encoded.as_bytes().chunks(64) {
            // Base64 is ASCII, so every chunk is valid UTF-8.
            pem.push_str(std::str::from_utf8(line).unwrap_or_default());
            pem.push('\n');
        }
        pem.push_str("-----END ");
        pem.push_str(PKCS8_KEY_LABEL);
        pem.push_str("-----\n");
        pem
    }
}

/// Why no certificate came back.
enum IssueFailure {
    /// Until the wait ran out the certs service did not answer, or answered 503, 429 or another
    /// 5xx: it cannot sign now (before the CA ceremony, while it starts after an install, or while
    /// its database is down).
    Unavailable(String),
    /// A refusal or an answer that is not a certificate; the run fails.
    Failed(String),
}

/// How far ahead of this clock a certificate's notBefore may be: the certs service backdates
/// notBefore by as much for clocks that are behind (services/certs/src/issue.rs,
/// `CLOCK_SKEW_SECS`).
const CLOCK_SKEW_SECS: i64 = 300;

/// Checks a chain as the broker serves it (`tls.crt`, PEM) for `names` (sorted) at `now`: it
/// parses, its first certificate is for these names only, is not a CA, is a server certificate
/// (serverAuth, not clientAuth), is signed by the second, is valid by now (notBefore at most
/// [`CLOCK_SKEW_SECS`] ahead) and has not expired. With `issuer_key_id`, the second certificate
/// must also be that issuing CA's: the certs service names the CA that signed in its answer
/// (`ca_key_id`, the SHA-256 of the CA's SubjectPublicKeyInfo, as `crates/ca-keys` defines it).
/// Both the certs service's answer ([`check_issued`], with its `ca_key_id`) and what the Secret
/// holds ([`decide`], without: the Secret records no issuing CA, and its data is trusted only in
/// the object this CronJob filled, after the chain passed this check with the CA the answer
/// named) must pass it. Returns the first certificate and its public key (the SPKI, DER).
///
/// # Errors
///
/// What is wrong, without echoing the chain.
fn check_chain(
    chain_pem: &[u8],
    names: &[String],
    now: i64,
    issuer_key_id: Option<&str>,
) -> Result<(Current, Vec<u8>), String> {
    let (current, chain) = leaf_of(chain_pem)?;
    let [leaf, issuer, ..] = chain.as_slice() else {
        return Err(
            "the chain holds no issuing CA certificate after the server certificate".to_owned(),
        );
    };
    let Ok(([], leaf)) = x509_parser::parse_x509_certificate(leaf) else {
        return Err("the server certificate does not parse".to_owned());
    };
    let Ok(([], issuer)) = x509_parser::parse_x509_certificate(issuer) else {
        return Err("the issuing CA certificate does not parse".to_owned());
    };
    if current.names != names {
        return Err(format!(
            "the certificate names {}, not {}",
            current.names.join(", "),
            names.join(", ")
        ));
    }
    match leaf.basic_constraints() {
        Ok(None) => {}
        Ok(Some(constraints)) if !constraints.value.ca => {}
        Ok(Some(_)) => return Err("the certificate is a CA certificate".to_owned()),
        Err(_) => return Err("the certificate's basic constraints do not parse".to_owned()),
    }
    match leaf.extended_key_usage() {
        Ok(Some(usage)) if usage.value.server_auth && !usage.value.client_auth => {}
        _ => {
            return Err(
                "the certificate's extended key usage is not server authentication only".to_owned(),
            );
        }
    }
    if leaf.verify_signature(Some(issuer.public_key())).is_err() {
        return Err("the issuing CA certificate in the chain did not sign it".to_owned());
    }
    if let Some(expected) = issuer_key_id
        && ::ca_keys::key_id(issuer.public_key().raw) != expected
    {
        return Err(format!(
            "the issuing CA certificate in the chain is not the CA the certs service named \
             (ca_key_id {expected})"
        ));
    }
    if current.not_before > now.saturating_add(CLOCK_SKEW_SECS) {
        return Err(format!(
            "the certificate is not valid yet: its notBefore {} is more than {CLOCK_SKEW_SECS} \
             seconds after {now}",
            current.not_before
        ));
    }
    if current.not_after <= now {
        return Err("the certificate has expired".to_owned());
    }
    Ok((current, leaf.public_key().raw.to_vec()))
}

/// Checks the certs service's answer for `public_key` (the SPKI, DER) and `names` (sorted) at
/// `now`: [`check_chain`] with the issuing CA the answer names, and the certificate is for this
/// key. Returns the certificate.
///
/// # Errors
///
/// What is wrong, without echoing the answer.
pub fn check_issued(
    issued: &BrokerCertificate,
    public_key: &[u8],
    names: &[String],
    now: i64,
) -> Result<Current, String> {
    let (current, key) = check_chain(
        issued.certificate_chain.as_bytes(),
        names,
        now,
        Some(&issued.ca_key_id),
    )?;
    if key != public_key {
        return Err("the certificate is for another public key".to_owned());
    }
    Ok(current)
}

/// Asks the certs service at `url` to sign `key`'s request; retries an unanswered request, 429
/// and 5xx until `wait` runs out.
async fn issue(
    client: &Client,
    url: &str,
    key: &KeyRequest,
    wait: Duration,
) -> Result<BrokerCertificate, IssueFailure> {
    let started = Instant::now();
    let mut warned = false;
    loop {
        let state = match client
            .post_bytes(url, Auth::None, REQUEST_TYPE, key.request.clone())
            .await
        {
            Err(error) => format!("the certs service did not answer: {error}"),
            Ok(response) => match response.status.as_u16() {
                201 => {
                    return serde_json::from_str::<BrokerCertificate>(&response.body).map_err(
                        |_| {
                            IssueFailure::Failed(
                                "the certs service answered 201 with something that is not a \
                                 broker certificate"
                                    .to_owned(),
                            )
                        },
                    );
                }
                503 => format!(
                    "the certs service cannot sign now (HTTP 503: {})",
                    first_line(&response.body)
                ),
                status @ (429 | 500..=599) => format!(
                    "the certs service answered HTTP {status}: {}",
                    first_line(&response.body)
                ),
                status => {
                    return Err(IssueFailure::Failed(format!(
                        "the certs service refused the request: HTTP {status}: {}",
                        first_line(&response.body)
                    )));
                }
            },
        };
        if started.elapsed() >= wait {
            return Err(IssueFailure::Unavailable(format!(
                "gave up after {wait:?}: {state}"
            )));
        }
        if !warned {
            tracing::warn!(state = %state, "no server certificate yet; retrying");
            warned = true;
        }
        tokio::time::sleep(RETRY_EVERY).await;
    }
}

/// The first line of an error body, at most 200 characters: the certs service's error bodies are
/// one line that never repeats the request.
fn first_line(body: &str) -> String {
    body.lines()
        .next()
        .unwrap_or_default()
        .chars()
        .take(200)
        .collect()
}

/// The CronJob's run at the current time; see [`converge_at`].
///
/// # Errors
///
/// As [`converge_at`].
pub async fn converge(
    settings: &MqttCertificateSettings,
    report: &mut Report,
) -> Result<Outcome, Error> {
    converge_at(settings, report, unix_now()).await
}

/// The CronJob's run at `now` (seconds since the Unix epoch): reads the Secret, keeps a current
/// certificate, and otherwise issues a new key and certificate and writes them (module
/// documentation).
///
/// # Errors
///
/// When the Secret is missing or being deleted, the API refuses access or does not answer within
/// `settings.wait`, another writer keeps changing it, the certs service refuses the request or
/// answers something unusable, or the certs service cannot sign while the Secret is anything but
/// the chart's never-filled declaration ([`Holding`]). The Secret is left as it was.
pub async fn converge_at(
    settings: &MqttCertificateSettings,
    report: &mut Report,
    now: i64,
) -> Result<Outcome, Error> {
    let requested = passwords::parse_generation(&settings.generation).ok_or_else(|| {
        Error::BrokerCertificate("TRUSTIOT__CERTIFICATE__GENERATION: not a generation".to_owned())
    })?;
    let names = expected_names(&settings.service_name);
    let access = Access {
        api_url: &settings.api_url,
        namespace: &settings.namespace,
        token_file: &settings.token_file,
        ca_file: &settings.ca_file,
    };
    let secrets = Api::new(access, settings.wait, SECRET_HINTS)?;
    let certs = http::plain();
    let url = format!("{}{ISSUE_PATH}", settings.certs_url.trim_end_matches('/'));
    // One key per run: a run that meets a conflict and still has to renew writes the certificate
    // it already has.
    let mut fresh: Option<(KeyRequest, BrokerCertificate, Current)> = None;
    for _ in 0..MAX_CONFLICTS {
        let found = read_secret(&secrets, &settings.secret).await?;
        let (reason, holding) = match decide(&found, &names, requested, now) {
            Decision::Keep(current) => {
                tracing::info!(
                    secret = %settings.secret,
                    serial = %current.serial,
                    not_after = current.not_after,
                    renew_from = current.renew_from(),
                    "the broker's server certificate is current"
                );
                return Ok(Outcome::Kept(current));
            }
            Decision::Renew { reason, holding } => (reason, holding),
        };
        if fresh.is_none() {
            tracing::info!(
                secret = %settings.secret,
                reason = %reason,
                "requesting a new server certificate for the broker"
            );
            let key = KeyRequest::generate().map_err(Error::BrokerCertificate)?;
            match issue(&certs, &url, &key, settings.wait).await {
                Ok(issued) => {
                    let current =
                        check_issued(&issued, &key.key.subject_public_key_info(), &names, now)
                            .map_err(|problem| {
                                Error::BrokerCertificate(format!(
                                    "the certs service's answer is unusable: {problem}"
                                ))
                            })?;
                    fresh = Some((key, issued, current));
                }
                Err(IssueFailure::Unavailable(why)) => {
                    return match holding {
                        Holding::Nothing => {
                            tracing::warn!(
                                secret = %settings.secret,
                                reason = %why,
                                "the certs service cannot sign the broker's server certificate \
                                 yet (no usable issuing CA before the CA ceremony, it is still \
                                 starting, or its database is down); the broker serves no gateway \
                                 port until a run gets one (services/certs/README.md, \
                                 \"發簽發 CA 憑證\"). A Secret that was deleted and declared \
                                 again by Helm reads the same: if gateways connected before, the \
                                 broker's certificate is lost (deploy/chart/README.md, \
                                 \"伺服器憑證\")"
                            );
                            Ok(Outcome::Waiting { reason: why })
                        }
                        Holding::Unusable => Err(Error::BrokerCertificate(format!(
                            "{reason}, but {why}; the Secret holds no certificate the broker can \
                             keep serving: gateways refuse an expired one, and a broker that \
                             restarts comes up without its gateway port (deploy/chart/README.md, \
                             \"伺服器憑證\")"
                        ))),
                        Holding::Usable(usable) => Err(Error::BrokerCertificate(format!(
                            "{reason}, but {why}; the broker keeps serving serial {} until {}",
                            usable.serial, usable.not_after
                        ))),
                    };
                }
                Err(IssueFailure::Failed(why)) => return Err(Error::BrokerCertificate(why)),
            }
        }
        let Some((key, issued, current)) = &fresh else {
            continue;
        };
        let key_pem = key.key_pem();
        let generation = requested.max(if passwords::is_bound(&found.metadata) {
            recorded_generation(&found)
        } else {
            0
        });
        let foreign = if passwords::is_bound(&found.metadata) {
            Foreign::default()
        } else {
            Foreign::of(&found.metadata)
        };
        let others: Vec<String> = found
            .data
            .keys()
            .filter(|key| *key != SERVER_CERTIFICATE && *key != SERVER_KEY)
            .cloned()
            .collect();
        let body = secret_patch(
            &found.metadata.resource_version,
            &found.metadata.uid,
            generation,
            &issued.certificate_chain,
            &key_pem,
            &foreign,
            &others,
        );
        match secrets.request(&settings.secret, Some(&body)).await? {
            Answer::Conflict => {
                tracing::info!(
                    secret = %settings.secret,
                    "the Secret changed while this run wrote it; reading it again"
                );
            }
            Answer::Secret(stored) => {
                let kept = |name: &str, written: &[u8]| {
                    stored
                        .data
                        .get(name)
                        .and_then(|value| STANDARD.decode(value).ok())
                        .map(Zeroizing::new)
                        .is_some_and(|bytes| bytes.as_slice() == written)
                };
                if !kept(SERVER_CERTIFICATE, issued.certificate_chain.as_bytes())
                    || !kept(SERVER_KEY, key_pem.as_bytes())
                {
                    return Err(Error::Secret {
                        secret: settings.secret.clone(),
                        problem: "the API stored another certificate or key than was written"
                            .to_owned(),
                    });
                }
                if !foreign.is_empty() {
                    report.change(format!(
                        "removed from Secret {} what neither the chart nor the CronJob sets: {}",
                        settings.secret,
                        foreign.describe()
                    ));
                }
                if !others.is_empty() {
                    report.change(format!(
                        "removed from Secret {} the data it does not hold: {}",
                        settings.secret,
                        others.join(", ")
                    ));
                }
                // Serial, issuing CA, names and validity only, never the key or the chain.
                tracing::info!(
                    secret = %settings.secret,
                    serial = %current.serial,
                    ca_key_id = %issued.ca_key_id,
                    names = ?current.names,
                    not_before = current.not_before,
                    not_after = current.not_after,
                    generation,
                    reason = %reason,
                    "wrote a new server certificate and key for the broker"
                );
                report.change(format!(
                    "wrote a new server certificate (serial {}) and key in Secret {}: {reason}",
                    current.serial, settings.secret
                ));
                return Ok(Outcome::Issued {
                    certificate: current.clone(),
                    reason,
                });
            }
        }
    }
    Err(Error::Secret {
        secret: settings.secret.clone(),
        problem: format!(
            "changed {MAX_CONFLICTS} times while the CronJob worked on it; something else writes \
             it"
        ),
    })
}

/// The Secret `name`, which must have a uid and not be being deleted.
async fn read_secret(secrets: &Api, name: &str) -> Result<Secret, Error> {
    let failed = |problem: &str| Error::Secret {
        secret: name.to_owned(),
        problem: problem.to_owned(),
    };
    let Answer::Secret(found) = secrets.request(name, None).await? else {
        return Err(failed("the API answered a read with a conflict"));
    };
    if found.metadata.uid.is_empty() {
        return Err(failed("the API returned it without metadata.uid"));
    }
    if found.metadata.deletion_timestamp.is_some() {
        return Err(failed(
            "is being deleted (metadata.deletionTimestamp is set). Once it is gone, run helm \
             upgrade, which creates it again (deploy/chart/README.md, \"MQTT broker\")",
        ));
    }
    Ok(found)
}

/// The merge patch that writes `chain` and `key_pem` into the Secret at `resource_version`,
/// records `generation`, binds it to `uid`, and removes `foreign` metadata and the `others` data
/// keys.
fn secret_patch(
    resource_version: &str,
    uid: &str,
    generation: u64,
    chain: &str,
    key_pem: &str,
    foreign: &Foreign,
    others: &[String],
) -> Zeroizing<String> {
    let mut annotations = Map::new();
    annotations.insert(
        GENERATION_ANNOTATION.to_owned(),
        Value::String(generation.to_string()),
    );
    let metadata = binding_metadata(resource_version, uid, annotations, foreign);
    let mut data: Map<String, Value> = others
        .iter()
        .map(|key| (key.clone(), Value::Null))
        .collect();
    data.insert(
        SERVER_CERTIFICATE.to_owned(),
        Value::String(STANDARD.encode(chain.as_bytes())),
    );
    data.insert(
        SERVER_KEY.to_owned(),
        Value::String(STANDARD.encode(key_pem.as_bytes())),
    );
    let body = json!({ "metadata": metadata, "data": data });
    let mut buffer = Zeroizing::new(Vec::with_capacity(PATCH_CAPACITY));
    serde_json::to_writer(&mut *buffer, &body).expect("a JSON value always serializes");
    let text = Zeroizing::new(
        String::from_utf8(std::mem::take(&mut *buffer)).expect("serde_json writes UTF-8"),
    );
    if let Value::Object(mut body) = body
        && let Some(Value::Object(data)) = body.remove("data")
    {
        for (_, value) in data {
            if let Value::String(encoded) = value {
                drop(Zeroizing::new(encoded));
            }
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use rcgen::{
        BasicConstraints, CertifiedIssuer, ExtendedKeyUsagePurpose, IsCa, KeyUsagePurpose,
        date_time_ymd,
    };
    use x509_parser::prelude::FromDer as _;

    use super::*;
    use crate::passwords::UID_ANNOTATION;

    const UID: &str = "6a1f2c3e-0000-4000-8000-0000000005f5";
    const SERVICE: &str = "trustiot-mqtt-broker.trustiot.svc";
    const NOW: i64 = 1_800_000_000;
    const DAY: i64 = 86_400;

    fn names() -> Vec<String> {
        expected_names(SERVICE)
    }

    /// `seconds` since the Unix epoch, as a duration to add to the epoch.
    fn since_epoch(seconds: i64) -> Duration {
        Duration::from_secs(u64::try_from(seconds).expect("after 1970"))
    }

    struct Ca {
        issuer: CertifiedIssuer<'static, KeyPair>,
        pem: String,
    }

    fn ca(name: &str) -> Ca {
        let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).expect("a key");
        let mut params = CertificateParams::default();
        params.distinguished_name.push(DnType::CommonName, name);
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        let issuer = CertifiedIssuer::self_signed(params, key).expect("a CA");
        let pem = issuer.pem();
        Ca { issuer, pem }
    }

    /// A server certificate for `names` from `ca`, valid from `not_before` for `days`, with the
    /// extended key usages `usages`, and its key as PKCS #8 PEM.
    fn server(
        ca: &Ca,
        names: &[String],
        not_before: i64,
        days: i64,
        usages: Vec<ExtendedKeyUsagePurpose>,
    ) -> (String, String, KeyPair) {
        let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).expect("a key");
        let mut params = CertificateParams::new(names.to_vec()).expect("DNS names");
        params
            .distinguished_name
            .push(DnType::CommonName, BROKER_SERVER_NAME);
        params.extended_key_usages = usages;
        params.not_before = date_time_ymd(1970, 1, 1) + since_epoch(not_before);
        params.not_after = date_time_ymd(1970, 1, 1) + since_epoch(not_before + days * DAY);
        let leaf = params.signed_by(&key, &ca.issuer).expect("a certificate");
        let chain = format!("{}{}", leaf.pem(), ca.pem);
        let key_pem = key.serialize_pem();
        (chain, key_pem, key)
    }

    fn secret(bound: bool, generation: Option<&str>, data: &[(&str, &str)]) -> Secret {
        let mut annotations = BTreeMap::new();
        if bound {
            annotations.insert(UID_ANNOTATION.to_owned(), UID.to_owned());
        }
        if let Some(generation) = generation {
            annotations.insert(GENERATION_ANNOTATION.to_owned(), generation.to_owned());
        }
        let data: BTreeMap<String, String> = data
            .iter()
            .map(|(key, value)| ((*key).to_owned(), STANDARD.encode(value)))
            .collect();
        serde_json::from_value(json!({
            "metadata": {"resourceVersion": "7", "uid": UID, "annotations": annotations},
            "data": data,
        }))
        .expect("a Secret")
    }

    fn holding(chain: &str, key: &str, generation: &str) -> Secret {
        secret(
            true,
            Some(generation),
            &[(SERVER_CERTIFICATE, chain), (SERVER_KEY, key)],
        )
    }

    /// The reason of a renewal and what the Secret holds meanwhile: "nothing", "unusable" or
    /// "usable".
    fn reason_of(decision: &Decision) -> (String, &'static str) {
        match decision {
            Decision::Renew { reason, holding } => (
                reason.clone(),
                match holding {
                    Holding::Nothing => "nothing",
                    Holding::Unusable => "unusable",
                    Holding::Usable(_) => "usable",
                },
            ),
            Decision::Keep(_) => panic!("expected a renewal, got {decision:?}"),
        }
    }

    #[test]
    fn a_current_certificate_is_kept_until_half_its_validity() {
        let ca = ca("Issuing CA");
        let issued = NOW - 300;
        let (chain, key, _) = server(
            &ca,
            &names(),
            issued,
            30,
            vec![ExtendedKeyUsagePurpose::ServerAuth],
        );
        let found = holding(&chain, &key, "1");
        let Decision::Keep(current) = decide(&found, &names(), 1, NOW) else {
            panic!("a fresh certificate is kept");
        };
        assert_eq!(current.not_before, issued);
        assert_eq!(current.renew_from(), issued + 15 * DAY);
        assert!(matches!(
            decide(&found, &names(), 1, issued + 15 * DAY - 1),
            Decision::Keep(_)
        ));
        let decision = decide(&found, &names(), 1, issued + 15 * DAY);
        assert_eq!(
            reason_of(&decision),
            (
                "half of the certificate's validity has passed".to_owned(),
                "usable"
            )
        );
        // Expired: of no use to the broker, so a run that cannot renew it fails.
        let decision = decide(&found, &names(), 1, issued + 30 * DAY);
        assert_eq!(
            reason_of(&decision),
            ("the certificate has expired".to_owned(), "unusable")
        );
    }

    #[test]
    fn a_higher_generation_renews_and_a_lower_one_does_not() {
        let ca = ca("Issuing CA");
        let (chain, key, _) = server(
            &ca,
            &names(),
            NOW,
            30,
            vec![ExtendedKeyUsagePurpose::ServerAuth],
        );
        let found = holding(&chain, &key, "2");
        assert!(matches!(
            decide(&found, &names(), 1, NOW),
            Decision::Keep(_)
        ));
        assert!(matches!(
            decide(&found, &names(), 2, NOW),
            Decision::Keep(_)
        ));
        let (reason, held) = reason_of(&decide(&found, &names(), 3, NOW));
        assert!(reason.contains("certificateGeneration is 3"), "{reason}");
        assert_eq!(held, "usable", "the old certificate stays usable meanwhile");
        // No annotation counts as generation 0.
        let mut unrecorded = holding(&chain, &key, "1");
        unrecorded
            .metadata
            .annotations
            .remove(GENERATION_ANNOTATION);
        assert_eq!(
            reason_of(&decide(&unrecorded, &names(), 1, NOW)).1,
            "usable"
        );
    }

    #[test]
    fn data_is_trusted_only_in_the_object_the_cronjob_filled() {
        let ca = ca("Issuing CA");
        let (chain, key, _) = server(
            &ca,
            &names(),
            NOW,
            30,
            vec![ExtendedKeyUsagePurpose::ServerAuth],
        );
        // The chart's declaration, never filled: the only state in which a run may wait.
        let empty = secret(false, None, &[]);
        assert_eq!(
            reason_of(&decide(&empty, &names(), 1, NOW)),
            ("the Secret holds no certificate yet".to_owned(), "nothing")
        );
        // No data, but an annotation only a fill writes: emptied in place, or created again from
        // a copy (another uid). Not the never-filled declaration, so a run that cannot fill it
        // fails.
        let mut copied = secret(true, Some("1"), &[]);
        copied.metadata.uid = "another-uid".to_owned();
        for emptied in [
            secret(true, Some("1"), &[]),
            secret(true, None, &[]),
            secret(false, Some("1"), &[]),
            copied,
        ] {
            let (reason, held) = reason_of(&decide(&emptied, &names(), 1, NOW));
            assert!(
                reason.contains("record that this CronJob filled it"),
                "{reason}"
            );
            assert_eq!(held, "unusable");
        }
        let planted = secret(
            false,
            Some("1"),
            &[(SERVER_CERTIFICATE, &chain), (SERVER_KEY, &key)],
        );
        let (reason, held) = reason_of(&decide(&planted, &names(), 1, NOW));
        assert!(reason.contains("not trusted"), "{reason}");
        assert_eq!(held, "unusable");
        let mut restored = holding(&chain, &key, "1");
        restored.metadata.uid = "another-uid".to_owned();
        let (reason, held) = reason_of(&decide(&restored, &names(), 1, NOW));
        assert!(reason.contains("not trusted"), "{reason}");
        assert_eq!(held, "unusable");
    }

    #[test]
    fn unusable_contents_are_renewed_without_echoing_them() {
        let ca = ca("Issuing CA");
        let (chain, key, _) = server(
            &ca,
            &names(),
            NOW,
            30,
            vec![ExtendedKeyUsagePurpose::ServerAuth],
        );
        let (_, other_key, _) = server(
            &ca,
            &names(),
            NOW,
            30,
            vec![ExtendedKeyUsagePurpose::ServerAuth],
        );
        let cases = [
            // Filled once, then emptied: not the chart's never-filled declaration.
            secret(true, Some("1"), &[]),
            secret(true, Some("1"), &[(SERVER_CERTIFICATE, &chain)]),
            secret(true, Some("1"), &[(SERVER_KEY, &key)]),
            holding(&chain, &other_key, "1"),
            holding("not a certificate", &key, "1"),
            holding(&chain, "not a key", "1"),
        ];
        for found in &cases {
            let (reason, held) = reason_of(&decide(found, &names(), 1, NOW));
            assert_eq!(held, "unusable", "{reason}");
            assert!(!reason.contains("PRIVATE KEY"), "{reason}");
            assert!(!reason.contains("not a key"), "{reason}");
        }
        let mut not_base64 = holding(&chain, &key, "1");
        not_base64
            .data
            .insert(SERVER_KEY.to_owned(), "%%%".to_owned());
        assert_eq!(
            reason_of(&decide(&not_base64, &names(), 1, NOW)).1,
            "unusable"
        );
    }

    #[test]
    fn a_certificate_for_other_names_is_renewed() {
        let ca = ca("Issuing CA");
        let (chain, key, _) = server(
            &ca,
            &expected_names(""),
            NOW,
            30,
            vec![ExtendedKeyUsagePurpose::ServerAuth],
        );
        let found = holding(&chain, &key, "1");
        let (reason, held) = reason_of(&decide(&found, &names(), 1, NOW));
        assert!(reason.contains("names"), "{reason}");
        assert_eq!(held, "unusable");
        assert!(matches!(
            decide(&found, &expected_names(""), 1, NOW),
            Decision::Keep(_)
        ));
    }

    #[test]
    fn a_stored_certificate_must_pass_the_checks_of_an_answer() {
        // Each pair matches and names the broker, but gateways would refuse the certificate (not
        // a server certificate) or cannot build its chain (no issuing CA, or not the one that
        // signed it): renewed, and of no use meanwhile.
        let issuing = ca("Issuing CA");
        let foreign = ca("Another CA");
        let leaf_only = |chain: &str| {
            chain
                .split_inclusive("-----END CERTIFICATE-----\n")
                .next()
                .expect("the leaf")
                .to_owned()
        };
        let with = |usages: Vec<ExtendedKeyUsagePurpose>| {
            let (chain, key, _) = server(&issuing, &names(), NOW, 30, usages);
            (chain, key)
        };
        let (client_only, client_key) = with(vec![ExtendedKeyUsagePurpose::ClientAuth]);
        let (both, both_key) = with(vec![
            ExtendedKeyUsagePurpose::ServerAuth,
            ExtendedKeyUsagePurpose::ClientAuth,
        ]);
        let (none, none_key) = with(Vec::new());
        let (good, good_key) = with(vec![ExtendedKeyUsagePurpose::ServerAuth]);
        let (early, early_key, _) = server(
            &issuing,
            &names(),
            NOW + CLOCK_SKEW_SECS + 1,
            30,
            vec![ExtendedKeyUsagePurpose::ServerAuth],
        );
        let ca_leaf_key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).expect("a key");
        let mut params = CertificateParams::new(names()).expect("DNS names");
        params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        params.not_before = date_time_ymd(1970, 1, 1) + since_epoch(NOW);
        params.not_after = date_time_ymd(1970, 1, 1) + since_epoch(NOW + 30 * DAY);
        let ca_leaf = format!(
            "{}{}",
            params
                .signed_by(&ca_leaf_key, &issuing.issuer)
                .expect("signed")
                .pem(),
            issuing.pem
        );
        let cases = [
            (
                holding(&client_only, &client_key, "1"),
                "extended key usage",
            ),
            (holding(&both, &both_key, "1"), "extended key usage"),
            (holding(&none, &none_key, "1"), "extended key usage"),
            (holding(&early, &early_key, "1"), "not valid yet"),
            (
                holding(&ca_leaf, &ca_leaf_key.serialize_pem(), "1"),
                "is a CA certificate",
            ),
            (
                holding(&leaf_only(&good), &good_key, "1"),
                "no issuing CA certificate",
            ),
            (
                holding(
                    &format!("{}{}", leaf_only(&good), foreign.pem),
                    &good_key,
                    "1",
                ),
                "did not sign",
            ),
        ];
        for (found, why) in &cases {
            let (reason, held) = reason_of(&decide(found, &names(), 1, NOW));
            assert!(reason.contains(why), "{why}: {reason}");
            assert_eq!(held, "unusable", "{reason}");
        }
        assert!(matches!(
            decide(&holding(&good, &good_key, "1"), &names(), 1, NOW),
            Decision::Keep(_)
        ));
    }

    /// The certs service's key id of `ca`: the SHA-256 of its SubjectPublicKeyInfo.
    fn key_id(ca: &Ca) -> String {
        let (_, certificate) =
            x509_parser::parse_x509_certificate(ca.issuer.der()).expect("the CA parses");
        ::ca_keys::key_id(certificate.public_key().raw)
    }

    /// An answer with `chain` from the issuing CA whose key id is `ca_key_id`.
    fn answer(chain: &str, ca_key_id: &str) -> BrokerCertificate {
        serde_json::from_value(json!({
            "serial": "00",
            "ca_key_id": ca_key_id,
            "names": names(),
            "not_before": "2027-01-15T08:00:00Z",
            "not_after": "2027-02-14T08:00:00Z",
            "certificate_chain": chain,
        }))
        .expect("an answer")
    }

    #[test]
    fn only_an_answer_for_this_key_names_and_purpose_from_the_chains_ca_is_taken() {
        let issuing = ca("Issuing CA");
        let server_auth = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let (chain, _, key) = server(&issuing, &names(), NOW, 30, server_auth.clone());
        let spki = key.subject_public_key_info();
        let issuing_id = key_id(&issuing);
        let current = check_issued(&answer(&chain, &issuing_id), &spki, &names(), NOW)
            .expect("a good answer");
        assert_eq!(current.names, names());

        let (other, _, _) = server(&issuing, &names(), NOW, 30, server_auth.clone());
        let refused = |chain: &str, why: &str| {
            let error = check_issued(&answer(chain, &issuing_id), &spki, &names(), NOW)
                .expect_err("a bad answer");
            assert!(error.contains(why), "{error}");
        };
        refused(&other, "another public key");
        let leaf_only = chain
            .split_inclusive("-----END CERTIFICATE-----\n")
            .next()
            .expect("the leaf");
        refused(leaf_only, "no issuing CA certificate");
        let foreign = ca("Another CA");
        refused(&format!("{leaf_only}{}", foreign.pem), "did not sign");
        refused("garbage", "holds no certificate");
        // A chain that holds together, from another CA than the one the answer names.
        let error = check_issued(&answer(&chain, &key_id(&foreign)), &spki, &names(), NOW)
            .expect_err("another issuing CA");
        assert!(
            error.contains("not the CA the certs service named"),
            "{error}"
        );

        // Same key, wrong names or purpose.
        let mut params =
            CertificateParams::new(vec![BROKER_SERVER_NAME.to_owned()]).expect("a DNS name");
        params.extended_key_usages = server_auth;
        let narrow = format!(
            "{}{}",
            params
                .signed_by(&key, &issuing.issuer)
                .expect("signed")
                .pem(),
            issuing.pem
        );
        refused(&narrow, "names");
        let mut params = CertificateParams::new(names()).expect("params");
        params.extended_key_usages = vec![
            ExtendedKeyUsagePurpose::ServerAuth,
            ExtendedKeyUsagePurpose::ClientAuth,
        ];
        let both = format!(
            "{}{}",
            params
                .signed_by(&key, &issuing.issuer)
                .expect("signed")
                .pem(),
            issuing.pem
        );
        refused(&both, "extended key usage");

        let error = check_issued(
            &answer(&chain, &issuing_id),
            &spki,
            &names(),
            NOW + 31 * DAY,
        )
        .expect_err("an expired answer");
        assert!(error.contains("expired"), "{error}");
    }

    #[test]
    fn the_key_is_pkcs8_pem_and_matches_the_request() {
        let request = KeyRequest::generate().expect("a key");
        let pem = request.key_pem();
        assert!(pem.starts_with("-----BEGIN PRIVATE KEY-----\n"));
        assert!(pem.ends_with("-----END PRIVATE KEY-----\n"));
        assert!(pem.lines().all(|line| line.len() <= 64));
        let parsed = KeyPair::from_pem(&pem).expect("the PEM parses");
        assert_eq!(
            parsed.subject_public_key_info(),
            request.key.subject_public_key_info(),
            "the same key"
        );
        let (_, csr) = x509_parser::certification_request::X509CertificationRequest::from_der(
            &request.request,
        )
        .expect("a DER CSR");
        assert_eq!(
            csr.certification_request_info.subject_pki.raw,
            request.key.subject_public_key_info().as_slice()
        );
        csr.verify_signature().expect("signed by its key");
        let other = KeyRequest::generate().expect("another key");
        assert_ne!(
            other.key.subject_public_key_info(),
            request.key.subject_public_key_info(),
            "every run makes a new key"
        );
    }

    #[test]
    fn the_patch_carries_the_version_the_binding_the_generation_and_both_files() {
        let mut found = secret(false, None, &[("planted", "x")]);
        found
            .metadata
            .annotations
            .insert("example.com/copy-me".to_owned(), "yes".to_owned());
        let foreign = Foreign::of(&found.metadata);
        let body = secret_patch(
            "7",
            UID,
            2,
            "CHAIN",
            "KEY",
            &foreign,
            &["planted".to_owned()],
        );
        let body: Value = serde_json::from_str(&body).expect("JSON");
        assert_eq!(body["metadata"]["resourceVersion"], "7");
        assert_eq!(body["metadata"]["annotations"][UID_ANNOTATION], UID);
        assert_eq!(body["metadata"]["annotations"][GENERATION_ANNOTATION], "2");
        assert_eq!(
            body["metadata"]["annotations"]["example.com/copy-me"],
            Value::Null
        );
        assert_eq!(body["data"]["planted"], Value::Null);
        assert_eq!(body["data"][SERVER_CERTIFICATE], STANDARD.encode("CHAIN"));
        assert_eq!(body["data"][SERVER_KEY], STANDARD.encode("KEY"));
    }
}
