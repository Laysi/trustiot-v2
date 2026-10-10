CREATE TABLE IF NOT EXISTS dev_seed.telemetry
(
    machine_id String,
    ts DateTime('UTC'),
    metric LowCardinality(String),
    value Float64
)
ENGINE = MergeTree
ORDER BY (machine_id, metric, ts)
