//! The identity service's Postgres schema as this release expects it: its migrations/postgres
//! files in apply order, the outbox templates it gets as a producer of
//! `identity.tenant_created` (Laysi/trustiot-v2#402), and the producers whose events its event
//! log consumes (Laysi/trustiot-v2#130).
//!
//! Two crates compile this file: the service, whose readiness check
//! ([`service_kit::migrations::MigrationCheck`]) keeps failing until the database has every
//! file, and the migration Job (`tools/migrate/src/registry.rs` includes it with `#[path]`),
//! which applies exactly these files. One list, so the two cannot disagree. Keep the file
//! self-contained: it may use `service_kit` only. A new file is a new entry at the end; applied
//! files never change (`tools/migrate/tests/registry.rs` compares the list with the directory).

use service_kit::migrations::{Migration, Schema};

/// What the identity service expects of its schema.
pub const SCHEMA: Schema = Schema {
    service: "identity",
    outbox_producer: true,
    migrations: &[
        Migration {
            name: "0001_identity.sql",
            sql: include_str!("../migrations/postgres/0001_identity.sql"),
        },
        Migration {
            name: "0002_tenants.sql",
            sql: include_str!("../migrations/postgres/0002_tenants.sql"),
        },
        Migration {
            name: "0003_event_log.sql",
            sql: include_str!("../migrations/postgres/0003_event_log.sql"),
        },
    ],
};

/// The outbox producers whose events the event log records (services/identity/README.md,
/// "事件層與操作紀錄"): the service runs one consumer per producer, and the migration Job makes
/// its platform role a reader of each one's `outbox_event` (the registry's `Outbox::consumes`).
/// A producer is added here when it starts writing events, in the same change: `identity`
/// (tenants, Laysi/trustiot-v2#65) and `assets` (device models, Laysi/trustiot-v2#99).
pub const CONSUMES: &[&str] = &["identity", "assets"];
