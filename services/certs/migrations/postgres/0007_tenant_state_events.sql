-- The certs service's copy of each tenant's state, now kept from the identity service's events
-- (Laysi/trustiot-v2#778, rebuild checklist B1; services/certs/README.md, "租戶狀態"). Run by
-- `certs_owner` like the files before it, in one transaction.
--
-- From this release the migration Job also makes `certs_platform` a reader of
-- `identity.outbox_event` (tools/migrate/src/registry.rs): the consumer of
-- `identity.tenant_created` and `identity.tenant_state_changed` writes `tenant_statuses` in the
-- event's tenant transaction as `certs_app`, through the same versioned insert as before
-- (`bindings::store_tenant_status`). Its subscription reads the identity service's log from the
-- start, so every tenant created before this release gets its row.
--
-- Two more states:
--
-- - `deleted`: the tenant's contract ended and its data is being or was deleted
--   (`contract::events::identity::TenantState::Deleted`, Laysi/trustiot-v2#109). Final.
-- - `unrecognized`: a state a newer identity service wrote that this build does not know
--   (`TenantState::Unrecognized`). Stored as such rather than as `suspended`, so the row says
--   what was seen; the store accepts a known state at the same version over it, so replaying
--   the subscription after an upgrade replaces it with the state the new build knows.
--
-- The service reads every state but `active`, and a tenant without a row, as suspended: it does
-- not renew their gateways' certificates (fail closed, the user's decision of 2026-10-30). A
-- build before this file treats the two new values as a broken row and refuses the renewal too
-- (503), so after rolling the service back a deleted or unrecognized tenant still gets nothing;
-- only a tenant without a row reads as active again, as it did before this release.
ALTER TABLE certs.tenant_statuses
    DROP CONSTRAINT tenant_statuses_status_check,
    ADD CONSTRAINT tenant_statuses_status_check
        CHECK (status IN ('active', 'suspended', 'deleted', 'unrecognized'));
