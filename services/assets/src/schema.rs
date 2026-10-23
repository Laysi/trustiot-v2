//! The assets service's Postgres schema as this release expects it: its migrations/postgres
//! files in apply order (Laysi/trustiot-v2#402). It consumes the identity service's outbox
//! events and, since the device models (Laysi/trustiot-v2#99), produces its own, so its schema
//! gets the outbox producer's tables and policies too.
//!
//! Two crates compile this file: the service, whose readiness check
//! ([`service_kit::migrations::MigrationCheck`]) keeps failing until the database has every
//! file, and the migration Job (`tools/migrate/src/registry.rs` includes it with `#[path]`),
//! which applies exactly these files. One list, so the two cannot disagree. Keep the file
//! self-contained: it may use `service_kit` only. A new file is a new entry at the end; applied
//! files never change (`tools/migrate/tests/registry.rs` compares the list with the directory).

use service_kit::migrations::{Migration, Schema};

/// What the assets service expects of its schema.
pub const SCHEMA: Schema = Schema {
    service: "assets",
    outbox_producer: true,
    migrations: &[
        Migration {
            name: "0001_assets.sql",
            sql: include_str!("../migrations/postgres/0001_assets.sql"),
        },
        Migration {
            name: "0002_device_models.sql",
            sql: include_str!("../migrations/postgres/0002_device_models.sql"),
        },
        Migration {
            name: "0003_gateway_pool.sql",
            sql: include_str!("../migrations/postgres/0003_gateway_pool.sql"),
        },
        Migration {
            name: "0004_devices.sql",
            sql: include_str!("../migrations/postgres/0004_devices.sql"),
        },
    ],
};
