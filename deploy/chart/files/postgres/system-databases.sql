-- The cluster's other databases, `postgres` and `template1`, are not for TrustIoT's roles
-- (Laysi/trustiot-v2#400). After initdb, PUBLIC may connect to both and create temporary tables
-- and functions in `postgres` (template1 gives PUBLIC CONNECT only), so a service role could use
-- them, and a session it holds open on template1 makes CREATE DATABASE fail. Revoking from
-- PUBLIC leaves only the superuser and the roles granted by name below.
--
-- The bundled Cluster runs these statements once at bootstrap (bootstrap.initdb.postInitSQL, as
-- the superuser on `postgres`). templates/postgres.yaml carries them one per item, because chart
-- templates never read chart files (scripts/ci/check-secret-handling.ts); the chart test fails
-- when the two differ. tools/migrate/tests/system_databases.rs runs this file, statement by
-- statement, on a throwaway PostgreSQL 18, and docs/runbooks/migrations.md runs it on Clusters
-- bootstrapped before it. Every statement is idempotent.
--
-- Format, which both tests read the same way: statements are separated by a blank line, a line
-- that starts with `--` is a comment, and each statement ends with `;`.

-- CloudNativePG's own roles that connect to `postgres` without being superusers
-- (deploy/cloudnative-pg/README.md): streaming_replica for pg_rewind, for the replicas' checks of
-- the primary and for the replication slots of a replica; cnpg_metrics_exporter for the
-- monitoring queries. The operator creates both when an instance first starts, after this file
-- has run, so they are created here as the operator would create them (it accepts roles that
-- already exist) and can be granted CONNECT by name. The operator's pooler role grants itself.
DO $$
BEGIN
    IF NOT EXISTS (SELECT FROM pg_catalog.pg_roles WHERE rolname = 'streaming_replica') THEN
        CREATE ROLE streaming_replica LOGIN REPLICATION;
        COMMENT ON ROLE streaming_replica IS 'Special user for streaming replication created by CloudNativePG';
    END IF;
    IF NOT EXISTS (SELECT FROM pg_catalog.pg_roles WHERE rolname = 'cnpg_metrics_exporter') THEN
        CREATE ROLE cnpg_metrics_exporter LOGIN PASSWORD NULL;
    END IF;
END
$$;

GRANT CONNECT ON DATABASE postgres TO streaming_replica, cnpg_metrics_exporter;

REVOKE CONNECT, TEMPORARY ON DATABASE postgres FROM PUBLIC;

REVOKE CONNECT, TEMPORARY ON DATABASE template1 FROM PUBLIC;
