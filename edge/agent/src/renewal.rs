//! Client certificate renewal (rebuild checklist D2, decision 12, Laysi/trustiot-v2#83).
//!
//! The gateway renews its MQTT client certificate over HTTPS on the gateway host
//! ([`protocol::renewal`], services/certs/README.md "換發"), proving who it is with its TPM key:
//! a challenge (`step: challenge`), then the challenge's signing input signed with the key of the
//! current certificate (`step: renew`), which the cloud answers with a new certificate for the
//! same, registered key. The request goes to the host and port of `cloud.url` with the outer TLS
//! (`cloud.trust`); it needs no client certificate, so it works when the current one has expired
//! or was revoked.
//!
//! **When.** [`Renewal::run`] renews
//!
//! - on schedule, once `renewal.remaining_percent` of the certificate's validity is left by the
//!   gateway's clock ([`due_at`]; default 33, so a 90-day certificate renews with 30 days left).
//!   After a renewal in this process the next one also waits, by the monotonic clock, for the
//!   same share of the new certificate's validity, so a clock running far ahead cannot make the
//!   agent renew over and over; a clock far behind never makes the certificate look due, and
//!   then the broker's refusal below starts the renewal;
//! - at once when the broker refuses the certificate ([`crate::link::Link::certificate_refused`]:
//!   a TLS certificate alert, CONNACK 0x86 or 0x87, or the inner mTLS failing for
//!   [`crate::link::REFUSED_AFTER`] while the cloud is reachable): it expired or was revoked.
//!
//! **Retries.** A failed attempt waits before the next one, which starts over with a new
//! challenge: exponential backoff with full jitter (`renewal.retry_base_s`, `renewal.retry_cap_s`,
//! [`crate::backoff`]); after 429 or 503 with `Retry-After`, at least that long (capped at
//! `renewal.retry_cap_s`) plus the backoff's delay. A renewed certificate that the broker refuses
//! as well, before any connection with it succeeded, waits for the backoff too; a connection with
//! renewed credentials starts the backoff over.
//!
//! **Checks.** A renewed certificate is used only when its CN is this gateway, its public key is
//! the TPM key's, the chain leads to a root of `broker.ca` (the answer's `root_certificate` must be
//! one of them; the answer never adds a trust anchor) as webpki checks it, its usages are a gateway
//! certificate's ([`crate::identity::with_key`]) and it is newer than the current one. Otherwise
//! nothing changes: neither the file nor the credentials in use.
//!
//! **Swapping.** The chain is written over `identity.certificate` atomically ([`write_atomically`]:
//! a temporary file in the same directory with the old file's permissions, fsync, rename, fsync of
//! the directory), so a power cut leaves the old or the new chain, never a broken one. Then the
//! inner TLS settings are replaced ([`InnerConfig`]) and the credentials' version raised: the
//! connection in use finishes what it accepted and reconnects with the new certificate (see
//! [`crate::link::LinkError::Renewed`]); heartbeats report the new expiry. When the file cannot be
//! written the new certificate is used anyway and the error logged: after a restart the old one is
//! loaded, and renewal runs again if it has to.

mod https;

use std::fs::{self, File, OpenOptions, Permissions};
use std::future::Future;
use std::io::{self, Write as _};
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use base64::Engine as _;
use protocol::UnixMillis;
use protocol::renewal::{
    ChallengeRequest, MAX_SIGNATURE_BYTES, Purpose, RenewRequest, RenewalRequest,
    RenewedCertificate, is_nonce,
};
use rustls::SignatureScheme;
use rustls::sign::SigningKey;
use rustls_pki_types::pem::PemObject as _;
use rustls_pki_types::{CertificateDer, UnixTime};
use tokio::sync::watch;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

pub use self::https::HttpsEndpoint;
use crate::backoff::{Backoff, BackoffSettings};
use crate::identity::{self, Identity, utc};
use crate::link::LinkState;
use crate::tls::{self, BrokerTrust, InnerConfig, RootCrl, Validity};

/// The default of `renewal.remaining_percent`: renew with a third of the validity left.
pub const DEFAULT_REMAINING_PERCENT: u8 = 33;
/// The default of `renewal.retry_base_s`.
pub const DEFAULT_RETRY_BASE: Duration = Duration::from_secs(30);
/// The default of `renewal.retry_cap_s`.
pub const DEFAULT_RETRY_CAP: Duration = Duration::from_secs(1800);
/// The longest wait for the schedule before the clock is read again, so a clock that was set
/// (for example by NTP after a boot with a wrong RTC) is noticed.
pub const RECHECK: Duration = Duration::from_secs(3600);
/// The most bytes of an answer the agent reads; an answer is about 2 KiB.
pub const MAX_ANSWER_BYTES: usize = 64 * 1024;
/// The most certificates in a renewed chain (the certificate and its issuing CA, and room for a
/// cross-signed one).
pub const MAX_CHAIN_CERTIFICATES: usize = 4;
/// The longest one request may take, from the TCP connection to the end of the answer.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// The renewal settings from the configuration (`[renewal]`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RenewalSettings {
    /// Renew once this share of the certificate's validity is left, in percent.
    pub remaining_percent: u8,
    /// The backoff between failed attempts.
    pub retry: BackoffSettings,
}

impl Default for RenewalSettings {
    fn default() -> Self {
        Self {
            remaining_percent: DEFAULT_REMAINING_PERCENT,
            retry: BackoffSettings {
                base: DEFAULT_RETRY_BASE,
                cap: DEFAULT_RETRY_CAP,
            },
        }
    }
}

/// One answer of the renewal endpoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Answer {
    /// The HTTP status.
    pub status: u16,
    /// `Retry-After` in seconds, if the answer had one in that form.
    pub retry_after: Option<Duration>,
    /// The body, at most [`MAX_ANSWER_BYTES`].
    pub body: Vec<u8>,
}

/// Where renewal requests go: [`HttpsEndpoint`], or a stand-in in tests.
pub trait Endpoint: Send + Sync {
    /// Sends `request` as JSON and returns the answer.
    ///
    /// # Errors
    ///
    /// The endpoint cannot be reached or does not answer within [`REQUEST_TIMEOUT`].
    fn post(&self, request: &RenewalRequest) -> impl Future<Output = io::Result<Answer>> + Send;
}

/// The step of a renewal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// `step: challenge`.
    Challenge,
    /// `step: renew`.
    Renew,
}

impl std::fmt::Display for Step {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Challenge => "challenge",
            Self::Renew => "renew",
        })
    }
}

/// Why a renewal attempt failed. Every failure leaves the certificate in use and its file as they
/// were.
#[derive(Debug, thiserror::Error)]
pub enum RenewalError {
    /// No answer: the network, TLS with the endpoint, or the request timed out.
    #[error("the renewal endpoint did not answer: {0}")]
    Unreachable(io::Error),
    /// The endpoint refused a step.
    #[error("the renewal endpoint refused the {step} step with HTTP {status}")]
    Refused {
        /// Which step.
        step: Step,
        /// The HTTP status.
        status: u16,
        /// `Retry-After`, if the answer had it (429 and 503 do).
        retry_after: Option<Duration>,
    },
    /// An answer the agent cannot use.
    #[error("the renewal endpoint's answer to the {step} step is not usable: {reason}")]
    BadAnswer {
        /// Which step.
        step: Step,
        /// Why.
        reason: String,
    },
    /// The key did not sign the challenge.
    #[error("signing the challenge: {0}")]
    Signing(String),
    /// The renewed certificate fails a check; it is not used.
    #[error("the renewed certificate is not used: {0}")]
    Rejected(String),
}

impl RenewalError {
    /// The `Retry-After` the endpoint asked for with 429 or 503.
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::Refused {
                status: 429 | 503,
                retry_after,
                ..
            } => *retry_after,
            _ => None,
        }
    }
}

/// What the agent renews: the identity in use and what checks and holds it.
#[derive(Debug)]
pub struct Credentials {
    /// The certificate chain and key in use.
    pub identity: Identity,
    /// The certificate file (`identity.certificate`), which a renewal replaces.
    pub certificate: PathBuf,
    /// The roots of `broker.ca`: a renewed chain must lead to one of them.
    pub trust: BrokerTrust,
    /// The root CRL, for the new inner TLS settings.
    pub root_crl: RootCrl,
    /// The inner TLS settings in use, which a renewal replaces.
    pub inner: InnerConfig,
}

/// A wall clock.
pub type Clock = Arc<dyn Fn() -> SystemTime + Send + Sync>;

/// Renews the client certificate; see the module documentation.
pub struct Renewal<E> {
    endpoint: E,
    settings: RenewalSettings,
    credentials: Credentials,
    version: watch::Sender<u64>,
    not_after: watch::Sender<UnixMillis>,
    jitter: fn() -> u64,
    clock: Clock,
}

impl<E> std::fmt::Debug for Renewal<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Renewal")
            .field("settings", &self.settings)
            .field("identity", &self.credentials.identity)
            .field("version", &*self.version.borrow())
            .finish_non_exhaustive()
    }
}

impl<E: Endpoint> Renewal<E> {
    /// Renewal of `credentials` through `endpoint`.
    pub fn new(endpoint: E, settings: RenewalSettings, credentials: Credentials) -> Self {
        let not_after = credentials.identity.not_after;
        Self {
            endpoint,
            settings,
            credentials,
            version: watch::Sender::new(0),
            not_after: watch::Sender::new(not_after),
            jitter: crate::link::random_u64,
            clock: Arc::new(SystemTime::now),
        }
    }

    /// The source of the backoff jitter: a uniformly random `u64` (the OS's outside tests).
    #[must_use]
    pub fn with_jitter(mut self, jitter: fn() -> u64) -> Self {
        self.jitter = jitter;
        self
    }

    /// The same renewal through `endpoint` instead, for example a stand-in in tests. The receivers
    /// of [`Renewal::credentials`] and [`Renewal::certificate_not_after`] keep working.
    pub fn with_endpoint<F: Endpoint>(self, endpoint: F) -> Renewal<F> {
        Renewal {
            endpoint,
            settings: self.settings,
            credentials: self.credentials,
            version: self.version,
            not_after: self.not_after,
            jitter: self.jitter,
            clock: self.clock,
        }
    }

    /// The wall clock the schedule and the certificate check read (the system's outside tests).
    #[must_use]
    pub fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
        self
    }

    /// The credentials' version for [`crate::link::LinkSettings::credentials`]: raised after each
    /// renewal, once the new settings are in place.
    pub fn credentials(&self) -> watch::Receiver<u64> {
        self.version.subscribe()
    }

    /// The expiry of the certificate in use, for [`crate::heartbeat::HeartbeatSettings`].
    pub fn certificate_not_after(&self) -> watch::Receiver<UnixMillis> {
        self.not_after.subscribe()
    }

    /// The identity in use.
    pub fn identity(&self) -> &Identity {
        &self.credentials.identity
    }

    /// One renewal: a challenge, the signature, the new certificate, its checks, and on success
    /// the swap (file, inner TLS settings, version, expiry). Returns the new identity.
    ///
    /// # Errors
    ///
    /// See [`RenewalError`]; nothing changes then.
    pub async fn renew(&mut self) -> Result<Identity, RenewalError> {
        let renewed = self.fetch().await?;
        let identity = self.check(&renewed)?;
        self.install(identity.clone())?;
        Ok(identity)
    }

    /// Steps 1 and 2: the answer with the new certificate.
    async fn fetch(&self) -> Result<RenewedCertificate, RenewalError> {
        let gateway = &self.credentials.identity.gateway;
        let challenge = self
            .exchange(
                Step::Challenge,
                &RenewalRequest::Challenge(ChallengeRequest {
                    hardware_id: gateway.clone(),
                }),
                200,
            )
            .await?;
        let challenge: protocol::renewal::Challenge = parse(Step::Challenge, &challenge)?;
        let bad = |reason: &str| RenewalError::BadAnswer {
            step: Step::Challenge,
            reason: reason.to_owned(),
        };
        if challenge.hardware_id != *gateway {
            return Err(bad("the challenge is for another hardware ID"));
        }
        if challenge.purpose != Purpose::CertRenewal {
            return Err(bad("the challenge is for another purpose"));
        }
        if !is_nonce(&challenge.nonce) {
            return Err(bad("the nonce is malformed"));
        }
        let signature = sign(
            Arc::clone(&self.credentials.identity.key),
            challenge.signing_input(),
        )
        .await?;
        let renewed = self
            .exchange(
                Step::Renew,
                &RenewalRequest::Renew(RenewRequest {
                    hardware_id: gateway.clone(),
                    nonce: challenge.nonce,
                    signature: base64::engine::general_purpose::STANDARD.encode(signature),
                }),
                201,
            )
            .await?;
        parse(Step::Renew, &renewed)
    }

    /// Sends one request and returns the body of an answer with status `success`.
    async fn exchange(
        &self,
        step: Step,
        request: &RenewalRequest,
        success: u16,
    ) -> Result<Vec<u8>, RenewalError> {
        let answer = self
            .endpoint
            .post(request)
            .await
            .map_err(RenewalError::Unreachable)?;
        if answer.status == success {
            return Ok(answer.body);
        }
        if (200..300).contains(&answer.status) {
            return Err(RenewalError::BadAnswer {
                step,
                reason: format!("HTTP {} instead of {success}", answer.status),
            });
        }
        Err(RenewalError::Refused {
            step,
            status: answer.status,
            retry_after: answer.retry_after,
        })
    }

    /// The checks of the module documentation; the identity of the new certificate if it passes.
    fn check(&self, renewed: &RenewedCertificate) -> Result<Identity, RenewalError> {
        let current = &self.credentials.identity;
        if renewed.hardware_id != current.gateway {
            return Err(RenewalError::Rejected(format!(
                "it is for hardware ID {}, not this gateway's",
                renewed.hardware_id
            )));
        }
        let chain = certificates(&renewed.certificate_chain)
            .map_err(|reason| RenewalError::Rejected(format!("certificate_chain: {reason}")))?;
        if chain.len() > MAX_CHAIN_CERTIFICATES {
            return Err(RenewalError::Rejected(format!(
                "certificate_chain has {} certificates, more than {MAX_CHAIN_CERTIFICATES}",
                chain.len()
            )));
        }
        let leaf = certificates(&renewed.certificate)
            .map_err(|reason| RenewalError::Rejected(format!("certificate: {reason}")))?;
        if leaf.len() != 1 || chain.first() != leaf.first() {
            return Err(RenewalError::Rejected(
                "certificate is not the first certificate of certificate_chain".to_owned(),
            ));
        }
        let root = certificates(&renewed.root_certificate)
            .map_err(|reason| RenewalError::Rejected(format!("root_certificate: {reason}")))?;
        match root.as_slice() {
            [root] if self.credentials.trust.is_anchor(root) => {}
            _ => {
                return Err(RenewalError::Rejected(
                    "root_certificate is not one of the roots in broker.ca".to_owned(),
                ));
            }
        }
        let identity = identity::with_key(
            Path::new("the renewal answer"),
            chain,
            Arc::clone(&current.key),
        )
        .map_err(|error| RenewalError::Rejected(error.to_string()))?;
        if identity.gateway != current.gateway {
            return Err(RenewalError::Rejected(format!(
                "its CN is {}, not this gateway's",
                identity.gateway
            )));
        }
        match tls::check_client_certificate(
            &self.credentials.trust,
            &identity,
            unix_time((self.clock)()),
        ) {
            Ok(Validity::Current) => {}
            Ok(Validity::OutsideNow) => tracing::warn!(
                not_before = %utc(identity.not_before),
                not_after = %utc(identity.not_after),
                "by this clock the renewed certificate is not valid now; using it anyway, since the clock may be wrong"
            ),
            Err(error) => return Err(RenewalError::Rejected(error.to_string())),
        }
        // Issued after the one in use: the certs service starts every certificate five minutes
        // before it signs it, so a later issuance starts later whatever its validity, and an
        // answer replayed from an earlier registration or renewal does not. Only the
        // certificates' own times are compared, never the gateway's clock.
        if identity.not_before <= current.not_before {
            return Err(RenewalError::Rejected(format!(
                "it was not issued after the certificate in use (valid from {}, the one in use from {})",
                utc(identity.not_before),
                utc(current.not_before)
            )));
        }
        Ok(identity)
    }

    /// Puts `identity` in place: the file, the inner TLS settings, the version and the expiry.
    fn install(&mut self, identity: Identity) -> Result<(), RenewalError> {
        let config = tls::inner_config(
            &self.credentials.trust,
            &self.credentials.root_crl,
            &identity,
        )
        .map_err(|error| RenewalError::Rejected(error.to_string()))?;
        let path = &self.credentials.certificate;
        if let Err(error) = write_atomically(path, pem_chain(&identity.chain).as_bytes()) {
            tracing::error!(
                path = %path.display(),
                %error,
                "the renewed certificate could not be saved; using it until the agent restarts, which loads the old one"
            );
        }
        self.credentials.inner.replace(config);
        self.not_after.send_replace(identity.not_after);
        self.credentials.identity = identity;
        self.version.send_modify(|version| *version += 1);
        tracing::info!(
            gateway = %self.credentials.identity.gateway,
            not_before = %utc(self.credentials.identity.not_before),
            not_after = %utc(self.credentials.identity.not_after),
            "the client certificate was renewed"
        );
        Ok(())
    }

    /// Renews on schedule and when the broker refuses the certificate, until `shutdown`. `link` is
    /// the link's state ([`crate::link::Link::state`]) and `refused` its refusals
    /// ([`crate::link::Link::certificate_refused`]).
    pub async fn run(
        mut self,
        mut link: watch::Receiver<LinkState>,
        mut refused: watch::Receiver<Option<u64>>,
        shutdown: CancellationToken,
    ) {
        let mut backoff = Backoff::new(self.settings.retry);
        // The earliest time of the next attempt, after a failure.
        let mut retry_at: Option<Instant> = None;
        // After a renewal in this process: not before then by schedule (monotonic).
        let mut floor: Option<Instant> = None;
        // A renewal whose credentials no connection has used yet.
        let mut unproven = false;
        tracing::info!(
            not_after = %utc(self.credentials.identity.not_after),
            due = %utc(due_at(&self.credentials.identity, self.settings.remaining_percent)),
            "certificate renewal is scheduled"
        );
        loop {
            let now = Instant::now();
            let version = *self.version.borrow();
            let refusal = refused
                .borrow_and_update()
                .is_some_and(|refused| refused >= version);
            let until_due = until(
                due_at(&self.credentials.identity, self.settings.remaining_percent),
                (self.clock)(),
            );
            let scheduled = until_due.is_zero() && floor.is_none_or(|floor| now >= floor);
            if refusal && unproven && retry_at.is_none() {
                // The broker refuses the renewed certificate too: no new one at once.
                let delay = backoff.next_delay((self.jitter)());
                tracing::warn!(
                    retry_in_s = delay.as_secs(),
                    "the broker refuses the renewed certificate as well; renewing again later"
                );
                retry_at = Some(now + delay);
                unproven = false;
            }
            let wanted = refusal || scheduled;
            if wanted && retry_at.is_none_or(|at| now >= at) {
                retry_at = None;
                tracing::info!(refused = refusal, "renewing the client certificate");
                let attempt = tokio::select! {
                    () = shutdown.cancelled() => return,
                    attempt = self.renew() => attempt,
                };
                match attempt {
                    Ok(identity) => {
                        unproven = true;
                        floor = Some(
                            Instant::now() + share(&identity, self.settings.remaining_percent),
                        );
                    }
                    Err(error) => {
                        let delay = retry_delay(
                            &error,
                            &mut backoff,
                            (self.jitter)(),
                            self.settings.retry.cap,
                        );
                        tracing::warn!(
                            %error,
                            retry_in_s = delay.as_secs(),
                            failures = backoff.failures(),
                            "certificate renewal failed"
                        );
                        retry_at = Some(Instant::now() + delay);
                    }
                }
                continue;
            }
            let wake = match (wanted, retry_at) {
                (true, Some(at)) => at,
                _ => {
                    let schedule = now + until_due.min(RECHECK);
                    let schedule = floor.map_or(schedule, |floor| schedule.max(floor));
                    retry_at.map_or(schedule, |at| schedule.max(at))
                }
            };
            tokio::select! {
                () = shutdown.cancelled() => return,
                () = tokio::time::sleep_until(wake) => {}
                changed = refused.changed() => if changed.is_err() { return },
                changed = link.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    let connected = matches!(*link.borrow_and_update(), LinkState::Connected { .. });
                    if connected && unproven {
                        // A connection with the renewed credentials: they work.
                        unproven = false;
                        backoff.reset();
                    }
                }
            }
        }
    }
}

/// When `identity` is due for renewal: once `percent` of its validity is left.
pub fn due_at(identity: &Identity, percent: u8) -> UnixMillis {
    let left = part(identity, percent);
    UnixMillis::new(
        identity
            .not_after
            .get()
            .saturating_sub(u64::try_from(left.as_millis()).unwrap_or(u64::MAX)),
    )
}

/// The part of `identity`'s validity before it is due: the shortest time between two scheduled
/// renewals in one process.
fn share(identity: &Identity, percent: u8) -> Duration {
    part(identity, 100_u8.saturating_sub(percent))
}

/// `percent` of `identity`'s validity.
fn part(identity: &Identity, percent: u8) -> Duration {
    let validity = identity
        .not_after
        .get()
        .saturating_sub(identity.not_before.get());
    let part = u128::from(validity) * u128::from(percent) / 100;
    Duration::from_millis(u64::try_from(part).unwrap_or(u64::MAX))
}

/// How long from `now` until `at`; zero once it has passed.
fn until(at: UnixMillis, now: SystemTime) -> Duration {
    let now = UnixMillis::from_system_time(now).map_or(0, UnixMillis::get);
    Duration::from_millis(at.get().saturating_sub(now))
}

/// How long to wait after `error`: the backoff's next delay, and after 429 or 503 at least the
/// `Retry-After` (capped at `cap`) on top of it.
pub fn retry_delay(
    error: &RenewalError,
    backoff: &mut Backoff,
    random: u64,
    cap: Duration,
) -> Duration {
    let delay = backoff.next_delay(random);
    match error.retry_after() {
        Some(after) => after.min(cap) + delay,
        None => delay,
    }
}

/// Signs `input` with `key` (ECDSA P-256 SHA-256, DER) off the async threads: a TPM signature
/// blocks for tens of milliseconds.
async fn sign(key: Arc<dyn SigningKey>, input: Vec<u8>) -> Result<Vec<u8>, RenewalError> {
    let signature = tokio::task::spawn_blocking(move || {
        let signer = key
            .choose_scheme(&[SignatureScheme::ECDSA_NISTP256_SHA256])
            .ok_or_else(|| "the key cannot sign with ECDSA P-256 SHA-256".to_owned())?;
        signer.sign(&input).map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| RenewalError::Signing(error.to_string()))?
    .map_err(RenewalError::Signing)?;
    if signature.is_empty() || signature.len() > MAX_SIGNATURE_BYTES {
        return Err(RenewalError::Signing(format!(
            "a signature of {} bytes is not a DER ECDSA P-256 signature",
            signature.len()
        )));
    }
    Ok(signature)
}

fn parse<T: serde::de::DeserializeOwned>(step: Step, body: &[u8]) -> Result<T, RenewalError> {
    serde_json::from_slice(body).map_err(|error| RenewalError::BadAnswer {
        step,
        reason: format!("not the expected JSON: {error}"),
    })
}

/// The certificates of PEM `text`; anything else in it is an error.
fn certificates(text: &str) -> Result<Vec<CertificateDer<'static>>, String> {
    let mut found = Vec::new();
    for item in CertificateDer::pem_slice_iter(text.as_bytes()) {
        let item = item.map_err(|error| error.to_string())?;
        found.push(item);
    }
    if found.is_empty() {
        return Err("no PEM certificate".to_owned());
    }
    let sections = text.matches("-----BEGIN ").count();
    if sections != found.len() {
        return Err("PEM sections other than certificates".to_owned());
    }
    Ok(found)
}

/// `chain` as PEM, leaf first.
fn pem_chain(chain: &[CertificateDer<'_>]) -> String {
    let mut text = String::new();
    for certificate in chain {
        text.push_str("-----BEGIN CERTIFICATE-----\n");
        let encoded = base64::engine::general_purpose::STANDARD.encode(certificate.as_ref());
        for line in encoded.as_bytes().chunks(64) {
            text.push_str(&String::from_utf8_lossy(line));
            text.push('\n');
        }
        text.push_str("-----END CERTIFICATE-----\n");
    }
    text
}

/// Replaces `path` with `contents` so that a power cut at any moment leaves either the old or the
/// new contents: a temporary file in the same directory with the old file's permissions (0600 if
/// there is none), fsync, rename over `path`, fsync of the directory. A temporary file left by an
/// earlier cut is overwritten.
///
/// # Errors
///
/// Any step failed; `path` is then unchanged, and the temporary file removed if it can be.
pub fn write_atomically(path: &Path, contents: &[u8]) -> io::Result<()> {
    let directory = path
        .parent()
        .filter(|directory| !directory.as_os_str().is_empty())
        .ok_or_else(|| io::Error::other("the certificate path has no directory"))?;
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::other("the certificate path has no file name"))?;
    let mode = match fs::metadata(path) {
        Ok(metadata) => metadata.permissions().mode() & 0o777,
        Err(error) if error.kind() == io::ErrorKind::NotFound => 0o600,
        Err(error) => return Err(error),
    };
    let mut temporary_name = name.to_os_string();
    temporary_name.push(".renewal.tmp");
    let temporary = directory.join(temporary_name);
    let written = (|| {
        match fs::remove_file(&temporary) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&temporary)?;
        // The mode given to open is reduced by the umask; the old file's mode is what counts.
        file.set_permissions(Permissions::from_mode(mode))?;
        file.write_all(contents)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, path)
    })();
    if let Err(error) = written {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    // The rename is durable only once the directory entry is on the disk.
    File::open(directory)?.sync_all()
}

/// `at` for the certificate check; a time before 1970 is "too early".
fn unix_time(at: SystemTime) -> UnixTime {
    UnixTime::since_unix_epoch(
        at.duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default(),
    )
}

#[cfg(test)]
mod tests;
