//! TrustIoT v2 ingest service: telemetry from gateways into ClickHouse, acknowledged only after
//! the batch is on disk (decision 8, rebuild checklist I1).
//!
//! The service is built in Laysi/trustiot-v2#91. This crate already owns the ClickHouse schema in
//! `migrations/clickhouse/`: the migration Job (Laysi/trustiot-v2#60) applies it at deploy time,
//! and runtime code never creates or alters tables (decision 14; `tools/repo-check` rejects DDL
//! in `services/*/src`). See `README.md` for the tables, the row policy mechanism and the tests.
