//! The alert before a CRL lapses (Laysi/trustiot-v2#78 acceptance 2): every replica reads each
//! issuing CA's CRL from the database every `crl.check_interval` and exports, per issuing CA
//! (attributes `certs.ca.key_id` and `certs.ca.generation`):
//!
//! - `certs.crl.remaining` (s): seconds until the CRL's nextUpdate, negative once it has passed;
//!   no point while the CA has no CRL;
//! - `certs.crl.expiring` (0 or 1): 1 while the CRL has less than `crl.alert_before` left or the
//!   CA has none, the alert condition; each replica also logs an ERROR line every check while it
//!   holds;
//! - `certs.ca.remaining` (s): seconds until the issuing CA certificate's notAfter, for the
//!   rotation before it expires (services/certs/README.md).
//!
//! and, without attributes:
//!
//! - `certs.ca.signing` (0 or 1): 1 while an issuing CA certificate is valid, so the service
//!   can sign gateway certificates. It is not a readiness condition (see
//!   [`CaState::watch`]); while it is 0 each replica logs a WARN line every check, naming the
//!   keys that wait for a certificate, and while a key waits beside a signing CA (a rotation) an
//!   INFO line ([`CaState::log_signing`]).
//!
//! The root CRL (Laysi/trustiot-v2#830; services/certs/README.md, 「根 CRL」), also without
//! attributes:
//!
//! - `certs.root_crl.remaining` (s): seconds until the adopted root CRL's nextUpdate, negative
//!   once it has passed (it stays adopted); no point while there is none;
//! - `certs.root_crl.expiring` (0 or 1): 1 while the adopted root CRL has less than 60 days left
//!   or has expired ([`crate::root_crl::ALERT_BEFORE_SECS`]); each replica also logs a WARN
//!   line every check while it holds. 0 while there is no root CRL, which is normal until the
//!   first root ceremony that signs one;
//! - `certs.root_crl.invalid` (0 or 1): 1 while the root CRL file cannot be adopted (not signed
//!   by the root, not the agent's format, unreadable); each replica logs an ERROR line every
//!   check while it holds ([`CaState::log_root_crl`]).
//!
//! The values are computed when the gauges are read: the CRL and CA times from the last database
//! reading, so they keep counting down while the database is unreachable, and `certs.ca.signing`
//! and the root CRL's from the files loaded then. The Grafana alert (decision 23,
//! docs/architecture/observability.md, 憑證到期) fires on `certs.crl.expiring == 1` and also
//! when the series is missing, which is what a stopped service looks like; the root CRL's on
//! `certs.root_crl.expiring == 1` or `certs.root_crl.invalid == 1`.

use std::sync::{Mutex, OnceLock, PoisonError};

use chrono::{DateTime, Utc};
use opentelemetry::KeyValue;
use opentelemetry::metrics::ObservableGauge;
use service_kit::Context;
use service_kit::tenant::BackgroundTenantDb;
use sqlx::Row as _;

use crate::authority::{CaState, unix_now};
use crate::config::CrlSchedule;
use crate::crl::CrlError;

/// What the last reading found for one issuing CA.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaReading {
    pub key_id: String,
    pub generation: u64,
    /// The CRL's nextUpdate, Unix seconds; `None` while the CA has no CRL.
    pub crl_next_update: Option<i64>,
    /// The CA certificate's notAfter, Unix seconds.
    pub ca_not_after: i64,
}

/// The last reading, read by the gauges' callbacks.
#[derive(Debug, Default)]
struct Snapshot {
    readings: Vec<CaReading>,
    alert_before: i64,
}

static SNAPSHOT: Mutex<Option<Snapshot>> = Mutex::new(None);

type Gauges = (
    ObservableGauge<f64>,
    ObservableGauge<u64>,
    ObservableGauge<f64>,
    ObservableGauge<u64>,
    RootCrlGauges,
);

type RootCrlGauges = (
    ObservableGauge<f64>,
    ObservableGauge<u64>,
    ObservableGauge<u64>,
);

static GAUGES: OnceLock<Gauges> = OnceLock::new();

impl CaReading {
    /// Seconds the CRL has left at `now`, if there is one.
    #[must_use]
    pub fn crl_remaining(&self, now: i64) -> Option<i64> {
        self.crl_next_update.map(|next| next.saturating_sub(now))
    }

    /// Whether the alert holds at `now`: no CRL, or less than `alert_before` seconds left.
    #[must_use]
    pub fn expiring(&self, now: i64, alert_before: i64) -> bool {
        self.crl_remaining(now)
            .is_none_or(|remaining| remaining < alert_before)
    }
}

/// Starts the monitor: registers the gauges and, every `schedule.check_interval`, logs whether
/// the service can sign and what needs the operators about the root CRL, and reads the CRLs, the first time one interval after start (the CRL job
/// runs at once, so a fresh install has its first CRLs by then, and the load logged the signing
/// state already). The gauges follow the first `ca` a process starts with; a process runs one
/// service.
pub fn start<T>(context: &Context<T>, db: BackgroundTenantDb, ca: CaState, schedule: CrlSchedule) {
    GAUGES.get_or_init(|| gauges(ca.clone()));
    let alert_before = i64::try_from(schedule.alert_before.as_secs()).unwrap_or(i64::MAX);
    let shutdown = context.shutdown().clone();
    context.spawn(async move {
        let mut ticks = tokio::time::interval(schedule.check_interval);
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticks.tick().await;
        loop {
            tokio::select! {
                biased;
                () = shutdown.stopping() => break,
                _ = ticks.tick() => {}
            }
            ca.log_signing(unix_now());
            ca.log_root_crl(unix_now());
            match read(&db, &ca).await {
                Ok(readings) => report(readings, alert_before, unix_now()),
                Err(error) => tracing::warn!(
                    error = %error,
                    "cannot read the CRLs; the gauges count down from the last reading"
                ),
            }
        }
    });
}

/// Every issuing CA whose certificate has not expired, with its CRL's nextUpdate.
async fn read(db: &BackgroundTenantDb, ca: &CaState) -> Result<Vec<CaReading>, CrlError> {
    let authority = ca.authority().ok_or(CrlError::NoAuthority)?;
    let now = unix_now();
    let mut tx = db.begin_without_tenant("crl-monitor").await?;
    let rows = sqlx::query("SELECT ca_key_id, next_update FROM certs.crls")
        .fetch_all(&mut *tx)
        .await?;
    tx.commit().await?;
    let mut readings = Vec::new();
    for issuing in authority.issuing() {
        if issuing.not_after() <= now {
            continue;
        }
        let mut crl_next_update = None;
        for row in &rows {
            let id: String = row.try_get("ca_key_id")?;
            if id == issuing.key_id() {
                let next: DateTime<Utc> = row.try_get("next_update")?;
                crl_next_update = Some(next.timestamp());
            }
        }
        readings.push(CaReading {
            key_id: issuing.key_id().to_owned(),
            generation: issuing.generation(),
            crl_next_update,
            ca_not_after: issuing.not_after(),
        });
    }
    Ok(readings)
}

/// Stores `readings` for the gauges and logs the alert for each CA it holds for at `now`.
pub fn report(readings: Vec<CaReading>, alert_before: i64, now: i64) {
    for reading in &readings {
        if reading.expiring(now, alert_before) {
            tracing::error!(
                ca_key_id = %reading.key_id,
                generation = reading.generation,
                crl_remaining_seconds = reading.crl_remaining(now),
                alert_before_seconds = alert_before,
                "the CRL of an issuing CA expires soon, has expired or is missing; the MQTT \
                 broker rejects every gateway of this CA once it lapses (services/certs/README.md, \
                 「CRL 到期告警」)"
            );
        }
    }
    *SNAPSHOT.lock().unwrap_or_else(PoisonError::into_inner) = Some(Snapshot {
        readings,
        alert_before,
    });
}

/// Calls `observe` with every reading of the last snapshot and its attributes.
fn each_reading(mut observe: impl FnMut(&CaReading, i64, &[KeyValue])) {
    let snapshot = SNAPSHOT.lock().unwrap_or_else(PoisonError::into_inner);
    let Some(snapshot) = snapshot.as_ref() else {
        return;
    };
    for reading in &snapshot.readings {
        let attributes = [
            KeyValue::new("certs.ca.key_id", reading.key_id.clone()),
            KeyValue::new(
                "certs.ca.generation",
                i64::try_from(reading.generation).unwrap_or(i64::MAX),
            ),
        ];
        observe(reading, snapshot.alert_before, &attributes);
    }
}

#[allow(
    clippy::cast_precision_loss,
    reason = "seconds until an expiry fit in an f64 exactly for millions of years"
)]
fn seconds(value: i64) -> f64 {
    value as f64
}

fn gauges(ca: CaState) -> Gauges {
    let meter = opentelemetry::global::meter("certs");
    let crl_remaining = meter
        .f64_observable_gauge("certs.crl.remaining")
        .with_unit("s")
        .with_description("Seconds until the issuing CA's CRL reaches its nextUpdate")
        .with_callback(|observer| {
            let now = unix_now();
            each_reading(|reading, _, attributes| {
                if let Some(remaining) = reading.crl_remaining(now) {
                    observer.observe(seconds(remaining), attributes);
                }
            });
        })
        .build();
    let expiring = meter
        .u64_observable_gauge("certs.crl.expiring")
        .with_unit("1")
        .with_description(
            "1 while the issuing CA's CRL has less than crl.alert_before left or is missing",
        )
        .with_callback(|observer| {
            let now = unix_now();
            each_reading(|reading, alert_before, attributes| {
                observer.observe(u64::from(reading.expiring(now, alert_before)), attributes);
            });
        })
        .build();
    let ca_remaining = meter
        .f64_observable_gauge("certs.ca.remaining")
        .with_unit("s")
        .with_description("Seconds until the issuing CA certificate's notAfter")
        .with_callback(|observer| {
            let now = unix_now();
            each_reading(|reading, _, attributes| {
                observer.observe(
                    seconds(reading.ca_not_after.saturating_sub(now)),
                    attributes,
                );
            });
        })
        .build();
    let signing_ca = ca.clone();
    let signing = meter
        .u64_observable_gauge("certs.ca.signing")
        .with_unit("1")
        .with_description("1 while an issuing CA certificate is valid, so the service can sign")
        .with_callback(move |observer| {
            observer.observe(u64::from(signing_ca.signing(unix_now()).is_ok()), &[]);
        })
        .build();
    (
        crl_remaining,
        expiring,
        ca_remaining,
        signing,
        root_crl_gauges(&meter, &ca),
    )
}

fn root_crl_gauges(meter: &opentelemetry::metrics::Meter, ca: &CaState) -> RootCrlGauges {
    let state = ca.clone();
    let remaining = meter
        .f64_observable_gauge("certs.root_crl.remaining")
        .with_unit("s")
        .with_description("Seconds until the adopted root CRL reaches its nextUpdate")
        .with_callback(move |observer| {
            if let Some(crl) = state.root_crl() {
                observer.observe(seconds(crl.remaining(unix_now())), &[]);
            }
        })
        .build();
    let state = ca.clone();
    let expiring = meter
        .u64_observable_gauge("certs.root_crl.expiring")
        .with_unit("1")
        .with_description(
            "1 while the adopted root CRL has less than 60 days left or has expired; 0 without one",
        )
        .with_callback(move |observer| {
            let expiring = state.root_crl().is_some_and(|crl| crl.expiring(unix_now()));
            observer.observe(u64::from(expiring), &[]);
        })
        .build();
    let state = ca.clone();
    let invalid = meter
        .u64_observable_gauge("certs.root_crl.invalid")
        .with_unit("1")
        .with_description("1 while the root CRL file cannot be adopted")
        .with_callback(move |observer| {
            observer.observe(u64::from(state.root_crl_problem().is_some()), &[]);
        })
        .build();
    (remaining, expiring, invalid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_alert_holds_without_a_crl_and_below_the_threshold() {
        let reading = |next: Option<i64>| CaReading {
            key_id: "a".repeat(64),
            generation: 1,
            crl_next_update: next,
            ca_not_after: 10_000,
        };
        assert!(reading(None).expiring(100, 50));
        assert!(reading(Some(149)).expiring(100, 50));
        assert!(!reading(Some(150)).expiring(100, 50));
        assert!(reading(Some(90)).expiring(100, 50));
        assert_eq!(reading(Some(90)).crl_remaining(100), Some(-10));
    }
}
