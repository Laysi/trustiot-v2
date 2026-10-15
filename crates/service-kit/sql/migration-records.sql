-- Template: the migration records in a service's own schema (Laysi/trustiot-v2#402). The
-- migration Job (tools/migrate) runs it as the service's owner role on every run, before it
-- applies anything in the schema, with `example_schema` replaced by the schema and `example_app`
-- by the service role. Every statement is idempotent.
--
-- One row per file the Job applied in this schema (the service's migrations/postgres files and,
-- for an outbox producer, the outbox templates), with the same component, name and SHA-256 as its
-- row in trustiot_migrate.applied. The Job writes both rows in the same transaction, and on
-- every run makes this copy equal to its own records of the service, so a database from before
-- this table gets it filled on the next run and a copy changed by hand is set back.
--
-- service-kit's `migrations` readiness check reads it as the service role, which may only
-- SELECT: the table belongs to the owner role, which never logs in, and no other role gets a
-- privilege. trustiot_migrate itself stays closed to every service role.
--
-- tools/migrate/tests/service_records.rs runs it through the Job; services/identity/tests
-- runs it for the schema the tests there set up by hand.

CREATE TABLE IF NOT EXISTS example_schema.trustiot_migrate_applied (
    component text NOT NULL,
    name text NOT NULL,
    sha256 text NOT NULL CHECK (sha256 ~ '^[0-9a-f]{64}$'),
    PRIMARY KEY (component, name)
);

REVOKE ALL ON example_schema.trustiot_migrate_applied FROM PUBLIC;
-- The service role reaches its own schema anyway (its search_path); granting it here too lets
-- the check read the records before the service's first file, which grants the same, is applied.
GRANT USAGE ON SCHEMA example_schema TO example_app;
GRANT SELECT ON example_schema.trustiot_migrate_applied TO example_app;
