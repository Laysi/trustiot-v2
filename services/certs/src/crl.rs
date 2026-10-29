//! Each issuing CA's CRL (decision 6; Laysi/trustiot-v2#78 acceptance 2): regenerated on a
//! schedule, kept in `certs.crls`, and published to whoever fetches `crl.pem` (the MQTT broker,
//! Laysi/trustiot-v2#84).
//!
//! Mosquitto checks every gateway certificate against its issuer's CRL and rejects every gateway
//! of an issuing CA once that CA's CRL has expired, so the CRL must never lapse: every
//! `crl.check_interval` the replica that holds the job lock (`service_kit::job_lock`) gives every
//! issuing CA whose CRL has less than `crl.renew_before` left (or none) a new one, valid for
//! `crl.validity`, with the next CRL number. The monitor ([`crate::monitor`]) raises the alert
//! when one has less than `crl.alert_before` left.
//!
//! A revocation must reach the broker without waiting for that schedule (Laysi/trustiot-v2#635):
//! once a revocation commits, its caller gives the certificate's issuing CA a new CRL at once
//! ([`Reissuer::after_revocation`], built on [`reissue`]). When that fails (the database is down,
//! the replica stops), the revocation stands and the job catches up: a CA also gets a new CRL
//! when one of its certificates was revoked after its current CRL's thisUpdate, so the
//! revocation is listed within one `crl.check_interval` on whichever replica holds the job lock,
//! restarted or not.
//!
//! A new CRL's nextUpdate lies `crl.validity` after the round that makes it and its thisUpdate
//! [`CLOCK_SKEW_SECS`] before it, as a gateway certificate's notBefore does, so a broker whose
//! clock runs a little behind does not see a CRL that is not valid yet (OpenSSL would then refuse
//! every gateway of the CA). It lists every revoked certificate of its CA that has not expired
//! at the round, with the revocation time and, when recorded, the reason. Its issuer is the issuing CA certificate's
//! subject and its authority key identifier is that certificate's subject key identifier, which
//! is how OpenSSL matches it to the certificates the CA signed.
//!
//! Every new CRL of a CA, scheduled or after a revocation, is made in one transaction that takes
//! [`LOCK_CA_CRL`] and locks the CA's row before it reads the current number and the revoked
//! certificates, and the upsert never lowers the number. So two of them never interleave: the
//! later one reads what the earlier one wrote and every revocation committed before it, and its
//! number is one higher.

use std::time::Duration;

use chrono::{DateTime, Utc};
use rcgen::{
    CertificateRevocationListParams, KeyIdMethod, RevocationReason, RevokedCertParams, SerialNumber,
};
use service_kit::Context;
use service_kit::job_lock::JobLock;
use service_kit::tenant::{BackgroundTenantDb, TenantDbError};
use sqlx::{PgConnection, PgPool, Row as _};
use tracing::Instrument as _;

use crate::authority::{Authority, CaState, IssuingCa, unix_now};
use crate::config::CrlSchedule;
use crate::issue::{CLOCK_SKEW_SECS, hex, timestamp, utc};
use crate::pem;

/// The job's name for its advisory lock and its log lines.
pub const JOB: &str = "crl";

/// Serializes the new CRLs of one issuing CA (`$1`, its key id) across replicas, held until the
/// transaction ends. The row lock alone does not cover a CA that has no CRL yet; replicas of an
/// older release, which take only the row lock, still serialize with this one once the row
/// exists. Class `0x63726c73` ("crls"); the CA key id is hashed into the second key.
pub const LOCK_CA_CRL: &str = "SELECT pg_advisory_xact_lock(1668443251, hashtext($1))";

/// How long [`Reissuer::after_revocation`] waits at most, at most half the API port's handler
/// timeout: a CRL job round that holds the CA's lock delays it, a stuck one must not keep the
/// revocation's response from going out.
pub const REISSUE_TIMEOUT: Duration = Duration::from_secs(10);

/// Why a round failed. Names CAs and problems, never key material.
#[derive(Debug, thiserror::Error)]
pub enum CrlError {
    #[error("no issuing CA is loaded")]
    NoAuthority,
    #[error("database: {0}")]
    Database(#[from] sqlx::Error),
    #[error("database session: {0}")]
    Session(#[from] TenantDbError),
    #[error("a revoked certificate of CA {ca_key_id} has an unknown reason code {code}")]
    Reason { ca_key_id: String, code: i16 },
    #[error("cannot sign the CRL of CA {ca_key_id}: {problem}")]
    Sign { ca_key_id: String, problem: String },
    #[error("CA {ca_key_id} is not a loaded issuing CA")]
    UnknownCa { ca_key_id: String },
    #[error("a CRL of CA {ca_key_id} with a higher number was stored meanwhile")]
    Superseded { ca_key_id: String },
}

/// Why a CA got a new CRL, as the log line names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cause {
    /// The CA had no CRL.
    First,
    /// Its CRL had less than `crl.renew_before` left.
    Expiring,
    /// The job found a certificate of the CA revoked after its CRL's thisUpdate: the new CRL
    /// right after that revocation failed or has not happened yet.
    Unlisted,
    /// Right after a revocation ([`reissue`]).
    Revocation,
}

impl Cause {
    /// The name in the log line's `cause` field.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::First => "first",
            Self::Expiring => "expiring",
            Self::Unlisted => "unlisted_revocation",
            Self::Revocation => "revocation",
        }
    }
}

/// A revoked certificate as a CRL lists it.
#[derive(Clone, Debug)]
pub struct Revoked {
    pub serial: Vec<u8>,
    /// Unix seconds.
    pub revoked_at: i64,
    /// RFC 5280 CRLReason code.
    pub reason: Option<i16>,
}

/// The CRL of `ca` number `number` listing `revoked`, valid from `this_update` (Unix seconds)
/// for `validity`, DER.
///
/// # Errors
///
/// When a reason code is unknown or signing fails.
pub fn build(
    ca: &IssuingCa,
    number: u64,
    revoked: &[Revoked],
    this_update: i64,
    validity: Duration,
) -> Result<Vec<u8>, CrlError> {
    let sign_error = |problem: String| CrlError::Sign {
        ca_key_id: ca.key_id().to_owned(),
        problem,
    };
    let next_update =
        this_update.saturating_add(i64::try_from(validity.as_secs()).unwrap_or(i64::MAX));
    let mut revoked_certs = Vec::with_capacity(revoked.len());
    for entry in revoked {
        let reason_code = entry
            .reason
            .map(|code| {
                reason(code).ok_or_else(|| CrlError::Reason {
                    ca_key_id: ca.key_id().to_owned(),
                    code,
                })
            })
            .transpose()?;
        revoked_certs.push(RevokedCertParams {
            serial_number: SerialNumber::from_slice(&entry.serial),
            revocation_time: timestamp(entry.revoked_at).map_err(|e| sign_error(e.to_string()))?,
            reason_code,
            invalidity_date: None,
        });
    }
    let params = CertificateRevocationListParams {
        this_update: timestamp(this_update).map_err(|e| sign_error(e.to_string()))?,
        next_update: timestamp(next_update).map_err(|e| sign_error(e.to_string()))?,
        crl_number: SerialNumber::from(number),
        issuing_distribution_point: None,
        revoked_certs,
        key_identifier_method: KeyIdMethod::PreSpecified(ca.subject_key_id().to_vec()),
    };
    params
        .signed_by(ca.issuer())
        .map(|crl| crl.der().to_vec())
        .map_err(|error| sign_error(error.to_string()))
}

fn reason(code: i16) -> Option<RevocationReason> {
    Some(match code {
        0 => RevocationReason::Unspecified,
        1 => RevocationReason::KeyCompromise,
        2 => RevocationReason::CaCompromise,
        3 => RevocationReason::AffiliationChanged,
        4 => RevocationReason::Superseded,
        5 => RevocationReason::CessationOfOperation,
        6 => RevocationReason::CertificateHold,
        8 => RevocationReason::RemoveFromCrl,
        9 => RevocationReason::PrivilegeWithdrawn,
        10 => RevocationReason::AaCompromise,
        _ => return None,
    })
}

/// Starts the CRL job: every `schedule.check_interval`, on whichever replica gets the job lock,
/// [`refresh`] with the authority as loaded then.
pub fn start<T>(
    context: &Context<T>,
    pool: PgPool,
    db: BackgroundTenantDb,
    ca: CaState,
    schedule: CrlSchedule,
) {
    let lock = JobLock::new(pool, "certs", JOB);
    let shutdown = context.shutdown().clone();
    context.spawn(async move {
        lock.run_periodically(schedule.check_interval, shutdown, move || {
            let (db, ca, schedule) = (db.clone(), ca.clone(), schedule.clone());
            async move {
                let authority = ca.authority().ok_or(CrlError::NoAuthority)?;
                refresh(&db, &authority, &schedule, unix_now())
                    .await
                    .map(drop)
            }
        })
        .await;
    });
}

/// One round at `now` (Unix seconds): every issuing CA whose certificate has not expired gets a
/// new CRL with the next number when it has none, when its CRL has less than
/// `schedule.renew_before` left, or when one of its certificates that has not expired was
/// revoked after its CRL's thisUpdate ([`Cause::Unlisted`]). Returns the key ids of the CAs that
/// got one.
///
/// The last condition is how a revocation whose immediate CRL ([`reissue`]) failed reaches the
/// CRL within one round. It compares with thisUpdate, which lies [`CLOCK_SKEW_SECS`] before the
/// round that made the CRL, so a revocation that committed just after that round read the
/// revoked certificates is caught even when the database's clock (which sets `revoked_at`) and
/// this replica's differ by up to that much. The price: for that long after a revocation, every
/// round makes one more CRL of its CA that lists nothing new.
///
/// Each CA's CRL is replaced in a transaction of its own (see the module documentation), so a
/// round that overlaps another or a [`reissue`] (the job lock bounds the former, see
/// `service_kit::job_lock`) waits for it, skips or repeats work, but never goes back.
///
/// # Errors
///
/// When the database fails or a CRL cannot be signed; the CAs before it keep their new CRL.
pub async fn refresh(
    db: &BackgroundTenantDb,
    authority: &Authority,
    schedule: &CrlSchedule,
    now: i64,
) -> Result<Vec<String>, CrlError> {
    let renew_before = i64::try_from(schedule.renew_before.as_secs()).unwrap_or(i64::MAX);
    let mut renewed = Vec::new();
    for ca in authority.issuing() {
        if ca.not_after() <= now {
            continue;
        }
        let mut tx = db.begin_without_tenant("crl-refresh").await?;
        let current = lock_current(&mut tx, ca.key_id()).await?;
        let cause = match &current {
            None => Cause::First,
            Some(current) if current.next_update.saturating_sub(now) <= renew_before => {
                Cause::Expiring
            }
            Some(current) => {
                if revoked_since(&mut tx, ca.key_id(), current.this_update, now).await? {
                    Cause::Unlisted
                } else {
                    continue;
                }
            }
        };
        let number = current.as_ref().map_or(0, |current| current.number);
        let issued = issue_locked(&mut tx, ca, number, schedule, now).await?;
        tx.commit().await?;
        if issued.stored {
            log_issued(ca, &issued, cause);
            renewed.push(ca.key_id().to_owned());
        }
    }
    Ok(renewed)
}

/// What [`reissue`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reissued {
    /// The CA has a new CRL with this number, listing this many revoked certificates.
    Issued { crl_number: i64, revoked: usize },
    /// The CA's certificate has expired at `now`, so has every certificate it signed: nothing
    /// to list, and the job no longer renews its CRL either.
    CaExpired,
}

/// Gives the issuing CA `ca_key_id` of `authority` a new CRL at `now` (Unix seconds) with the
/// next number, whatever its current CRL's nextUpdate: what a caller does right after its
/// revocation commits, so the CRL the broker fetches lists it without waiting for the schedule
/// (Laysi/trustiot-v2#635). The CRL follows the same rules as [`refresh`]'s: thisUpdate
/// [`CLOCK_SKEW_SECS`] before `now`, nextUpdate `schedule.validity` after it, every revoked
/// certificate of the CA that has not expired at `now`. It runs in its own transaction, so it
/// sees every revocation committed before it began; call it after the revocation's commit,
/// never inside that transaction.
///
/// Each wait for one of the CA's locks (a round of the CRL job or another reissue holds them
/// while it makes its CRL) gives up after `lock_wait` (Postgres `lock_timeout`), so a stuck
/// holder fails this call with a database error instead of keeping its connection waiting. A CA
/// with no CRL yet gets its first one. [`Reissuer::after_revocation`] wraps this for request
/// handlers.
///
/// # Errors
///
/// When the CA is not one of `authority`'s issuing CAs, the database fails or a lock wait
/// times out, the CRL cannot be signed, or a CRL with a higher number appeared meanwhile (only a
/// replica that takes neither lock could do that).
pub async fn reissue(
    db: &BackgroundTenantDb,
    authority: &Authority,
    ca_key_id: &str,
    schedule: &CrlSchedule,
    now: i64,
    lock_wait: Duration,
) -> Result<Reissued, CrlError> {
    let ca = authority
        .issuing()
        .iter()
        .find(|ca| ca.key_id() == ca_key_id)
        .ok_or_else(|| CrlError::UnknownCa {
            ca_key_id: ca_key_id.to_owned(),
        })?;
    if ca.not_after() <= now {
        return Ok(Reissued::CaExpired);
    }
    let mut tx = db.begin_without_tenant("crl-reissue").await?;
    // Postgres reads at least 1 ms here; a value over its maximum (about 24 days) is clamped.
    let lock_wait_ms = lock_wait
        .as_millis()
        .clamp(1, u128::from(i32::MAX.unsigned_abs()));
    sqlx::query("SELECT set_config('lock_timeout', $1, true)")
        .bind(format!("{lock_wait_ms}ms"))
        .execute(&mut *tx)
        .await?;
    let number = lock_current(&mut tx, ca_key_id)
        .await?
        .map_or(0, |current| current.number);
    let issued = issue_locked(&mut tx, ca, number, schedule, now).await?;
    if !issued.stored {
        return Err(CrlError::Superseded {
            ca_key_id: ca_key_id.to_owned(),
        });
    }
    tx.commit().await?;
    log_issued(ca, &issued, Cause::Revocation);
    Ok(Reissued::Issued {
        crl_number: issued.number,
        revoked: issued.revoked.len(),
    })
}

/// A CA's current CRL, as the transaction that replaces it reads it.
struct Current {
    number: i64,
    /// Unix seconds.
    this_update: i64,
    /// Unix seconds.
    next_update: i64,
}

/// Takes [`LOCK_CA_CRL`] for `ca_key_id`, then locks its row and reads it: `None` when the CA
/// has no CRL yet.
async fn lock_current(tx: &mut PgConnection, ca_key_id: &str) -> Result<Option<Current>, CrlError> {
    sqlx::query(LOCK_CA_CRL)
        .bind(ca_key_id)
        .execute(&mut *tx)
        .await?;
    let row = sqlx::query(
        "SELECT crl_number, this_update, next_update FROM certs.crls
         WHERE ca_key_id = $1 FOR UPDATE",
    )
    .bind(ca_key_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let this_update: DateTime<Utc> = row.try_get("this_update")?;
    let next_update: DateTime<Utc> = row.try_get("next_update")?;
    Ok(Some(Current {
        number: row.try_get("crl_number")?,
        this_update: this_update.timestamp(),
        next_update: next_update.timestamp(),
    }))
}

/// Whether a certificate of `ca_key_id` that has not expired at `now` was revoked after `since`
/// (Unix seconds). Reads the partial index `issued_certificates_revoked`.
async fn revoked_since(
    tx: &mut PgConnection,
    ca_key_id: &str,
    since: i64,
    now: i64,
) -> Result<bool, CrlError> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (
             SELECT 1 FROM certs.issued_certificates
             WHERE ca_key_id = $1 AND revoked_at IS NOT NULL AND not_after > $2
               AND revoked_at > $3
         )",
    )
    .bind(ca_key_id)
    .bind(utc(now))
    .bind(utc(since))
    .fetch_one(&mut *tx)
    .await?)
}

/// A CRL [`issue_locked`] made.
struct Issued {
    number: i64,
    /// Unix seconds.
    next_update: i64,
    revoked: Vec<Revoked>,
    /// Whether the upsert stored it: false only when a CRL with a higher number was already there.
    stored: bool,
}

/// Signs and upserts the CRL of `ca` after `number` at `now`, in `tx`, which holds the CA's
/// locks ([`lock_current`]).
async fn issue_locked(
    tx: &mut PgConnection,
    ca: &IssuingCa,
    number: i64,
    schedule: &CrlSchedule,
    now: i64,
) -> Result<Issued, CrlError> {
    let rows = sqlx::query(
        "SELECT serial, revoked_at, revocation_reason FROM certs.issued_certificates
         WHERE ca_key_id = $1 AND revoked_at IS NOT NULL AND not_after > $2
         ORDER BY serial",
    )
    .bind(ca.key_id())
    .bind(utc(now))
    .fetch_all(&mut *tx)
    .await?;
    let mut revoked = Vec::with_capacity(rows.len());
    for row in rows {
        let revoked_at: DateTime<Utc> = row.try_get("revoked_at")?;
        revoked.push(Revoked {
            serial: row.try_get("serial")?,
            revoked_at: revoked_at.timestamp(),
            reason: row.try_get("revocation_reason")?,
        });
    }
    let next = number.saturating_add(1);
    let this_update = now.saturating_sub(CLOCK_SKEW_SECS);
    let next_update =
        now.saturating_add(i64::try_from(schedule.validity.as_secs()).unwrap_or(i64::MAX));
    let lifetime =
        Duration::from_secs(u64::try_from(next_update.saturating_sub(this_update)).unwrap_or(0));
    let der = build(
        ca,
        u64::try_from(next).unwrap_or(1),
        &revoked,
        this_update,
        lifetime,
    )?;
    let stored = sqlx::query(
        "INSERT INTO certs.crls (ca_key_id, crl_number, this_update, next_update, crl)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (ca_key_id) DO UPDATE SET
             crl_number = EXCLUDED.crl_number,
             this_update = EXCLUDED.this_update,
             next_update = EXCLUDED.next_update,
             crl = EXCLUDED.crl
         WHERE certs.crls.crl_number < EXCLUDED.crl_number",
    )
    .bind(ca.key_id())
    .bind(next)
    .bind(utc(this_update))
    .bind(utc(next_update))
    .bind(&der)
    .execute(&mut *tx)
    .await?
    .rows_affected()
        == 1;
    Ok(Issued {
        number: next,
        next_update,
        revoked,
        stored,
    })
}

fn log_issued(ca: &IssuingCa, issued: &Issued, cause: Cause) {
    tracing::info!(
        ca_key_id = %ca.key_id(),
        generation = ca.generation(),
        crl_number = issued.number,
        next_update = issued.next_update,
        cause = cause.as_str(),
        revoked = issued.revoked.len(),
        revoked_serials = ?issued.revoked.iter().map(|entry| hex(&entry.serial)).collect::<Vec<_>>(),
        "issued a new CRL"
    );
}

/// [`reissue`] for request handlers: the CA state, the service role and the schedule a handler
/// needs to give an issuing CA a new CRL right after it revoked one of its certificates.
/// Cloning is cheap.
#[derive(Clone)]
pub struct Reissuer {
    db: BackgroundTenantDb,
    ca: CaState,
    schedule: CrlSchedule,
    timeout: Duration,
}

impl Reissuer {
    /// New CRLs written with the service role's `db`, for the CAs of `ca` as loaded at the time,
    /// valid as `schedule` says; each attempt bounded by `timeout` (the caller passes
    /// [`REISSUE_TIMEOUT`], or less when its own deadline is shorter).
    #[must_use]
    pub fn new(
        db: BackgroundTenantDb,
        ca: CaState,
        schedule: CrlSchedule,
        timeout: Duration,
    ) -> Self {
        Self {
            db,
            ca,
            schedule,
            timeout,
        }
    }

    /// Call right after the revocation of `serial`, signed by `ca_key_id`, committed: gives the
    /// CA a new CRL now ([`reissue`]) and returns whether the CA's CRL is now up to date (also
    /// true for a CA that has expired, which needs none). It runs on a task of its own, in the
    /// caller's span, so a caller that goes away (a client that disconnects) does not cancel it,
    /// and gives up after the timeout. Each of its two lock waits in Postgres gives up after a
    /// third of the timeout, so a stuck lock ends the statement in the database before the
    /// timeout and its connection returns to the pool. A failure is logged as an ERROR and
    /// changes nothing for the revocation, which stands: the CRL job lists it within one
    /// `crl.check_interval` ([`refresh`], [`Cause::Unlisted`]). Write the revocation's own log
    /// line before calling this.
    pub async fn after_revocation(&self, ca_key_id: &str, serial: &[u8]) -> bool {
        let (db, ca, schedule, timeout) = (
            self.db.clone(),
            self.ca.clone(),
            self.schedule.clone(),
            self.timeout,
        );
        let key_id = ca_key_id.to_owned();
        let outcome = tokio::spawn(
            async move {
                let authority = ca.authority().ok_or(CrlError::NoAuthority)?;
                match tokio::time::timeout(
                    timeout,
                    reissue(&db, &authority, &key_id, &schedule, unix_now(), timeout / 3),
                )
                .await
                {
                    Ok(result) => result.map(Some),
                    Err(_) => Ok(None),
                }
            }
            .instrument(tracing::Span::current()),
        )
        .await;
        let problem = match outcome {
            Ok(Ok(Some(Reissued::Issued { .. } | Reissued::CaExpired))) => return true,
            Ok(Ok(None)) => format!("timed out after {timeout:?}"),
            Ok(Err(error)) => error.to_string(),
            Err(error) => format!("the task failed: {error}"),
        };
        tracing::error!(
            ca_key_id,
            serial = %hex(serial),
            error = %problem,
            check_interval = ?self.schedule.check_interval,
            "could not issue a new CRL after a revocation; the revocation stands and the CRL job \
             lists it within one check interval"
        );
        false
    }
}

/// The current CRL (DER) of every issuing CA of `authority` whose certificate has not expired at
/// `now`, in the authority's order; `Err` with the key ids of the CAs that have none yet.
///
/// # Errors
///
/// When the database fails, or with `Ok(Err(missing))` when a CA has no CRL.
pub async fn current(
    pool: &PgPool,
    authority: &Authority,
    now: i64,
) -> Result<Result<Vec<Vec<u8>>, Vec<String>>, sqlx::Error> {
    let ids: Vec<String> = authority
        .issuing()
        .iter()
        .filter(|ca| ca.not_after() > now)
        .map(|ca| ca.key_id().to_owned())
        .collect();
    let rows = sqlx::query("SELECT ca_key_id, crl FROM certs.crls WHERE ca_key_id = ANY($1)")
        .bind(&ids)
        .fetch_all(pool)
        .await?;
    let mut crls = Vec::with_capacity(ids.len());
    let mut missing = Vec::new();
    for id in &ids {
        let found = rows
            .iter()
            .find(|row| row.try_get::<String, _>("ca_key_id").ok().as_deref() == Some(id));
        match found {
            Some(row) => crls.push(row.try_get::<Vec<u8>, _>("crl")?),
            None => missing.push(id.clone()),
        }
    }
    Ok(if missing.is_empty() {
        Ok(crls)
    } else {
        Err(missing)
    })
}

/// `crls` as one PEM file, as Mosquitto's `crlfile` reads it.
#[must_use]
pub fn pem_bundle(crls: &[Vec<u8>]) -> String {
    crls.iter().map(|crl| pem::encode(pem::CRL, crl)).collect()
}
