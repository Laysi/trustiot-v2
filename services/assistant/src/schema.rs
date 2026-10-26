//! The assistant service's Postgres schema as this release expects it: its migrations/postgres
//! files in apply order (Laysi/trustiot-v2#553). No outbox: the assistant writes no events yet.
//!
//! Two crates compile this file: the service, whose readiness check
//! ([`service_kit::migrations::MigrationCheck`], registered in `start`) fails until the database
//! has every file (Laysi/trustiot-v2#592), and
//! the migration Job (`tools/migrate/src/registry.rs` includes it with `#[path]`), which applies
//! exactly these files. One list, so the two cannot disagree. Keep the file self-contained: it
//! may use `service_kit` only. A new file is a new entry at the end; applied files never change
//! (`tools/migrate/tests/registry.rs` compares the list with the directory).

use service_kit::migrations::{Migration, Schema};

/// What the assistant service expects of its schema.
pub const SCHEMA: Schema = Schema {
    service: "assistant",
    outbox_producer: false,
    migrations: &[
        Migration {
            name: "0001_assistant.sql",
            sql: include_str!("../migrations/postgres/0001_assistant.sql"),
        },
        // The dashboards and their versions (Laysi/trustiot-v2#150).
        Migration {
            name: "0002_dashboards.sql",
            sql: include_str!("../migrations/postgres/0002_dashboards.sql"),
        },
    ],
};
