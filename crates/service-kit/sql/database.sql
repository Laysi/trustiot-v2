-- Template: run once per database by its owner (the migration role) before any service
-- connects (decision 29). tests/tenant_db.rs runs this file as it is.
--
-- No temporary tables: they are not subject to row-level security and live as long as the
-- pooled session, so a tenant transaction could leave rows in one for the next transaction on the
-- same connection, of any tenant. Postgres grants TEMPORARY to PUBLIC by default; service-kit
-- refuses sessions that still have it.
DO $$
BEGIN
    EXECUTE format('REVOKE TEMPORARY ON DATABASE %I FROM PUBLIC', current_database());
END
$$;
