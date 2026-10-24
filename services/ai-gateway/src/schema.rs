//! The AI gateway's Postgres schema as this release expects it: its migrations/postgres files in
//! apply order (Laysi/trustiot-v2#605). It neither produces nor consumes outbox events, so its
//! schema gets no outbox template.
//!
//! Two crates compile this file: the service, whose readiness check
//! ([`service_kit::migrations::MigrationCheck`]) keeps failing until the database has every
//! file, and the migration Job (`tools/migrate/src/registry.rs` includes it with `#[path]`),
//! which applies exactly these files. One list, so the two cannot disagree. Keep the file
//! self-contained: it may use `service_kit` only. A new file is a new entry at the end; applied
//! files never change (`tools/migrate/tests/registry.rs` compares the list with the directory).

use service_kit::migrations::{Migration, Schema};

/// What the AI gateway expects of its schema.
pub const SCHEMA: Schema = Schema {
    service: "ai-gateway",
    outbox_producer: false,
    migrations: &[Migration {
        name: "0001_model_roles.sql",
        sql: include_str!("../migrations/postgres/0001_model_roles.sql"),
    }],
};
