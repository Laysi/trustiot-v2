//! TrustIoT v2 ingest service: telemetry from gateways into ClickHouse, acknowledged only after
//! the batch is on disk (decision 8, rebuild checklist I1).
//!
//! The service is built in Laysi/trustiot-v2#91. This crate already owns the ClickHouse schema in
//! `migrations/clickhouse/`: the migration Job (Laysi/trustiot-v2#60) applies it at deploy time,
//! and runtime code never creates or alters tables (decision 14; `tools/repo-check` rejects DDL
//! in `services/*/src`). See `README.md` for the tables, the row policy mechanism and the tests.
//!
//! [`retention`] holds the platform's raw and aggregate data retention, which the migration Job
//! applies as the tables' TTL (rebuild checklist I2, Laysi/trustiot-v2#92, Laysi/trustiot-v2#141).
//! [`rollup`] holds the statements on the one-minute and one-hour aggregate tables: the settings
//! every statement that writes a stream must carry ([`rollup::stream_settings`]) and the
//! reconcile the ingest service must run after an I1 rewrite (no code writes or rewrites
//! telemetry yet: the write path is Laysi/trustiot-v2#91 and the rewrite Laysi/trustiot-v2#641,
//! which take over that contract), the backfill the migration Job runs, and the energy query of
//! cumulative points.

pub mod retention;
pub mod rollup;
