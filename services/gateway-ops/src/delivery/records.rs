//! The rows of `gateway_ops.config_deliveries`: a version's publishes and the gateway's latest
//! report about it, written as the service role in the version's tenant (decision 29), so
//! row-level security keeps every write in the tenant the version belongs to.

use contract::TenantId;
use protocol::config::{ConfigOutcome, ConfigReport};
use protocol::{ConfigVersion, GatewayId};
use service_kit::tenant::{BackgroundTenantDb, TenantDbError};

/// The purpose the writes name in the log.
pub const WRITE_PURPOSE: &str = "gateway-config-delivery";

/// Records a publish the broker acknowledged. `$1` tenant, `$2` hardware ID, `$3` version.
const PUBLISHED: &str = "INSERT INTO gateway_ops.config_deliveries AS stored
    (tenant_id, hardware_id, version, first_published_at, last_published_at)
VALUES ($1::uuid, $2, $3, now(), now())
ON CONFLICT (tenant_id, hardware_id, version) DO UPDATE
    SET last_published_at = EXCLUDED.last_published_at,
        first_published_at = COALESCE(stored.first_published_at, EXCLUDED.first_published_at)";

/// How long a report that is the same as the one stored for its version (outcome, active
/// version and issues) is not written again: a gateway repeating itself costs one write per
/// window, on whichever replica its reports arrive.
pub const REPEAT_WINDOW_SECONDS: f64 = 60.0;

/// Records the gateway's latest report about a version, unless the stored report is the same
/// and was written less than `$7` seconds ago; returns a row only when it wrote. `$1` tenant,
/// `$2` hardware ID, `$3` version, `$4` outcome, `$5` active version, `$6` issues (JSON
/// array). The check is part of the write, so it holds across replicas.
const REPORTED: &str = "INSERT INTO gateway_ops.config_deliveries AS stored
    (tenant_id, hardware_id, version, outcome, active_version, issues, reported_at)
VALUES ($1::uuid, $2, $3, $4, $5, $6::jsonb, now())
ON CONFLICT (tenant_id, hardware_id, version) DO UPDATE
    SET outcome = EXCLUDED.outcome, active_version = EXCLUDED.active_version,
        issues = EXCLUDED.issues, reported_at = EXCLUDED.reported_at
    WHERE NOT (stored.outcome IS NOT DISTINCT FROM EXCLUDED.outcome
        AND stored.active_version IS NOT DISTINCT FROM EXCLUDED.active_version
        AND stored.issues IS NOT DISTINCT FROM EXCLUDED.issues
        AND stored.reported_at > now() - make_interval(secs => $7))
RETURNING 1";

/// What [`Records::reported`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reported {
    /// The report was written.
    Written,
    /// The same report was written less than [`REPEAT_WINDOW_SECONDS`] ago: nothing changed.
    Repeat,
}

/// Why a row could not be written.
#[derive(Debug, thiserror::Error)]
pub enum RecordError {
    /// The database failed.
    #[error(transparent)]
    Database(#[from] TenantDbError),
    /// The report's issues could not be written as JSON (they always can).
    #[error("the report's issues cannot be written as JSON")]
    Issues,
}

impl From<sqlx::Error> for RecordError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(TenantDbError::Database(error))
    }
}

/// The writes; cloning is cheap and shares the pool.
#[derive(Clone, Debug)]
pub struct Records {
    db: BackgroundTenantDb,
}

fn version_column(version: ConfigVersion) -> i64 {
    // Stored versions come from a bigint column, and reports above the latest version are
    // refused before they are recorded, so every version written here fits.
    i64::try_from(version.get()).unwrap_or(i64::MAX)
}

impl Records {
    /// Writes through the service role's pool `db`.
    #[must_use]
    pub fn new(db: BackgroundTenantDb) -> Self {
        Self { db }
    }

    /// Records that the broker acknowledged a retained publish of `version` of `gateway`, which
    /// belongs to `tenant`.
    ///
    /// # Errors
    ///
    /// When the database fails.
    pub async fn published(
        &self,
        tenant: TenantId,
        gateway: &GatewayId,
        version: ConfigVersion,
    ) -> Result<(), RecordError> {
        let mut tx = self.db.begin(tenant, WRITE_PURPOSE).await?;
        sqlx::query(PUBLISHED)
            .bind(tenant.to_string())
            .bind(gateway.as_str())
            .bind(version_column(version))
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Records `report` (an applied or rejected outcome) of `gateway` about a version of
    /// `tenant`, unless it repeats the stored one within the window; see [`Reported`].
    ///
    /// # Errors
    ///
    /// When the database fails.
    pub async fn reported(
        &self,
        tenant: TenantId,
        gateway: &GatewayId,
        report: &ConfigReport,
    ) -> Result<Reported, RecordError> {
        let outcome = match report.outcome {
            ConfigOutcome::Applied => "applied",
            ConfigOutcome::Rejected => "rejected",
            // Refused before it gets here (crate::delivery::reports).
            ConfigOutcome::Other(_) => return Ok(Reported::Repeat),
        };
        let issues = serde_json::to_string(&report.issues).map_err(|_| RecordError::Issues)?;
        let mut tx = self.db.begin(tenant, WRITE_PURPOSE).await?;
        let written: Option<i32> = sqlx::query_scalar(REPORTED)
            .bind(tenant.to_string())
            .bind(gateway.as_str())
            .bind(version_column(report.version))
            .bind(outcome)
            .bind(report.active_version.map(version_column))
            .bind(issues)
            .bind(REPEAT_WINDOW_SECONDS)
            .fetch_optional(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(if written.is_some() {
            Reported::Written
        } else {
            Reported::Repeat
        })
    }
}
