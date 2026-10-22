//! The product's ClickHouse DDL (`services/ingest/migrations/clickhouse/`), in apply order, for
//! [`crate::registry::PRODUCT`].
//!
//! A module of its own so that `tools/sql-check` can allow exactly this file: `0004_access.sql`
//! has a comment that names ClickHouse's `SET ROLE NONE`, which sql-check reads as a Postgres
//! session setting. Everything else of the registry, the per-tenant role hooks included, stays
//! checked. `tests/registry.rs` checks that every file of the directory is listed, in order.

use crate::registry::Migration;

/// `services/ingest/migrations/clickhouse/` in apply order.
pub const FILES: &[Migration] = &[
    Migration {
        name: "0001_databases.sql",
        sql: include_str!("../../../services/ingest/migrations/clickhouse/0001_databases.sql"),
    },
    Migration {
        name: "0002_telemetry.sql",
        sql: include_str!("../../../services/ingest/migrations/clickhouse/0002_telemetry.sql"),
    },
    Migration {
        name: "0003_batch_commits.sql",
        sql: include_str!("../../../services/ingest/migrations/clickhouse/0003_batch_commits.sql"),
    },
    Migration {
        name: "0004_access.sql",
        sql: include_str!("../../../services/ingest/migrations/clickhouse/0004_access.sql"),
    },
    Migration {
        name: "0005_tenant_view.sql",
        sql: include_str!("../../../services/ingest/migrations/clickhouse/0005_tenant_view.sql"),
    },
    Migration {
        name: "0006_logs.sql",
        sql: include_str!("../../../services/ingest/migrations/clickhouse/0006_logs.sql"),
    },
    Migration {
        name: "0007_monitoring.sql",
        sql: include_str!("../../../services/ingest/migrations/clickhouse/0007_monitoring.sql"),
    },
    Migration {
        name: "0008_telemetry_query.sql",
        sql: include_str!(
            "../../../services/ingest/migrations/clickhouse/0008_telemetry_query.sql"
        ),
    },
    Migration {
        name: "0009_telemetry_query_grantees.sql",
        sql: include_str!(
            "../../../services/ingest/migrations/clickhouse/0009_telemetry_query_grantees.sql"
        ),
    },
];
