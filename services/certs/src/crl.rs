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
//! A new CRL's nextUpdate lies `crl.validity` after the round that makes it and its thisUpdate
//! [`CLOCK_SKEW_SECS`] before it, as a gateway certificate's notBefore does, so a broker whose
//! clock runs a little behind does not see a CRL that is not valid yet (OpenSSL would then refuse
//! every gateway of the CA). It lists every revoked certificate of its CA that has not expired
//! at the round, with the revocation time and, when recorded, the reason. Its issuer is the issuing CA certificate's
//! subject and its authority key identifier is that certificate's subject key identifier, which
//! is how OpenSSL matches it to the certificates the CA signed.

use std::time::Duration;

use chrono::{DateTime, Utc};
use rcgen::{
    CertificateRevocationListParams, KeyIdMethod, RevocationReason, RevokedCertParams, SerialNumber,
};
use service_kit::Context;
use service_kit::job_lock::JobLock;
use service_kit::tenant::{BackgroundTenantDb, TenantDbError};
use sqlx::{PgPool, Row as _};

use crate::authority::{Authority, CaState, IssuingCa, unix_now};
use crate::config::CrlSchedule;
use crate::issue::{CLOCK_SKEW_SECS, hex, timestamp, utc};
use crate::pem;

/// The job's name for its advisory lock and its log lines.
pub const JOB: &str = "crl";

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

/// One round at `now` (Unix seconds): every issuing CA whose certificate has not expired and
/// whose CRL has less than `schedule.renew_before` left, or none, gets a new CRL with the next
/// number. Returns the key ids of the CAs that got one.
///
/// Each CA's CRL is replaced in a transaction of its own that locks its row, and an upsert never
/// lowers the number, so a round that overlaps another (the job lock bounds that, see
/// `service_kit::job_lock`) skips or repeats work but never goes back.
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
        let current = sqlx::query(
            "SELECT crl_number, next_update FROM certs.crls WHERE ca_key_id = $1 FOR UPDATE",
        )
        .bind(ca.key_id())
        .fetch_optional(&mut *tx)
        .await?;
        let (number, remaining) = match &current {
            Some(row) => {
                let number: i64 = row.try_get("crl_number")?;
                let next_update: DateTime<Utc> = row.try_get("next_update")?;
                (number, Some(next_update.timestamp().saturating_sub(now)))
            }
            None => (0, None),
        };
        if remaining.is_some_and(|remaining| remaining > renew_before) {
            continue;
        }
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
        let lifetime = Duration::from_secs(
            u64::try_from(next_update.saturating_sub(this_update)).unwrap_or(0),
        );
        let der = build(
            ca,
            u64::try_from(next).unwrap_or(1),
            &revoked,
            this_update,
            lifetime,
        )?;
        sqlx::query(
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
        .await?;
        tx.commit().await?;
        tracing::info!(
            ca_key_id = %ca.key_id(),
            generation = ca.generation(),
            crl_number = next,
            next_update,
            revoked = revoked.len(),
            revoked_serials = ?revoked.iter().map(|entry| hex(&entry.serial)).collect::<Vec<_>>(),
            "issued a new CRL"
        );
        renewed.push(ca.key_id().to_owned());
    }
    Ok(renewed)
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
