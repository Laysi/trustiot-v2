-- The unit of a data point (Laysi/trustiot-v2#982; services/assets/README.md, "設備型號"): what
-- the device page writes after a value and on its chart, such as `°C` or `%RH`. Platform
-- metadata only: the gateway's configuration and the `assets.device_model_*` events carry
-- crates/protocol's data points, which have no unit, so a gateway never sees it.
--
-- Nullable without a default, so adding it rewrites no row: every existing point keeps its
-- values and has no unit. As for `description`, the API checks every rule before it writes
-- (trimmed, no control or invisible formatting characters, at most `MAX_UNIT_CHARS`, which is
-- crates/contract's limit for the unit of a dashboard component); the CHECK repeats the length
-- so a row that breaks it cannot exist. The service role's table-level grants
-- (0002_device_models.sql: SELECT, INSERT, DELETE) cover the new column.
--
-- Forward compatible: the previous release's code names its columns, so it reads and writes
-- points without the unit (a model it replaces loses its units, which an operator sets again).

ALTER TABLE assets.device_model_points
    ADD COLUMN unit text CONSTRAINT device_model_points_unit_length
        CHECK (char_length(unit) BETWEEN 1 AND 24);
