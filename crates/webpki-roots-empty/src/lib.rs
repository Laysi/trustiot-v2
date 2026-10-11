//! An empty replacement for the crates.io `webpki-roots` (Mozilla's root certificates).
//!
//! sqlx's rustls connector builds its trust store from `webpki_roots::TLS_SERVER_ROOTS` and then
//! adds the configured `sslrootcert`, so with the real crate a Postgres server certificate from
//! any public CA for the database host name passes `verify-full`. sqlx has no setting or feature
//! that leaves the built-in roots out, so the root `Cargo.toml` patches `webpki-roots` with this
//! crate: the list is empty and the trust store holds only the CA a service configures
//! (`TRUSTIOT_POSTGRES_CA_FILE`, `service_kit::postgres::PostgresConfig::ca_file`).
//!
//! The patch applies to the whole workspace. Nothing else may depend on `webpki-roots`: a client
//! that needs public roots would silently get none. `crates/service-kit/tests/postgres_trust.rs`
//! checks `Cargo.lock` for that, for the patch being in use, and for sqlx loading no other root
//! source (such as the system store through `rustls-native-certs`).

#![no_std]

use rustls_pki_types::TrustAnchor;

/// No trust anchors. Same name and type as the crates.io crate's list.
pub const TLS_SERVER_ROOTS: &[TrustAnchor<'static>] = &[];
