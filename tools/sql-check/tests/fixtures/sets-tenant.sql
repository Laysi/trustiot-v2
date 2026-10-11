-- Fixture for tools/sql-check: an included file that switches the tenant.
SELECT set_config('trustiot.tenant_id', '00000000-0000-0000-0000-00000000000b', false);
