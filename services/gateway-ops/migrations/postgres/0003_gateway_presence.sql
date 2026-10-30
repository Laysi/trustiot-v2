-- Whether each bound gateway is connected (Laysi/trustiot-v2#112; rebuild checklist C3): the
-- gateway's retained status on `up/status` (its MQTT last will included) and its heartbeats on
-- `up/heartbeat` (crates/protocol, "在線狀態與心跳"). Run by `gateway_ops_owner` like 0001; the
-- table is tenant data and follows crates/service-kit/sql/tenant-table.sql.
--
-- One row per binding, in the binding's tenant: the presence task writes a gateway's status and
-- heartbeats as `gateway_ops_app` in the tenant of the gateway's current binding, so what a
-- gateway reported while bound to one tenant never shows in another. A gateway that was never
-- heard from during its binding has no row (services/gateway-ops/README.md, "在線狀態").

CREATE TABLE gateway_ops.gateway_presence (
    tenant_id uuid NOT NULL,
    binding_id uuid NOT NULL,
    -- crates/protocol's `GatewayId`, the gateway the binding is for.
    hardware_id text NOT NULL CHECK (hardware_id COLLATE "C" ~ '^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$'),
    state text NOT NULL CHECK (state IN ('online', 'offline')),
    -- The connection the state is about (crates/protocol's `SessionId`): the latest `online`'s,
    -- or the session whose status or heartbeat set the row.
    session text NOT NULL CHECK (session COLLATE "C" ~ '^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$'),
    -- Why the gateway is offline: its own `shutdown`, the broker's last will
    -- (`connection_lost`), or no heartbeat within the time its interval allows
    -- (`heartbeat_timeout`); NULL while online.
    offline_reason text CHECK (offline_reason IN ('shutdown', 'connection_lost', 'heartbeat_timeout')),
    -- When `state` last changed, by the database clock.
    changed_at timestamptz NOT NULL,
    -- The last status or heartbeat of `session` the service received, by the database clock.
    last_seen_at timestamptz NOT NULL,
    -- From the latest `online` or heartbeat of the session (crates/protocol's `AgentVersion`).
    agent_version text CHECK (agent_version COLLATE "C" ~ '^[A-Za-z0-9][A-Za-z0-9.+_-]{0,63}$'),
    -- The latest heartbeat of the session: when it arrived, the seconds it announced until the
    -- next one, and the configuration version the gateway said it polls with (NULL before it
    -- applied any).
    last_heartbeat_at timestamptz,
    heartbeat_interval_s integer CHECK (heartbeat_interval_s >= 1),
    config_version bigint CHECK (config_version >= 1),
    -- The `sent_at` (the gateway's clock, Unix milliseconds) of the latest heartbeat of
    -- `session`; NULL before the session's first. Every replica receives every heartbeat, and
    -- one that is not newer than this is not written again (src/presence/rules.rs).
    heartbeat_sent_ms bigint CHECK (heartbeat_sent_ms >= 0),
    PRIMARY KEY (tenant_id, binding_id),
    -- Only for bindings the projection knows, in their own tenant.
    FOREIGN KEY (tenant_id, binding_id)
        REFERENCES gateway_ops.gateway_bindings (tenant_id, binding_id),
    CHECK ((state = 'offline') = (offline_reason IS NOT NULL)),
    CHECK ((last_heartbeat_at IS NULL) = (heartbeat_interval_s IS NULL))
);

-- The heartbeat timeout's search across tenants (the platform role): the online rows by when
-- they were last heard from. The third deliberate exception to "tenant_id leads every index".
CREATE INDEX gateway_presence_online
    ON gateway_ops.gateway_presence (last_seen_at)
    WHERE state = 'online';

ALTER TABLE gateway_ops.gateway_presence ENABLE ROW LEVEL SECURITY;

CREATE POLICY tenant_isolation ON gateway_ops.gateway_presence
    USING (tenant_id = NULLIF(current_setting('trustiot.tenant_id', true), '')::uuid)
    WITH CHECK (tenant_id = NULLIF(current_setting('trustiot.tenant_id', true), '')::uuid);

-- The heartbeat timeout finds the online gateways whose heartbeats stopped before it knows their
-- tenants: the platform role reads every tenant's rows, read only. It never writes; the timeout
-- is written as the service role in the row's tenant.
CREATE POLICY platform_access ON gateway_ops.gateway_presence
    FOR SELECT
    TO gateway_ops_platform
    USING (true);

-- The service role records statuses and heartbeats; the key and the gateway never change.
GRANT SELECT, INSERT ON gateway_ops.gateway_presence TO gateway_ops_app;
GRANT UPDATE (state, session, offline_reason, changed_at, last_seen_at, agent_version,
    last_heartbeat_at, heartbeat_interval_s, config_version, heartbeat_sent_ms)
    ON gateway_ops.gateway_presence TO gateway_ops_app;
GRANT SELECT ON gateway_ops.gateway_presence TO gateway_ops_platform;
