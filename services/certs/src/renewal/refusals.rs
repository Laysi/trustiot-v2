//! Refused renewals of registered gateways: counted per hardware ID, [`Category`] and time
//! window, and reported once per window as the platform-wide `certs.gateway_renewal_refused` once
//! it has ended (README.md, "換發", "事件").
//!
//! The two categories have tables and windows of their own (Laysi/trustiot-v2#847): the refusals
//! that need no key (`certs.renewal_unverified_refusals`) are counted per day
//! ([`UNVERIFIED_WINDOW`]), so anyone who knows registered hardware IDs makes at most one event
//! per gateway and day; the ones after the signature held (`certs.renewal_refusals`) per
//! `renewal.refusal_window`.
//!
//! Both run as the platform role: only it may read the registrations, which decide whether a
//! hardware ID is counted at all, and only it may write platform-wide events.
//!
//! Exactly one event per window: the replica that reports a window deletes its row and writes
//! the event in one transaction, and the others skip rows it holds (`FOR UPDATE SKIP LOCKED`).
//! A window is reported only [`REPORT_MARGIN`] after it ended. A count runs one statement under
//! [`COUNT_STATEMENT_TIMEOUT`] and picks its window by the statement's start, so no count of an
//! ended window can still be running when the window's row is deleted, which would start a
//! second row for the same window.

use std::collections::BTreeMap;
use std::time::Duration;

use chrono::{DateTime, Utc};
use contract::ServiceName;
use contract::event::Actor;
use contract::events::certs::{GATEWAY_REGISTRATION_AGGREGATE, GatewayRenewalRefused};
use outbox::Aggregate;
use protocol::GatewayId;
use service_kit::Context;
use service_kit::identity::ServiceIdentity;
use service_kit::tenant::{PlatformActor, PlatformDb, PlatformReader, PlatformTransaction};
use sqlx::Row as _;

/// How long after a window ended it is reported: well past [`COUNT_STATEMENT_TIMEOUT`].
pub const REPORT_MARGIN: Duration = Duration::from_secs(60);

/// The longest a count's statement may run; see the module documentation.
pub const COUNT_STATEMENT_TIMEOUT: Duration = Duration::from_secs(5);

/// The longest a count waits for a row lock.
const COUNT_LOCK_TIMEOUT: Duration = Duration::from_secs(2);

/// Windows reported per transaction.
const REPORT_BATCH: i64 = 100;

/// The purpose of counting in the audit log (`platform database access`).
const COUNT_PURPOSE: &str = "count-renewal-refusal";

/// The purpose of reporting in the audit log.
const REPORT_PURPOSE: &str = "report-renewal-refusals";

/// The window of the refusals that need no key: a day, so a gateway's hardware ID alone makes
/// at most one event a day however often it is named. Not shorter than any
/// `renewal.refusal_window` (at most a day).
pub const UNVERIFIED_WINDOW: Duration = Duration::from_secs(86_400);

/// Which refusals a count belongs to; each has its own table and window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Category {
    /// Refused before the signature held for the registered key (an unknown, mismatched or
    /// expired challenge, a wrong signature): anyone who knows the hardware ID can cause it.
    /// Counted per [`UNVERIFIED_WINDOW`] in `certs.renewal_unverified_refusals`, whose
    /// `CHECK` admits only these reasons.
    Unverified,
    /// Refused after the signature held: only the holder of the registered key gets here.
    /// Counted per `renewal.refusal_window` in `certs.renewal_refusals`.
    Verified,
}

impl Category {
    /// Both, in the order they are reported.
    const ALL: [Self; 2] = [Self::Verified, Self::Unverified];

    /// The window of this category: `configured` is `renewal.refusal_window`.
    pub(crate) const fn window(self, configured: Duration) -> Duration {
        match self {
            Self::Unverified => UNVERIFIED_WINDOW,
            Self::Verified => configured,
        }
    }
}

/// Bounds the rest of the transaction on the server, like `SET LOCAL`: `$1` and `$2` are the
/// statement and lock timeouts in milliseconds.
const SET_TIMEOUTS: &str = "SELECT set_config('statement_timeout', $1, true), \
     set_config('lock_timeout', $2, true)";

/// Adds one refusal for `reason` (`$2`) to the window of `$1` that holds the statement's start,
/// for windows of `$3` seconds aligned to 2000-01-01T00:00:00Z; nothing when `$1` is not
/// registered. Returns a row when it counted.
const COUNT: &str = "INSERT INTO certs.renewal_refusals AS stored
    (hardware_id, window_start, window_end, refusals, reasons)
SELECT registration.hardware_id, bin.start, bin.start + make_interval(secs => $3), 1,
    jsonb_build_object($2::text, 1)
FROM certs.gateway_registrations AS registration
CROSS JOIN (
    SELECT date_bin(make_interval(secs => $3), statement_timestamp(),
        TIMESTAMPTZ '2000-01-01 00:00:00+00') AS start
) AS bin
WHERE registration.hardware_id = $1
ON CONFLICT (hardware_id, window_start) DO UPDATE
    SET refusals = stored.refusals + 1,
        reasons = stored.reasons || jsonb_build_object($2::text,
            COALESCE((stored.reasons ->> $2::text)::bigint, 0) + 1)
RETURNING 1";

/// [`COUNT`] for [`Category::Unverified`].
const COUNT_UNVERIFIED: &str = "INSERT INTO certs.renewal_unverified_refusals AS stored
    (hardware_id, window_start, window_end, refusals, reasons)
SELECT registration.hardware_id, bin.start, bin.start + make_interval(secs => $3), 1,
    jsonb_build_object($2::text, 1)
FROM certs.gateway_registrations AS registration
CROSS JOIN (
    SELECT date_bin(make_interval(secs => $3), statement_timestamp(),
        TIMESTAMPTZ '2000-01-01 00:00:00+00') AS start
) AS bin
WHERE registration.hardware_id = $1
ON CONFLICT (hardware_id, window_start) DO UPDATE
    SET refusals = stored.refusals + 1,
        reasons = stored.reasons || jsonb_build_object($2::text,
            COALESCE((stored.reasons ->> $2::text)::bigint, 0) + 1)
RETURNING 1";

/// Whether a window of either category that ended at least `$1` seconds ago waits to be
/// reported.
const ANY_DUE: &str = "SELECT EXISTS (
    SELECT 1 FROM certs.renewal_refusals WHERE window_end <= now() - make_interval(secs => $1)
) OR EXISTS (
    SELECT 1 FROM certs.renewal_unverified_refusals
    WHERE window_end <= now() - make_interval(secs => $1)
)";

/// Deletes up to `$2` windows that ended at least `$1` seconds ago, oldest first, skipping rows
/// another transaction holds, and returns them.
const TAKE_DUE: &str = "WITH due AS (
    SELECT hardware_id, window_start FROM certs.renewal_refusals
    WHERE window_end <= now() - make_interval(secs => $1)
    ORDER BY window_end, hardware_id
    LIMIT $2
    FOR UPDATE SKIP LOCKED
)
DELETE FROM certs.renewal_refusals AS refusal
USING due
WHERE refusal.hardware_id = due.hardware_id AND refusal.window_start = due.window_start
RETURNING refusal.hardware_id, refusal.window_start, refusal.window_end, refusal.refusals,
    refusal.reasons::text AS reasons";

/// [`TAKE_DUE`] for [`Category::Unverified`].
const TAKE_DUE_UNVERIFIED: &str = "WITH due AS (
    SELECT hardware_id, window_start FROM certs.renewal_unverified_refusals
    WHERE window_end <= now() - make_interval(secs => $1)
    ORDER BY window_end, hardware_id
    LIMIT $2
    FOR UPDATE SKIP LOCKED
)
DELETE FROM certs.renewal_unverified_refusals AS refusal
USING due
WHERE refusal.hardware_id = due.hardware_id AND refusal.window_start = due.window_start
RETURNING refusal.hardware_id, refusal.window_start, refusal.window_end, refusal.refusals,
    refusal.reasons::text AS reasons";

fn actor() -> PlatformActor {
    PlatformActor::service(ServiceIdentity::new(ServiceName::Certs))
}

/// Counts a refusal of `hardware_id` for `reason` of `category` in that category's current
/// window (`refusal_window` is `renewal.refusal_window`, [`Category::window`]). Returns whether
/// it counted: `false` when the hardware ID is not registered.
///
/// # Errors
///
/// When the database fails; the caller logs it and answers as it would have.
pub(crate) async fn count(
    db: &PlatformDb,
    hardware_id: &GatewayId,
    reason: &'static str,
    category: Category,
    refusal_window: Duration,
) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    let window = category.window(refusal_window);
    let statement = match category {
        Category::Unverified => sqlx::query(COUNT_UNVERIFIED),
        Category::Verified => sqlx::query(COUNT),
    };
    let mut tx = db.begin(actor(), COUNT_PURPOSE).await?;
    sqlx::query(SET_TIMEOUTS)
        .bind(format!("{}ms", COUNT_STATEMENT_TIMEOUT.as_millis()))
        .bind(format!("{}ms", COUNT_LOCK_TIMEOUT.as_millis()))
        .execute(&mut *tx)
        .await?;
    let counted = statement
        .bind(hardware_id.as_str())
        .bind(reason)
        .bind(window.as_secs_f64())
        .fetch_optional(&mut *tx)
        .await?
        .is_some();
    tx.commit().await?;
    Ok(counted)
}

/// Every `interval` until shutdown, reports the windows that ended at least [`REPORT_MARGIN`]
/// ago. Runs on every replica; failures are logged at WARN and retried next interval, and the
/// rows stay until then.
pub(crate) fn start<T>(context: &Context<T>, db: PlatformDb, interval: Duration) {
    let reader = db.reader(actor(), REPORT_PURPOSE);
    let shutdown = context.shutdown().clone();
    context.spawn(async move {
        let mut ticks = tokio::time::interval(interval);
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                biased;
                () = shutdown.stopping() => break,
                _ = ticks.tick() => {}
            }
            match report_due(&db, &reader).await {
                Ok(0) => {}
                Ok(reported) => {
                    tracing::info!(windows = reported, "reported refused certificate renewals")
                }
                Err(error) => tracing::warn!(
                    error = %error,
                    "could not report refused certificate renewals; trying again next interval"
                ),
            }
        }
    });
}

/// Reports every window due now, of both categories, a batch per transaction; returns how many.
/// The check that anything is due is a read-only transaction logged at DEBUG, so an idle
/// interval leaves no INFO line.
///
/// # Errors
///
/// When the database fails; the batches before it stay reported.
pub(crate) async fn report_due(
    db: &PlatformDb,
    reader: &PlatformReader,
) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
    let margin = REPORT_MARGIN.as_secs_f64();
    let mut check = reader.begin().await?;
    let due: bool = sqlx::query_scalar(ANY_DUE)
        .bind(margin)
        .fetch_one(&mut *check)
        .await?;
    check.commit().await?;
    if !due {
        return Ok(0);
    }
    let mut reported = 0;
    for category in Category::ALL {
        loop {
            let mut tx = db.begin(actor(), REPORT_PURPOSE).await?;
            let take = match category {
                Category::Unverified => sqlx::query(TAKE_DUE_UNVERIFIED),
                Category::Verified => sqlx::query(TAKE_DUE),
            };
            let rows = take
                .bind(margin)
                .bind(REPORT_BATCH)
                .fetch_all(&mut *tx)
                .await?;
            for row in &rows {
                publish(&mut tx, row).await?;
            }
            tx.commit().await?;
            reported += rows.len();
            if rows.len() < usize::try_from(REPORT_BATCH).unwrap_or(usize::MAX) {
                break;
            }
        }
    }
    Ok(reported)
}

/// Writes the event of one deleted window in the deleting transaction. A row the event cannot
/// carry (it breaks the table's rules, which cannot happen) is logged at ERROR and dropped
/// rather than blocking every later window.
async fn publish(
    tx: &mut PlatformTransaction,
    row: &sqlx::postgres::PgRow,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let hardware_id: String = row.try_get("hardware_id")?;
    let window_start: DateTime<Utc> = row.try_get("window_start")?;
    let window_end: DateTime<Utc> = row.try_get("window_end")?;
    let refusals: i64 = row.try_get("refusals")?;
    let reasons: String = row.try_get("reasons")?;
    let event = match (
        GatewayId::new(hardware_id.clone()),
        u64::try_from(refusals),
        serde_json::from_str::<BTreeMap<String, u64>>(&reasons),
    ) {
        (Ok(hardware_id), Ok(refusals), Ok(reasons)) => GatewayRenewalRefused {
            hardware_id,
            window_start,
            window_end,
            refusals,
            reasons,
        },
        _ => {
            tracing::error!(
                hardware_id,
                window_start = %window_start,
                "dropped a renewal refusal count that breaks the table's rules"
            );
            return Ok(());
        }
    };
    let aggregate = Aggregate::new(GATEWAY_REGISTRATION_AGGREGATE, event.hardware_id.as_str())?;
    outbox::publish_as(
        tx.transaction(),
        &aggregate,
        &event,
        None,
        Actor::Service {
            service: ServiceName::Certs,
        },
    )
    .await?;
    tracing::info!(
        hardware_id = %event.hardware_id,
        window_start = %event.window_start,
        window_end = %event.window_end,
        refusals = event.refusals,
        "reported refused renewals of a registered gateway"
    );
    Ok(())
}
