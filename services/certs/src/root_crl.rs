//! The root CRL (services/certs/README.md, 「根 CRL」; Laysi/trustiot-v2#830): the offline root
//! CA's CRL of revoked issuing CAs, which the chart puts beside the root certificate in the CA
//! certificates ConfigMap (`certificateAuthority.rootCrl`). The service adopts it only after the
//! checks the gateway agent applies (`crates/root-crl`, against the root certificate it loaded),
//! keeps it with the authority it was checked against ([`crate::authority::CaState`]), and hands
//! it to gateways (Laysi/trustiot-v2#831, #832) through [`crate::authority::CaState::root_crl`].
//!
//! An expired root CRL is still adopted: gateways keep checking with it (README, 「過期」). Its
//! expiry raises the alert instead ([`ALERT_BEFORE_SECS`], [`crate::monitor`]).

use std::cmp::Ordering;
use std::fmt;
use std::io::Read as _;
use std::path::Path;

use rustls_pki_types::CertificateDer;
use sha2::{Digest, Sha256};

use crate::pem;

/// A root CRL with less than this left raises the alert (60 days): the root ceremony that signs
/// the next one needs the operators at the offline machine, so the alert comes well ahead.
pub const ALERT_BEFORE_SECS: i64 = 60 * 86_400;

/// A root CRL the service adopted: signed by the loaded root and in the format the agent
/// accepts, with a CRL number and a nextUpdate.
#[derive(Clone, PartialEq, Eq)]
pub struct RootCrl {
    der: Vec<u8>,
    pem: String,
    number: String,
    this_update: i64,
    next_update: i64,
    root_sha256: [u8; 32],
}

impl fmt::Debug for RootCrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RootCrl")
            .field("number", &self.number)
            .field("this_update", &self.this_update)
            .field("next_update", &self.next_update)
            .finish_non_exhaustive()
    }
}

impl RootCrl {
    /// Checks the root CRL in `bytes` (a file's content) against the root certificate `root_der`
    /// (read from `source`, which an error names when the CRL's issuer is another): the agent's
    /// checks (`crates/root-crl`), and a CRL number and
    /// a nextUpdate, which the agent does not require but the gateways' choice of the newest
    /// root CRL and the alert need.
    ///
    /// # Errors
    ///
    /// Why it cannot be adopted; names no data but the CRL's issuer, shown without control
    /// characters.
    pub fn check(bytes: &[u8], root_der: &[u8], source: &Path) -> Result<Self, String> {
        let certificates = [CertificateDer::from(root_der.to_vec())];
        let checked = ::root_crl::check(
            bytes,
            &::root_crl::Anchors {
                certificates: &certificates,
                store: &::root_crl::store(&certificates)?,
                source,
            },
        )?;
        let number = checked.number.ok_or_else(|| {
            "it has no CRL number, by which gateways keep the newest root CRL".to_owned()
        })?;
        let next_update = checked
            .next_update
            .ok_or_else(|| "it has no nextUpdate, so its expiry could not alert".to_owned())?;
        let der = checked.der.to_vec();
        // Gateways get this PEM, in lines of 64, not the file: a file with the base64 on fewer
        // lines can be under the agent's limit while the PEM handed out is over it, and the agent
        // refuses to start with that.
        let pem = pem::encode(pem::CRL, &der);
        if u64::try_from(pem.len()).unwrap_or(u64::MAX) > ::root_crl::MAX_BYTES {
            return Err(format!(
                "its PEM in lines of 64, as gateways get it, is larger than {} bytes",
                ::root_crl::MAX_BYTES
            ));
        }
        Ok(Self {
            pem,
            der,
            number,
            this_update: checked.this_update,
            next_update,
            root_sha256: Sha256::digest(root_der).into(),
        })
    }

    /// The CRL, DER.
    #[must_use]
    pub fn der(&self) -> &[u8] {
        &self.der
    }

    /// The CRL as one PEM block `X509 CRL`, ending in a newline: what gateways load.
    #[must_use]
    pub fn pem(&self) -> &str {
        &self.pem
    }

    /// Its CRL number, in decimal.
    #[must_use]
    pub fn number(&self) -> &str {
        &self.number
    }

    /// Its thisUpdate, Unix seconds.
    #[must_use]
    pub fn this_update(&self) -> i64 {
        self.this_update
    }

    /// Its nextUpdate, Unix seconds.
    #[must_use]
    pub fn next_update(&self) -> i64 {
        self.next_update
    }

    /// Seconds until its nextUpdate at `now`, negative once it has passed.
    #[must_use]
    pub fn remaining(&self, now: i64) -> i64 {
        self.next_update.saturating_sub(now)
    }

    /// Whether its nextUpdate has passed at `now`. It is still adopted.
    #[must_use]
    pub fn expired(&self, now: i64) -> bool {
        self.next_update <= now
    }

    /// Whether the alert holds at `now`: less than [`ALERT_BEFORE_SECS`] left, or expired.
    #[must_use]
    pub fn expiring(&self, now: i64) -> bool {
        self.remaining(now) < ALERT_BEFORE_SECS
    }

    /// Whether it was checked against the root certificate `root_der`.
    #[must_use]
    pub fn checked_against(&self, root_der: &[u8]) -> bool {
        self.root_sha256 == <[u8; 32]>::from(Sha256::digest(root_der))
    }

    /// How its CRL number compares with `other`'s (numbers of any size, as decimal strings).
    #[must_use]
    pub fn number_cmp(&self, other: &Self) -> Ordering {
        let a = self.number.trim_start_matches('0');
        let b = other.number.trim_start_matches('0');
        a.len().cmp(&b.len()).then_with(|| a.cmp(b))
    }
}

/// The most characters of a problem with the root CRL file that the service keeps and logs: a
/// parse error can quote the file, which the monitor would otherwise log in full at every check.
pub const MAX_PROBLEM_CHARS: usize = 1024;

/// `problem` cut to [`MAX_PROBLEM_CHARS`].
pub(crate) fn bounded(problem: String) -> String {
    if problem.chars().nth(MAX_PROBLEM_CHARS).is_none() {
        return problem;
    }
    let mut cut: String = problem.chars().take(MAX_PROBLEM_CHARS).collect();
    cut.push_str(" (cut short)");
    cut
}

/// What the root CRL file held when it was read.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RootCrlFile {
    /// Empty or only whitespace: no root CRL yet (the chart writes an empty file while
    /// `certificateAuthority.rootCrl` is empty).
    Empty,
    /// Content to check.
    Content(Vec<u8>),
}

/// Reads at most [`::root_crl::MAX_BYTES`] of `path`.
///
/// # Errors
///
/// When it cannot be read or is larger.
pub(crate) fn read(path: &Path) -> Result<RootCrlFile, String> {
    let file = std::fs::File::open(path)
        .map_err(|error| format!("{}: cannot open it: {error}", path.display()))?;
    let mut bytes = Vec::new();
    file.take(::root_crl::MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("{}: cannot read it: {error}", path.display()))?;
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(RootCrlFile::Empty);
    }
    Ok(RootCrlFile::Content(bytes))
}
