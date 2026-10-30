//! The certs service's Postgres schema as this release expects it: its migrations/postgres
//! files in apply order (Laysi/trustiot-v2#402). Since the factory registration
//! (Laysi/trustiot-v2#79) it produces outbox events (`certs.gateway_registered`, platform-wide),
//! so its schema gets the outbox producer's tables and policies; since Laysi/trustiot-v2#632 it
//! also consumes the assets service's events (`tools/migrate/src/registry.rs` lists it as a
//! consumer of `assets`), so the Job creates the consumer's tables too.
//!
//! Two crates compile this file: the service, whose readiness check
//! ([`service_kit::migrations::MigrationCheck`]) keeps failing until the database has every
//! file, and the migration Job (`tools/migrate/src/registry.rs` includes it with `#[path]`),
//! which applies exactly these files. One list, so the two cannot disagree. Keep the file
//! self-contained: it may use `service_kit` only. A new file is a new entry at the end; applied
//! files never change (`tools/migrate/tests/registry.rs` compares the list with the directory).

use service_kit::migrations::{Migration, Schema};

/// What the certs service expects of its schema.
pub const SCHEMA: Schema = Schema {
    service: "certs",
    outbox_producer: true,
    migrations: &[
        Migration {
            name: "0001_certs.sql",
            sql: include_str!("../migrations/postgres/0001_certs.sql"),
        },
        Migration {
            name: "0002_workstation_certificates.sql",
            sql: include_str!("../migrations/postgres/0002_workstation_certificates.sql"),
        },
        Migration {
            name: "0003_broker_certificates.sql",
            sql: include_str!("../migrations/postgres/0003_broker_certificates.sql"),
        },
        Migration {
            name: "0004_gateway_registrations.sql",
            sql: include_str!("../migrations/postgres/0004_gateway_registrations.sql"),
        },
        Migration {
            name: "0005_gateway_bindings.sql",
            sql: include_str!("../migrations/postgres/0005_gateway_bindings.sql"),
        },
        Migration {
            name: "0006_certificate_renewal.sql",
            sql: include_str!("../migrations/postgres/0006_certificate_renewal.sql"),
        },
    ],
};
