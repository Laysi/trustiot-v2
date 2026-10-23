-- AI gateway schema (Laysi/trustiot-v2#605; decisions 28 and 29): the platform administrator's
-- overrides of the model roles' model lists, the settings generation every forwarded request
-- reads, and the append-only log of every change (services/ai-gateway/README.md, 「資料庫」). Run
-- by the migration role `ai_gateway_owner`, which owns everything here; the service role
-- `ai_gateway_app` owns nothing (crates/service-kit/README.md, "角色與 policy 範本"). The migration
-- Job (tools/migrate) creates both roles and the schema `ai_gateway` owned by `ai_gateway_owner`,
-- and applies these files in order as `ai_gateway_owner`, each in one transaction. No platform
-- role and no outbox: the gateway reads no other service's data and sends no events.
--
-- None of these tables holds tenant data (no `tenant_id`): the settings are the deployment's, so
-- row-level security does not apply. The chart's `ai.roles` still decides which roles exist,
-- their protocol, limits and default models; a row here only replaces a role's model list until
-- it is reset, and then stays without models to keep the role's version.
--
-- Grants are the least the service needs (src/store.rs):
-- - role_models: SELECT to load the overrides; INSERT, and UPDATE of the columns a change sets
--   (`models`, `version`, `updated_at`, `updated_by`, never `role`), to set or reset one (an
--   upsert). No DELETE or TRUNCATE: a reset keeps the row, so a role's version never goes back.
-- - settings_generation: SELECT, which every forwarded request runs once, and UPDATE of
--   `generation`, which every change increments first so that writers queue on the row lock.
-- - role_model_changes: SELECT and INSERT only, so the service can neither rewrite nor remove a
--   past change, whatever its code does (append-only, Laysi/trustiot-v2#537).
--
-- Triggers hold every role, the owner included, to the order the gateway relies on: an UPDATE
-- must raise the generation and a role's version. Restoring a backup writes rows anew (INSERT or
-- COPY), which the triggers do not stop; a gateway replica then adopts the restored settings
-- (services/ai-gateway/README.md, 「多副本」).

GRANT USAGE ON SCHEMA ai_gateway TO ai_gateway_app;

-- The settings generation: one row whose number goes up by one with every change and reset, in
-- the change's own transaction. A gateway replica keeps the settings it loaded together with the
-- generation they belong to, compares that with this row on every forwarded request, and loads
-- the settings again only when the row differs. A change also takes this number as its version,
-- so versions only go up, a reset included.
CREATE TABLE ai_gateway.settings_generation (
    -- Always true: the primary key allows one row.
    singleton boolean PRIMARY KEY DEFAULT true CHECK (singleton),
    generation bigint NOT NULL CHECK (generation >= 0)
);

INSERT INTO ai_gateway.settings_generation (singleton, generation) VALUES (true, 0);

REVOKE ALL ON ai_gateway.settings_generation FROM PUBLIC;
GRANT SELECT ON ai_gateway.settings_generation TO ai_gateway_app;
GRANT UPDATE (generation) ON ai_gateway.settings_generation TO ai_gateway_app;

-- An UPDATE must raise the generation. An invoker's function with a pinned search_path, like
-- certs.gateway_registrations_guard; ERRCODE 42501 (insufficient_privilege).
CREATE FUNCTION ai_gateway.settings_generation_rises() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, pg_temp
    AS $$
BEGIN
    IF NEW.generation <= OLD.generation OR NEW.singleton IS DISTINCT FROM OLD.singleton THEN
        RAISE EXCEPTION 'the settings generation only goes up (% to %)',
            OLD.generation, NEW.generation
            USING ERRCODE = 'insufficient_privilege';
    END IF;
    RETURN NEW;
END;
$$;

REVOKE ALL ON FUNCTION ai_gateway.settings_generation_rises() FROM PUBLIC;

CREATE TRIGGER settings_generation_rises
    BEFORE UPDATE ON ai_gateway.settings_generation
    FOR EACH ROW EXECUTE FUNCTION ai_gateway.settings_generation_rises();

-- One row per role whose model list the platform administrator ever changed: the models in
-- fallback order, each `<provider>/<model>`, or NULL after a reset (the role uses its configured
-- models). A reset keeps the row and its new version: were the row deleted, the role's version
-- would read 0 again, and a change based on a read from before an intervening change and reset
-- would pass the version check. The gateway checks every list against its configuration when it
-- writes one and again whenever it loads them; one that no longer fits (a provider removed, the
-- role's protocol changed in the chart) is ignored, and the role uses its configured models. The
-- checks here only keep the shape the gateway writes.
CREATE TABLE ai_gateway.role_models (
    -- A role name as the gateway's configuration spells it (services/ai-gateway/src/config.rs).
    role text PRIMARY KEY CHECK (role ~ '^[a-z][a-z0-9_]{0,31}$'),
    models text[] CHECK (
        models IS NULL
        OR (array_ndims(models) = 1
            AND cardinality(models) BETWEEN 1 AND 8
            AND array_position(models, NULL) IS NULL)
    ),
    -- The settings generation of the change that wrote this row.
    version bigint NOT NULL CHECK (version > 0),
    -- When it was written, by the database clock, and who asked for it.
    updated_at timestamptz NOT NULL DEFAULT now(),
    updated_by text NOT NULL CHECK (char_length(updated_by) BETWEEN 1 AND 128)
);

REVOKE ALL ON ai_gateway.role_models FROM PUBLIC;
GRANT SELECT, INSERT ON ai_gateway.role_models TO ai_gateway_app;
GRANT UPDATE (models, version, updated_at, updated_by) ON ai_gateway.role_models TO ai_gateway_app;

-- An UPDATE must raise the role's version and keep the role (the version check of a change relies
-- on both).
CREATE FUNCTION ai_gateway.role_models_version_rises() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, pg_temp
    AS $$
BEGIN
    IF NEW.version <= OLD.version OR NEW.role IS DISTINCT FROM OLD.role THEN
        RAISE EXCEPTION 'the version of model role % only goes up (% to %)',
            OLD.role, OLD.version, NEW.version
            USING ERRCODE = 'insufficient_privilege';
    END IF;
    RETURN NEW;
END;
$$;

REVOKE ALL ON FUNCTION ai_gateway.role_models_version_rises() FROM PUBLIC;

CREATE TRIGGER role_models_version_rises
    BEFORE UPDATE ON ai_gateway.role_models
    FOR EACH ROW EXECUTE FUNCTION ai_gateway.role_models_version_rises();

-- Every change and reset, written in the same transaction as the change itself, never updated or
-- deleted. Keyed by the change's version, which is the settings generation it created, so no
-- sequence is needed.
CREATE TABLE ai_gateway.role_model_changes (
    version bigint PRIMARY KEY CHECK (version > 0),
    role text NOT NULL CHECK (role ~ '^[a-z][a-z0-9_]{0,31}$'),
    -- `set`: the role's models became `models`; `reset`: the override was cleared and the role
    -- went back to its configured models.
    action text NOT NULL CHECK (action IN ('set', 'reset')),
    models text[] CHECK (
        (action = 'set'
            AND models IS NOT NULL
            AND array_ndims(models) = 1
            AND cardinality(models) BETWEEN 1 AND 8
            AND array_position(models, NULL) IS NULL)
        OR (action = 'reset' AND models IS NULL)
    ),
    changed_at timestamptz NOT NULL DEFAULT now(),
    changed_by text NOT NULL CHECK (char_length(changed_by) BETWEEN 1 AND 128)
);

REVOKE ALL ON ai_gateway.role_model_changes FROM PUBLIC;
GRANT SELECT, INSERT ON ai_gateway.role_model_changes TO ai_gateway_app;
