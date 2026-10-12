-- The migration Job's own bookkeeping (tools/migrate/README.md). The migration role runs this at
-- the start of every run, so every statement is idempotent; anything that is not goes into a
-- numbered file next to it, which the Job applies once.
--
-- One row per applied file: `component` is `migrate` (the Job's numbered files), `database`
-- (crates/service-kit/sql/database.sql), `<service>` (its migrations/postgres files) or
-- `<service>.outbox` (the outbox policy template). `sha256` is the file's hash when it was
-- applied; a file that changed afterwards stops the Job, because applied files are append-only.

CREATE SCHEMA IF NOT EXISTS trustiot_migrate;

CREATE TABLE IF NOT EXISTS trustiot_migrate.applied (
    component text NOT NULL,
    name text NOT NULL,
    sha256 text NOT NULL CHECK (sha256 ~ '^[0-9a-f]{64}$'),
    applied_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (component, name)
);
