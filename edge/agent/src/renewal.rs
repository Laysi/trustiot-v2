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
//!   gateway's clock ([`due_at`]; default 33, so a 90-day certificate renews with 30 days left),
//!   and not before the same share of its validity has passed since it was put in place (the
//!   certificate file's modification time, set by the gateway's clock when it is written). After
//!   a renewal in this process the next one also waits for that share by the monotonic clock. So
//!   a clock running ahead cannot make the agent renew over and over, restarts included; a clock
//!   far behind never makes the certificate look due, and then the broker's refusal below starts
//!   the renewal;
//! - when the broker refuses the certificate ([`crate::link::Link::certificate_refused`]): a TLS
//!   certificate alert from the inner mTLS handshake (inside the outer TLS, so from the broker's
//!   side) or inside the connection, CONNACK 0x86 or 0x87, or the inner mTLS failing for
//!   [`crate::link::REFUSED_AFTER`] plus a random part of [`crate::link::REFUSED_JITTER`] while
//!   the cloud is reachable. An alert in the outer TLS handshake is not a refusal: anything on
//!   the path can send one. An explicit refusal of a certificate not proven yet renews at once.
//!   An inferred one (the inner mTLS failing) of a certificate valid by the gateway's clock waits
//!   for the backoff below, since the broker may be the trouble. A refusal of a proven
//!   certificate renews no sooner than [`REFUSAL_RENEWAL_INTERVAL`] after the last renewal a
//!   refusal started.
//!
//! **Proof.** A certificate is proven once a connection opened with it stays up for
//! [`crate::backoff::STABLE_AFTER`]. Only a connection whose attempt presented it counts
//! ([`crate::link::Link::attempts`] names the credentials version of each attempt), so not the
//! connection that was up when it was put in place, and a reconnect while it goes in counts.
//!
//! **Retries.** A failed attempt waits before the next one, which starts over with a new
//! challenge: exponential backoff with full jitter (`renewal.retry_base_s`, `renewal.retry_cap_s`,
//! [`crate::backoff`]); after 429 or 503 with `Retry-After`, at least that long (capped at
//! `renewal.retry_cap_s`) plus the backoff's delay. A renewed certificate that the broker refuses
//! as well before it is proven waits for the backoff too; only proving a renewed certificate
//! starts the backoff over.
//!
//! **Checks.** A renewed certificate is used only when its CN is this gateway, its public key is
//! the TPM key's, the chain leads to a root of `broker.ca` (the answer's `root_certificate` must be
//! one of them; the answer never adds a trust anchor) as webpki checks it, its usages are a gateway
//! certificate's ([`crate::identity::with_key`]), it is valid now by the gateway's clock (its
//! notBefore at most [`CLOCK_SKEW`] ahead, its notAfter not passed; the gateway has a hardware
//! clock, so a certificate it cannot use yet would only lock it out) and it starts after the last
//! proven certificate (an answer replayed from an earlier issuance does not). Otherwise nothing
//! changes: neither the file nor the credentials in use.
//!
//! **Rollback.** Before a proven certificate that the broker has not refused is replaced (a
//! renewal on schedule), it is kept next to the certificate file ([`PREVIOUS_SUFFIX`], written
//! the same way with the same permissions). When the broker explicitly refuses the renewed one
//! (not an inferred refusal) before it is proven and the kept one is still valid by the gateway's
//! clock, the kept one is put back (no request) and counts as not proven: it is never rolled back
//! from, and the next renewal waits for the backoff. The kept file is removed once the
//! certificate file durably holds the certificate put back (otherwise a restart can roll back
//! again), or once the renewed certificate is proven. The kept file outlives a restart; at startup it is used only when it
//! belongs to this gateway and key, chains to `broker.ca` and is older than the certificate in
//! use, and removed otherwise.
//!
//! **Swapping.** The chain is written over `identity.certificate` atomically ([`write_atomically`]:
//! a temporary file in the same directory with the old file's permissions, fsync, rename, fsync of
//! the directory), so a power cut leaves the old or the new chain, never a broken one. Then the
//! inner TLS settings are replaced ([`InnerConfig`]) and the credentials' version raised: the
//! connection in use finishes what it accepted and reconnects with the new certificate (see
//! [`crate::link::LinkError::Renewed`]); heartbeats report the new expiry. When the file cannot be
//! written the new certificate is used anyway and the error logged: after a restart the old one is
//! loaded, and renewal runs again if it has to. When only the directory's fsync fails, the file
//! has the new chain, which a power cut may still undo; that is logged as a warning.

mod https;

use std::fs::{self, File, OpenOptions, Permissions};
use std::future::Future;
use std::io::{self, Write as _};
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use base64::Engine as _;
use protocol::renewal::{
    ChallengeRequest, MAX_SIGNATURE_BYTES, Purpose, RenewRequest, RenewalRequest,
    RenewedCertificate, is_nonce,
};
use protocol::{SessionId, UnixMillis};
use rustls::SignatureScheme;
use rustls::sign::SigningKey;
use rustls_pki_types::pem::PemObject as _;
use rustls_pki_types::{CertificateDer, UnixTime};
use tokio::sync::watch;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

pub use self::https::HttpsEndpoint;
use crate::backoff::{Backoff, BackoffSettings, STABLE_AFTER};
use crate::identity::{self, Identity, utc};
use crate::link::{Attempt, LinkState, Refusal, printable};
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
/// How far after the gateway's now a renewed certificate may start: the certs service starts
/// every certificate this long before it signs it (`CLOCK_SKEW_SECS` in services/certs), so a
/// gateway clock that is behind by up to twice this still takes it. One that starts later, or
/// has already ended, is refused: the gateway has a hardware RTC, and installing it could leave
/// the gateway without a certificate the broker takes.
pub const CLOCK_SKEW: Duration = Duration::from_secs(300);
/// The shortest time between two renewals that the broker's refusal of a proven certificate
/// starts, so a broker (or a relay in front of it) that keeps refusing cannot make the gateway
/// renew over and over.
pub const REFUSAL_RENEWAL_INTERVAL: Duration = Duration::from_secs(15 * 60);
/// The name the last proven certificate chain is kept under, next to `identity.certificate`, while
/// a renewed one has not been proven yet.
pub const PREVIOUS_SUFFIX: &str = ".prev";

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
    /// Whether a connection with the certificate in use stayed up for
    /// [`crate::backoff::STABLE_AFTER`] in this process.
    proven: bool,
    /// The last proven certificate, kept in [`PREVIOUS_SUFFIX`] while the one in use is not
    /// proven: what a rollback goes back to.
    previous: Option<Identity>,
    /// A renewed certificate must start after this: the start of the last proven certificate
    /// (or of the one loaded at startup when none is known).
    replay_floor: UnixMillis,
    /// When the certificate in use was put in place, by the gateway's clock: the file's
    /// modification time, which outlives a restart.
    installed_at: Option<SystemTime>,
    /// The broker refused the certificate in use: it is not worth going back to.
    refused_in_use: bool,
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
        let installed_at = fs::metadata(&credentials.certificate)
            .and_then(|metadata| metadata.modified())
            .ok();
        let previous = load_previous(&credentials);
        let replay_floor = previous
            .as_ref()
            .map_or(credentials.identity.not_before, |previous| {
                previous.not_before
            });
        Self {
            endpoint,
            settings,
            credentials,
            version: watch::Sender::new(0),
            not_after: watch::Sender::new(not_after),
            jitter: crate::link::random_u64,
            clock: Arc::new(SystemTime::now),
            proven: false,
            previous,
            replay_floor,
            installed_at,
            refused_in_use: false,
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
            proven: self.proven,
            previous: self.previous,
            replay_floor: self.replay_floor,
            installed_at: self.installed_at,
            refused_in_use: self.refused_in_use,
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
        self.check_valid_now(&identity)?;
        // Issued after the last proven certificate: the certs service starts every certificate
        // CLOCK_SKEW before it signs it, so a later issuance starts later whatever its validity,
        // and an answer replayed from an earlier registration or renewal does not. The floor is
        // the last proven certificate's start, so an installed one that never worked cannot
        // raise it.
        if identity.not_before <= self.replay_floor {
            return Err(RenewalError::Rejected(format!(
                "it was not issued after the last proven certificate (valid from {}, that one from {})",
                utc(identity.not_before),
                utc(self.replay_floor)
            )));
        }
        Ok(identity)
    }

    /// Whether `identity` can be used now by the gateway's clock: started (or starting within
    /// [`CLOCK_SKEW`]), not ended, and its chain valid then as webpki checks it.
    fn check_valid_now(&self, identity: &Identity) -> Result<(), RenewalError> {
        let now = UnixMillis::from_system_time((self.clock)()).unwrap_or(UnixMillis::new(0));
        let latest_start = now
            .get()
            .saturating_add(u64::try_from(CLOCK_SKEW.as_millis()).unwrap_or(u64::MAX));
        if identity.not_before.get() > latest_start {
            return Err(RenewalError::Rejected(format!(
                "it starts at {}, more than {} s after this clock's now ({})",
                utc(identity.not_before),
                CLOCK_SKEW.as_secs(),
                utc(now)
            )));
        }
        if identity.not_after <= now {
            return Err(RenewalError::Rejected(format!(
                "it ended at {}, before this clock's now ({})",
                utc(identity.not_after),
                utc(now)
            )));
        }
        // Within the skew allowance, check the chain at the certificate's start.
        let at = now.max(identity.not_before);
        let at = UnixTime::since_unix_epoch(Duration::from_millis(at.get()));
        match tls::check_client_certificate(&self.credentials.trust, identity, at) {
            Ok(Validity::Current) => Ok(()),
            Ok(Validity::OutsideNow) => Err(RenewalError::Rejected(format!(
                "its chain is not valid at {} by this clock",
                utc(now)
            ))),
            Err(error) => Err(RenewalError::Rejected(error.to_string())),
        }
    }

    /// Puts `identity` in place: the file, the inner TLS settings, the version and the expiry.
    fn install(&mut self, identity: Identity) -> Result<(), RenewalError> {
        let config = tls::inner_config(
            &self.credentials.trust,
            &self.credentials.root_crl,
            &identity,
        )
        .map_err(|error| RenewalError::Rejected(error.to_string()))?;
        if self.proven && !self.refused_in_use {
            // The certificate in use worked and the broker has not refused it: keep it to go back
            // to, should the broker refuse the renewed one before any connection with it lasted.
            self.keep_previous();
        }
        let now = (self.clock)();
        // When the file keeps the old certificate, a restart loads that one; renewal runs again
        // if it has to.
        let _saved = self.save(&identity, now);
        self.use_identity(identity, config, now);
        tracing::info!(
            gateway = %self.credentials.identity.gateway,
            not_before = %utc(self.credentials.identity.not_before),
            not_after = %utc(self.credentials.identity.not_after),
            "the client certificate was renewed"
        );
        Ok(())
    }

    /// Writes the certificate in use to [`PREVIOUS_SUFFIX`] and remembers it as the one to go
    /// back to. When the file cannot be written, only this process can go back to it.
    fn keep_previous(&mut self) {
        let path = previous_path(&self.credentials.certificate);
        let current = &self.credentials.identity;
        let modified = self.installed_at.unwrap_or_else(|| (self.clock)());
        // With the certificate file's permissions: it holds the same kind of content.
        let written = write_atomically_like(
            &path,
            pem_chain(&current.chain).as_bytes(),
            modified,
            &self.credentials.certificate,
        );
        // A rollback in this process needs only the certificate in memory; the file lets a
        // restart roll back too.
        self.previous = Some(current.clone());
        match written {
            Ok(()) => {}
            Err(error @ WriteError::NotDurable(_)) => tracing::warn!(
                path = %path.display(),
                %error,
                "the certificate in use was kept, but a power cut may still lose the copy"
            ),
            Err(error @ WriteError::NotReplaced(_)) => {
                tracing::warn!(
                    path = %path.display(),
                    %error,
                    "the certificate in use could not be kept on disk; after a restart a renewed one the broker refuses cannot be rolled back"
                );
                // Whatever an earlier run left there is not the certificate in use.
                remove_quietly(&path);
            }
        }
    }

    /// Writes `identity`'s chain over the certificate file, modified at `modified`.
    /// Returns whether the file durably holds `identity`.
    fn save(&self, identity: &Identity, modified: SystemTime) -> bool {
        let path = &self.credentials.certificate;
        match write_atomically(path, pem_chain(&identity.chain).as_bytes(), modified) {
            Ok(()) => true,
            Err(error @ WriteError::NotReplaced(_)) => {
                tracing::error!(
                    path = %path.display(),
                    %error,
                    "the certificate could not be saved; using it until the agent restarts, which loads the one in the file"
                );
                false
            }
            Err(error @ WriteError::NotDurable(_)) => {
                tracing::warn!(
                    path = %path.display(),
                    %error,
                    "the certificate was saved, but a power cut may still bring back the one before it"
                );
                false
            }
        }
    }

    /// Puts `identity` in use: the inner TLS settings, the expiry, the version. It is not proven.
    fn use_identity(
        &mut self,
        identity: Identity,
        config: Arc<rustls::ClientConfig>,
        at: SystemTime,
    ) {
        self.credentials.inner.replace(config);
        self.not_after.send_replace(identity.not_after);
        self.credentials.identity = identity;
        self.installed_at = Some(at);
        self.proven = false;
        self.refused_in_use = false;
        self.version.send_modify(|version| *version += 1);
    }

    /// A connection with the certificate in use lasted: it works. The previous one is no longer
    /// needed, and later renewals must start after this one.
    fn mark_proven(&mut self) {
        if self.proven {
            return;
        }
        self.proven = true;
        self.replay_floor = self.credentials.identity.not_before;
        if self.previous.take().is_some() {
            remove_quietly(&previous_path(&self.credentials.certificate));
        }
        tracing::info!(
            not_after = %utc(self.credentials.identity.not_after),
            "the client certificate in use is proven by a lasting connection"
        );
    }

    /// Goes back to the previous certificate when the one in use is not proven, the previous one
    /// is kept and still valid by this clock. Returns whether it did. The previous certificate
    /// counts as not proven again, so it is never rolled back from: no back and forth.
    fn roll_back(&mut self) -> bool {
        if self.proven {
            return false;
        }
        let Some(previous) = self.previous.clone() else {
            return false;
        };
        if let Err(error) = self.check_valid_now(&previous) {
            tracing::warn!(%error, "the previous certificate cannot be used any more; no rollback");
            return false;
        }
        let config = match tls::inner_config(
            &self.credentials.trust,
            &self.credentials.root_crl,
            &previous,
        ) {
            Ok(config) => config,
            Err(error) => {
                tracing::warn!(%error, "the previous certificate cannot be used; no rollback");
                return false;
            }
        };
        // Modified at its start, so the schedule (which waits from the modification time) treats
        // it as the certificate it was, not as a new one.
        let started = SystemTime::UNIX_EPOCH + Duration::from_millis(previous.not_before.get());
        // The kept copy goes only once the certificate file durably holds it again: otherwise a
        // restart would load the refused certificate with nothing to go back to. Kept, it lets a
        // restart roll back again.
        if self.save(&previous, started) {
            remove_quietly(&previous_path(&self.credentials.certificate));
        }
        self.previous = None;
        self.use_identity(previous, config, started);
        tracing::warn!(
            not_after = %utc(self.credentials.identity.not_after),
            "the broker refuses the renewed certificate before any lasting connection; went back to the previous one"
        );
        true
    }

    /// Renews on schedule and when the broker refuses the certificate, until `shutdown`. `link` is
    /// the link's state ([`crate::link::Link::state`]), `attempts` its connection attempts
    /// ([`crate::link::Link::attempts`]) and `refused` its refusals
    /// ([`crate::link::Link::certificate_refused`]).
    pub async fn run(
        mut self,
        mut link: watch::Receiver<LinkState>,
        attempts: watch::Receiver<Option<Attempt>>,
        mut refused: watch::Receiver<Option<Refusal>>,
        shutdown: CancellationToken,
    ) {
        let mut backoff = Backoff::new(self.settings.retry);
        // The earliest time of the next attempt.
        let mut retry_at: Option<Instant> = None;
        // After a renewal in this process: not before then by schedule (monotonic).
        let mut floor: Option<Instant> = None;
        // The certificate in use came from a renewal in this process and is not proven yet.
        let mut renewed = false;
        // A refusal of the certificate in use that no renewal or rollback has answered yet.
        let mut pending: Option<Refusal> = None;
        // When the last renewal that a refusal started was done.
        let mut last_refusal_renewal: Option<Instant> = None;
        // The connection that may prove the certificate in use, and since when this task has
        // seen it up: only one whose attempt presented the certificate in use, so not the one
        // that was up when the certificate changed. Looked at again on every pass, so a
        // reconnect while a new certificate goes in counts once its attempt says so.
        let mut connected: Option<(SessionId, Instant)> = None;
        tracing::info!(
            not_after = %utc(self.credentials.identity.not_after),
            due = %utc(self.due()),
            "certificate renewal is scheduled"
        );
        // Whether `refused` has a value not looked at yet (the one there at the start, too).
        let mut fresh_refusal = true;
        loop {
            let now = Instant::now();
            let version = *self.version.borrow();
            connected = match proving_session(&mut link, &attempts, version) {
                Some(session) => match connected {
                    Some((up, since)) if up == session => Some((up, since)),
                    _ => Some((session, now)),
                },
                None => None,
            };
            // A refusal of the certificate in use.
            let refusal = if std::mem::take(&mut fresh_refusal) {
                refused
                    .borrow_and_update()
                    .filter(|refusal| refusal.credentials >= version)
            } else {
                None
            };
            if let Some(refusal) = refusal
                && pending.is_none()
            {
                // Only when the broker said so: inner mTLS failures may be the broker's trouble.
                if refusal.explicit && !self.proven && self.roll_back() {
                    // The renewed certificate does not work and the previous one is back: the
                    // next renewal waits like after a failure.
                    renewed = false;
                    let delay = backoff.next_delay((self.jitter)());
                    retry_at = Some(now + delay);
                    floor = None;
                    continue;
                }
                pending = Some(refusal);
                self.refused_in_use = true;
                let wait = if renewed && !self.proven {
                    // The broker refuses the renewed certificate too: not again at once.
                    tracing::warn!("the broker refuses the renewed certificate as well");
                    Some(now + backoff.next_delay((self.jitter)()))
                } else if !refusal.explicit && self.check_valid_now(self.identity()).is_ok() {
                    // Only inferred, and valid by this clock: the broker may be the trouble.
                    Some(now + backoff.next_delay((self.jitter)()))
                } else if self.proven {
                    last_refusal_renewal.map(|at| at + REFUSAL_RENEWAL_INTERVAL)
                } else {
                    None
                };
                if let Some(at) = wait {
                    retry_at = Some(retry_at.map_or(at, |retry_at| retry_at.max(at)));
                }
            }
            let until_due = until(self.due(), (self.clock)());
            let scheduled = until_due.is_zero() && floor.is_none_or(|floor| now >= floor);
            let wanted = pending.is_some() || scheduled;
            if wanted && retry_at.is_none_or(|at| now >= at) {
                retry_at = None;
                tracing::info!(
                    refused = pending.is_some(),
                    "renewing the client certificate"
                );
                let attempt = tokio::select! {
                    () = shutdown.cancelled() => return,
                    attempt = self.renew() => attempt,
                };
                match attempt {
                    Ok(identity) => {
                        renewed = true;
                        let done = Instant::now();
                        floor = Some(done + share(&identity, self.settings.remaining_percent));
                        if pending.take().is_some() {
                            last_refusal_renewal = Some(done);
                        }
                    }
                    Err(error) => {
                        let delay = retry_delay(
                            &error,
                            &mut backoff,
                            (self.jitter)(),
                            self.settings.retry.cap,
                        );
                        tracing::warn!(
                            error = %printable(&error.to_string(), MAX_LOGGED_ERROR),
                            retry_in_s = delay.as_secs(),
                            failures = backoff.failures(),
                            "certificate renewal failed"
                        );
                        retry_at = Some(Instant::now() + delay);
                    }
                }
                continue;
            }
            // A connection that lasted proves the certificate in use.
            let proof = connected
                .as_ref()
                .filter(|_| !self.proven)
                .map(|(_, since)| *since + STABLE_AFTER);
            if proof.is_some_and(|at| now >= at) {
                self.mark_proven();
                if renewed {
                    renewed = false;
                    backoff.reset();
                }
                continue;
            }
            let mut wake = match (wanted, retry_at) {
                (true, Some(at)) => at,
                _ => {
                    let schedule = now + until_due.min(RECHECK);
                    let schedule = floor.map_or(schedule, |floor| schedule.max(floor));
                    retry_at.map_or(schedule, |at| schedule.max(at))
                }
            };
            if let Some(proof) = proof {
                wake = wake.min(proof);
            }
            tokio::select! {
                () = shutdown.cancelled() => return,
                () = tokio::time::sleep_until(wake) => {}
                changed = refused.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    // Read at the top of the loop.
                    fresh_refusal = true;
                }
                changed = link.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    // Read at the top of the loop.
                }
            }
        }
    }

    /// When the certificate in use is due by the gateway's clock: once
    /// `renewal.remaining_percent` of its validity is left, and not before the same share of its
    /// validity has passed since it was put in place (the file's modification time, so a clock
    /// running ahead does not renew again after every restart).
    fn due(&self) -> UnixMillis {
        let identity = &self.credentials.identity;
        let due = due_at(identity, self.settings.remaining_percent);
        let since_installed = self
            .installed_at
            .map(|at| at + share(identity, self.settings.remaining_percent))
            .and_then(UnixMillis::from_system_time);
        since_installed.map_or(due, |since_installed| due.max(since_installed))
    }
}

/// The session `link` is connected with, if its attempt presented credentials of `version` or
/// later: only such a connection can prove the certificate in use. The link reports an attempt
/// before its session can be Connected, so a Connected session without its attempt is an older
/// one and does not count.
fn proving_session(
    link: &mut watch::Receiver<LinkState>,
    attempts: &watch::Receiver<Option<Attempt>>,
    version: u64,
) -> Option<SessionId> {
    let LinkState::Connected { session } = &*link.borrow_and_update() else {
        return None;
    };
    attempts
        .borrow()
        .as_ref()
        .filter(|attempt| attempt.session == *session && attempt.credentials >= version)
        .map(|_| session.clone())
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

/// Why [`write_atomically`] did not finish.
#[derive(Debug, thiserror::Error)]
pub enum WriteError {
    /// A step before the rename failed: the file is unchanged.
    #[error("{0}; the file is unchanged")]
    NotReplaced(io::Error),
    /// The rename was done but the directory could not be synced: the file has the new contents,
    /// but a power cut before the system writes the directory may bring back the old ones.
    #[error("the file was replaced, but syncing its directory failed: {0}")]
    NotDurable(io::Error),
}

/// Replaces `path` with `contents` so that a power cut at any moment leaves either the old or the
/// new contents: a temporary file in the same directory with the old file's permissions (0600 if
/// there is none) and the modification time `modified`, fsync, rename over `path`, fsync of the
/// directory. A temporary file left by an earlier cut is overwritten.
///
/// # Errors
///
/// [`WriteError::NotReplaced`]: `path` is unchanged, and the temporary file removed if it can
/// be. [`WriteError::NotDurable`]: `path` has the new contents, not yet durably.
pub fn write_atomically(
    path: &Path,
    contents: &[u8],
    modified: SystemTime,
) -> Result<(), WriteError> {
    write_atomically_like(path, contents, modified, path)
}

/// [`write_atomically`] with the permissions of `like` (0600 if there is none) instead of
/// `path`'s.
fn write_atomically_like(
    path: &Path,
    contents: &[u8],
    modified: SystemTime,
    like: &Path,
) -> Result<(), WriteError> {
    let directory = path
        .parent()
        .filter(|directory| !directory.as_os_str().is_empty())
        .ok_or_else(|| WriteError::NotReplaced(io::Error::other("the path has no directory")))?;
    let name = path
        .file_name()
        .ok_or_else(|| WriteError::NotReplaced(io::Error::other("the path has no file name")))?;
    let mode = match fs::metadata(like) {
        Ok(metadata) => metadata.permissions().mode() & 0o777,
        Err(error) if error.kind() == io::ErrorKind::NotFound => 0o600,
        Err(error) => return Err(WriteError::NotReplaced(error)),
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
        // After the last write, which would set it to now; a rename keeps it.
        file.set_modified(modified)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, path)
    })();
    if let Err(error) = written {
        let _ = fs::remove_file(&temporary);
        return Err(WriteError::NotReplaced(error));
    }
    // The rename is durable only once the directory entry is on the disk.
    File::open(directory)
        .and_then(|directory| directory.sync_all())
        .map_err(WriteError::NotDurable)
}

/// Where the last proven certificate chain is kept next to `certificate`.
fn previous_path(certificate: &Path) -> PathBuf {
    let mut name = certificate.as_os_str().to_os_string();
    name.push(PREVIOUS_SUFFIX);
    PathBuf::from(name)
}

/// Removes `path`; a file that is not there is fine, other failures are logged.
fn remove_quietly(path: &Path) {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => tracing::warn!(path = %path.display(), %error, "could not remove the file"),
    }
}

/// The certificate kept in [`PREVIOUS_SUFFIX`] by an earlier run, if it is one this gateway could
/// go back to: its chain with the key in use, this gateway's, a client certificate under
/// `broker.ca` (whatever the time; that is checked before a rollback) and older than the one in
/// use. Anything else is removed and logged.
fn load_previous(credentials: &Credentials) -> Option<Identity> {
    let path = previous_path(&credentials.certificate);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return None,
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "the previous certificate cannot be read; ignoring it");
            return None;
        }
    };
    let current = &credentials.identity;
    let previous = certificates(&text)
        .and_then(|chain| {
            identity::with_key(&path, chain, Arc::clone(&current.key))
                .map_err(|error| error.to_string())
        })
        .and_then(|previous| {
            if previous.gateway != current.gateway {
                return Err(format!("it is for {}, not this gateway", previous.gateway));
            }
            if previous.not_before >= current.not_before {
                return Err("it is not older than the certificate in use".to_owned());
            }
            tls::check_client_certificate(&credentials.trust, &previous, UnixTime::now())
                .map_err(|error| error.to_string())?;
            Ok(previous)
        });
    match previous {
        Ok(previous) => {
            tracing::info!(
                not_after = %utc(previous.not_after),
                "a previous certificate is kept until the one in use is proven"
            );
            Some(previous)
        }
        Err(reason) => {
            tracing::warn!(path = %path.display(), reason = %printable(&reason, MAX_LOGGED_ERROR), "the previous certificate cannot be used; removing it");
            remove_quietly(&path);
            None
        }
    }
}

/// The most characters of an error that came from the endpoint's answer put in a log line.
const MAX_LOGGED_ERROR: usize = 512;

/// `at` for the certificate check; a time before 1970 is "too early".
#[cfg(test)]
fn unix_time(at: SystemTime) -> UnixTime {
    UnixTime::since_unix_epoch(
        at.duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default(),
    )
}

#[cfg(test)]
mod tests;
