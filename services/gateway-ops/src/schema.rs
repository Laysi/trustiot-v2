//! The gateway operations service's Postgres schema as this release expects it: its
//! migrations/postgres files in apply order (Laysi/trustiot-v2#402, Laysi/trustiot-v2#719,
//! Laysi/trustiot-v2#720). It consumes the assets service's outbox events but produces none, so
//! its schema gets no outbox template.
//!
//! Two crates compile this file: the service, whose readiness check
//! ([`service_kit::migrations::MigrationCheck`]) keeps failing until the database has every
//! file, and the migration Job (`tools/migrate/src/registry.rs` includes it with `#[path]`),
//! which applies exactly these files. One list, so the two cannot disagree. Keep the file
//! self-contained: it may use `service_kit` only. A new file is a new entry at the end; applied
//! files never change (`tools/migrate/tests/registry.rs` compares the list with the directory).

use service_kit::migrations::{Migration, Schema};

/// What the gateway operations service expects of its Postgres schema.
pub const SCHEMA: Schema = Schema {
    service: "gateway-ops",
    outbox_producer: false,
    migrations: &[
        Migration {
            name: "0001_gateway_ops.sql",
            sql: include_str!("../migrations/postgres/0001_gateway_ops.sql"),
        },
        Migration {
            name: "0002_config_deliveries.sql",
            sql: include_str!("../migrations/postgres/0002_config_deliveries.sql"),
        },
    ],
};
