-- Devices (Laysi/trustiot-v2#100, rebuild checklist C2; services/assets/README.md, "設備"): the
-- Modbus endpoints a tenant's gateways poll. The API checks every rule before it writes
-- (services/assets/src/devices/document.rs, with crates/protocol's configuration rules); the
-- CHECK constraints below repeat the per-field rules, so a row that breaks them cannot exist
-- even if a later code path forgot a check. The rules across rows of one gateway (one transport
-- per converter, one device per unit ID on it) are the API's, under a per-gateway lock, with the
-- unit ID's uniqueness enforced by the table too.
--
-- Tenant data, so it follows crates/service-kit/sql/tenant-table.sql: `tenant_id` leads the
-- primary key and every index and foreign key, so a device can only reference its own tenant's
-- gateway binding and model, and `tenant_isolation` limits every role to the transaction's
-- tenant. No platform access: nothing reads devices across tenants (the gateway configuration
-- learns them from the `assets.device_*` events), so `assets_platform` gets no grant.

-- The binding a device belongs to names the gateway too, so the device's foreign key can say
-- both: the device's `hardware_id` is its binding's gateway, in its own tenant.
ALTER TABLE assets.gateway_bindings
    ADD CONSTRAINT gateway_bindings_gateway UNIQUE (tenant_id, binding_id, hardware_id);

CREATE TABLE assets.devices (
    tenant_id uuid NOT NULL REFERENCES assets.tenants (tenant_id),
    device_id uuid NOT NULL,
    -- The naming rule of models (crates/protocol's `ModelName`); unique in the tenant, never
    -- changed after the insert.
    name text NOT NULL CHECK (
        char_length(name) BETWEEN 1 AND 40
        AND name COLLATE "C" ~ '^[a-z][a-z0-9]*(-[a-z0-9]+)*$'
    ),
    -- The binding of the gateway that polls the device, current when the device was created,
    -- and that gateway's hardware ID. Never changed after the insert. A binding is never deleted
    -- (0003_gateway_pool.sql), so the reference holds; what unbinding a gateway that has devices
    -- does to them is Laysi/trustiot-v2#115's.
    binding_id uuid NOT NULL,
    hardware_id text NOT NULL,
    -- The model the gateway reads the device with. Never changed after the insert; while the
    -- device exists, the model cannot be deleted (the API answers 409 `device_model.in_use`).
    model_id uuid NOT NULL,
    -- crates/protocol's `Transport`: Modbus TCP, or RTU framing over TCP to an RS485 converter.
    transport text NOT NULL CHECK (transport IN ('modbus_tcp', 'rtu_over_tcp')),
    -- crates/protocol's `Host` as the user wrote it: an IP address or a DNS name, at most 253
    -- bytes; never spelled as a loopback address (services/assets/README.md).
    host text NOT NULL CHECK (octet_length(host) BETWEEN 1 AND 253),
    -- The host in the form that is equal for equal hosts (`Host::canonical`): devices with the
    -- same `host_key` and `port` on one gateway are behind one endpoint, such as one converter.
    host_key text NOT NULL CHECK (octet_length(host_key) BETWEEN 1 AND 253),
    port integer NOT NULL CHECK (port BETWEEN 1 AND 65535),
    -- The Modbus unit identifier: 1-247 behind an RTU-over-TCP converter, 0-255 for Modbus TCP.
    unit_id smallint NOT NULL CHECK (
        unit_id BETWEEN 0 AND 255
        AND (transport <> 'rtu_over_tcp' OR unit_id BETWEEN 1 AND 247)
    ),
    -- How often the gateway polls the device, in milliseconds: 1 s to 1 day (the API's range;
    -- a plan's shortest interval is checked on top, Laysi/trustiot-v2#116).
    poll_interval_ms integer NOT NULL CHECK (poll_interval_ms BETWEEN 1000 AND 86400000),
    -- 1 at creation, one more with every update; the events carry it.
    revision bigint NOT NULL DEFAULT 1 CHECK (revision >= 1),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, device_id),
    CONSTRAINT devices_name_unique UNIQUE (tenant_id, name),
    -- One device per unit ID on one endpoint of a gateway: two would answer the same requests.
    CONSTRAINT devices_unit_unique UNIQUE (tenant_id, hardware_id, host_key, port, unit_id),
    CONSTRAINT devices_binding FOREIGN KEY (tenant_id, binding_id, hardware_id)
        REFERENCES assets.gateway_bindings (tenant_id, binding_id, hardware_id),
    -- RESTRICT: deleting a model a device uses fails with SQLSTATE 23001, which the model API
    -- answers with 409 `device_model.in_use` (0002_device_models.sql).
    CONSTRAINT devices_model FOREIGN KEY (tenant_id, model_id)
        REFERENCES assets.device_models (tenant_id, model_id) ON DELETE RESTRICT
);

-- The foreign keys' lookups from the referenced side (deleting a model checks for devices).
CREATE INDEX devices_by_model ON assets.devices (tenant_id, model_id);

ALTER TABLE assets.devices ENABLE ROW LEVEL SECURITY;

CREATE POLICY tenant_isolation ON assets.devices
    USING (tenant_id = NULLIF(current_setting('trustiot.tenant_id', true), '')::uuid)
    WITH CHECK (tenant_id = NULLIF(current_setting('trustiot.tenant_id', true), '')::uuid);

-- The API reads, creates, updates (UPDATE also takes the device's row lock) and deletes devices.
GRANT SELECT, INSERT, UPDATE, DELETE ON assets.devices TO assets_app;
