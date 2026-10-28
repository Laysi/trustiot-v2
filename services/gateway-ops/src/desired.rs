//! The read interface of the stored desired configurations (services/gateway-ops/README.md,
//! "讀取介面"), for the delivery of Laysi/trustiot-v2#720: a gateway's current configuration, and
//! every gateway's latest version for republishing after a start.
//!
//! The delivery works per gateway, before it knows a tenant, so both reads are the platform
//! role's, in READ ONLY transactions through the `platform_access` policy of
//! `gateway_ops.desired_configs`, which opens SELECT only. The payload is returned exactly as
//! stored, so what is published is what was validated and versioned; it is decoded here once
//! more only to refuse a row whose payload and version disagree.

use chrono::{DateTime, Utc};
use contract::{GatewayBindingId, ServiceName, TenantId};
use protocol::config::DesiredConfig;
use protocol::{ConfigVersion, GatewayId};
use service_kit::identity::ServiceIdentity;
use service_kit::tenant::{PlatformActor, PlatformDb, PlatformReader, TenantDbError};
use sqlx::Row;
use sqlx::postgres::PgRow;

/// The purpose the reads name in the log.
pub const READ_PURPOSE: &str = "gateway-config-delivery";

/// A gateway's latest configuration, whichever tenant holds it. `$1` hardware ID.
const CURRENT: &str = "SELECT hardware_id, tenant_id::text AS tenant_id,
    binding_id::text AS binding_id, version, bound, device_count, payload, created_at
FROM gateway_ops.desired_configs
WHERE hardware_id = $1
ORDER BY version DESC
LIMIT 1";

/// Every gateway's latest version.
const LATEST_VERSIONS: &str = "SELECT DISTINCT ON (hardware_id) hardware_id, version
FROM gateway_ops.desired_configs
ORDER BY hardware_id, version DESC";

/// A gateway's stored desired configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredConfig {
    /// The gateway.
    pub hardware_id: GatewayId,
    /// The tenant of the binding it was made for.
    pub tenant_id: TenantId,
    /// The binding it was made for.
    pub binding_id: GatewayBindingId,
    /// Its version, also inside the payload.
    pub version: ConfigVersion,
    /// `false` for the configuration without devices that ends a binding (README.md, "解綁").
    pub bound: bool,
    /// How many devices it lists.
    pub device_count: u32,
    /// The encoded `DesiredConfig` (crates/protocol's codec) to publish unchanged.
    pub payload: Vec<u8>,
    /// When it was stored.
    pub created_at: DateTime<Utc>,
}

/// A gateway's latest version.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LatestVersion {
    /// The gateway.
    pub hardware_id: GatewayId,
    /// Its latest configuration version.
    pub version: ConfigVersion,
}

/// Why a configuration could not be read.
#[derive(Debug, thiserror::Error)]
pub enum ReadError {
    /// A stored row breaks the rules the table and the generation enforce, which cannot happen.
    #[error("a stored desired configuration breaks the rules: {0}")]
    Corrupt(&'static str),
    /// The database failed.
    #[error(transparent)]
    Database(#[from] TenantDbError),
}

impl From<sqlx::Error> for ReadError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(TenantDbError::Database(error))
    }
}

/// The reads; see the module. Cloning is cheap and shares the pool.
#[derive(Clone, Debug)]
pub struct DesiredConfigs {
    reader: PlatformReader,
}

impl DesiredConfigs {
    /// Reads on the platform role's pool `platform`, as the gateway-ops service on its own
    /// behalf, purpose [`READ_PURPOSE`].
    #[must_use]
    pub fn new(platform: &PlatformDb) -> Self {
        let actor = PlatformActor::service(ServiceIdentity::new(ServiceName::GatewayOps));
        Self {
            reader: platform.reader(actor, READ_PURPOSE),
        }
    }

    /// The gateway's latest configuration; `None` when it has none yet.
    ///
    /// # Errors
    ///
    /// [`ReadError`] when the database fails or the stored row is corrupt.
    pub async fn current(
        &self,
        hardware_id: &GatewayId,
    ) -> Result<Option<StoredConfig>, ReadError> {
        let mut tx = self.reader.begin().await?;
        let row = sqlx::query(CURRENT)
            .bind(hardware_id.as_str())
            .fetch_optional(&mut *tx)
            .await?;
        tx.commit().await?;
        row.as_ref().map(stored_config).transpose()
    }

    /// Every gateway's latest version, by hardware ID.
    ///
    /// # Errors
    ///
    /// [`ReadError`] when the database fails or a stored row is corrupt.
    pub async fn latest_versions(&self) -> Result<Vec<LatestVersion>, ReadError> {
        let mut tx = self.reader.begin().await?;
        let rows = sqlx::query(LATEST_VERSIONS).fetch_all(&mut *tx).await?;
        tx.commit().await?;
        rows.iter()
            .map(|row| {
                Ok(LatestVersion {
                    hardware_id: gateway(row)?,
                    version: version(row)?,
                })
            })
            .collect()
    }
}

fn gateway(row: &PgRow) -> Result<GatewayId, ReadError> {
    GatewayId::new(row.try_get::<String, _>("hardware_id")?)
        .map_err(|_| ReadError::Corrupt("hardware_id"))
}

fn version(row: &PgRow) -> Result<ConfigVersion, ReadError> {
    u64::try_from(row.try_get::<i64, _>("version")?)
        .ok()
        .and_then(ConfigVersion::new)
        .ok_or(ReadError::Corrupt("version"))
}

fn stored_config(row: &PgRow) -> Result<StoredConfig, ReadError> {
    let version = version(row)?;
    let payload: Vec<u8> = row.try_get("payload")?;
    let decoded = DesiredConfig::decode(&payload).map_err(|_| ReadError::Corrupt("payload"))?;
    if decoded.version != version {
        return Err(ReadError::Corrupt("payload version"));
    }
    Ok(StoredConfig {
        hardware_id: gateway(row)?,
        tenant_id: row
            .try_get::<String, _>("tenant_id")?
            .parse()
            .map_err(|_| ReadError::Corrupt("tenant_id"))?,
        binding_id: row
            .try_get::<String, _>("binding_id")?
            .parse()
            .map_err(|_| ReadError::Corrupt("binding_id"))?,
        version,
        bound: row.try_get("bound")?,
        device_count: u32::try_from(row.try_get::<i32, _>("device_count")?)
            .map_err(|_| ReadError::Corrupt("device_count"))?,
        payload,
        created_at: row.try_get("created_at")?,
    })
}
