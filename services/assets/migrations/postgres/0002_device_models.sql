-- Device models (Laysi/trustiot-v2#99, rebuild checklist C1 and F5): a tenant's register maps,
-- which devices of one type share. The API checks every rule before it writes (services/assets
-- /src/device_models/document.rs, with crates/protocol's `DeviceModel::validate`); the CHECK
-- constraints below repeat the per-field rules so a row that breaks them cannot exist even if a
-- later code path forgot a check. Rules across rows (unique keys, no overlapping addresses) are
-- the API's, except key uniqueness, which the primary key enforces too.
--
-- Both tables hold tenant data and follow crates/service-kit/sql/tenant-table.sql: `tenant_id`
-- leads the primary key and every index and foreign key, so a row can only ever reference rows
-- of its own tenant, and `tenant_isolation` limits every role to the transaction's tenant. No
-- platform access: nothing reads models across tenants (the gateway configuration learns them
-- from the outbox events), so `assets_platform` gets no grant on either table.
--
-- The migration Job also makes the assets service an outbox producer from this release on
-- (`outbox_producer` in src/schema.rs): it creates `assets.outbox_event` and
-- `assets.outbox_aggregate` with their policies, so the API writes `assets.device_model_*`
-- events in the transaction of each change.

CREATE TABLE assets.device_models (
    tenant_id uuid NOT NULL REFERENCES assets.tenants (tenant_id),
    model_id uuid NOT NULL,
    -- crates/protocol's `ModelName`: 1-40 lowercase letters and digits in groups joined by
    -- single hyphens, starting with a letter. Never changes after the insert.
    name text NOT NULL CHECK (
        char_length(name) BETWEEN 1 AND 40
        AND name COLLATE "C" ~ '^[a-z][a-z0-9]*(-[a-z0-9]+)*$'
    ),
    -- 1 at creation, one more with every update; the events carry it.
    revision bigint NOT NULL DEFAULT 1 CHECK (revision >= 1),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, model_id),
    CONSTRAINT device_models_name_unique UNIQUE (tenant_id, name)
);

-- A model's data points, in the order the user gave them (`position`). A device table that uses
-- models (Laysi/trustiot-v2#100) references `assets.device_models (tenant_id, model_id)` with
-- `ON DELETE RESTRICT`, so a model a device uses cannot be deleted; the API answers 409 then.
CREATE TABLE assets.device_model_points (
    tenant_id uuid NOT NULL,
    model_id uuid NOT NULL,
    position integer NOT NULL CHECK (position >= 0),
    -- crates/protocol's `PointKey`.
    key text NOT NULL CHECK (key COLLATE "C" ~ '^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$'),
    -- Only the read functions: 1 coils, 2 discrete inputs, 3 holding registers, 4 input
    -- registers.
    function_code smallint NOT NULL CHECK (function_code BETWEEN 1 AND 4),
    -- The 0-based protocol address of the first coil or register.
    address integer NOT NULL CHECK (address BETWEEN 0 AND 65535),
    data_type text NOT NULL CHECK (
        data_type IN ('bool', 'u16', 'i16', 'u32', 'i32', 'u64', 'i64', 'f32', 'f64', 'string')
    ),
    byte_order text NOT NULL DEFAULT 'abcd' CHECK (byte_order IN ('abcd', 'badc', 'cdab', 'dcba')),
    -- For `string` only: the number of registers.
    length integer CHECK (length BETWEEN 1 AND 125),
    -- Rebuild checklist F5: the value is `raw * scale + value_offset` (`offset` is a reserved
    -- word). Finite; a scale of 0 would turn every reading into the offset. NaN sorts above
    -- every other value in Postgres, so the range excludes it with the infinities.
    scale double precision CHECK (
        scale BETWEEN -1.7976931348623157e308 AND 1.7976931348623157e308 AND scale <> 0
    ),
    value_offset double precision CHECK (
        value_offset BETWEEN -1.7976931348623157e308 AND 1.7976931348623157e308
    ),
    -- What the value means, for people and for the AI features that read the model.
    description text CHECK (char_length(description) BETWEEN 1 AND 512),
    PRIMARY KEY (tenant_id, model_id, key),
    UNIQUE (tenant_id, model_id, position),
    FOREIGN KEY (tenant_id, model_id)
        REFERENCES assets.device_models (tenant_id, model_id) ON DELETE CASCADE
);

ALTER TABLE assets.device_models ENABLE ROW LEVEL SECURITY;
ALTER TABLE assets.device_model_points ENABLE ROW LEVEL SECURITY;

CREATE POLICY tenant_isolation ON assets.device_models
    USING (tenant_id = NULLIF(current_setting('trustiot.tenant_id', true), '')::uuid)
    WITH CHECK (tenant_id = NULLIF(current_setting('trustiot.tenant_id', true), '')::uuid);

CREATE POLICY tenant_isolation ON assets.device_model_points
    USING (tenant_id = NULLIF(current_setting('trustiot.tenant_id', true), '')::uuid)
    WITH CHECK (tenant_id = NULLIF(current_setting('trustiot.tenant_id', true), '')::uuid);

-- The API reads, creates, updates (UPDATE takes the model's row lock and bumps its revision)
-- and deletes models. It replaces a model's points by deleting and inserting them, so it never
-- updates a point.
GRANT SELECT, INSERT, UPDATE, DELETE ON assets.device_models TO assets_app;
GRANT SELECT, INSERT, DELETE ON assets.device_model_points TO assets_app;
