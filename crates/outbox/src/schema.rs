//! The outbox tables.
//!
//! Every statement names its tables without a schema, so they live in the first schema on the
//! connection's `search_path`: each service's own schema (decision 4). The DDL is idempotent;
//! the migration job runs it on every deployment, and later changes are added as further
//! idempotent statements, never by editing what is already here.

use sqlx::PgConnection;

/// Tables of a producing service: the event log and the per-aggregate sequence.
///
/// - `outbox_aggregate` holds, per aggregate, the last sequence number and the ordering
///   transaction of its last event. Writing an event updates this row, which serializes the
///   writers of one aggregate and carries the order forward (see the README). Its `tenant_id`
///   column is not here: it comes with its row-level security in service-kit's
///   `sql/outbox-aggregate.sql`, in one transaction, because service-kit refuses the sessions of
///   a role that may use a table with a `tenant_id` column and no row-level security
///   (Laysi/trustiot-v2#413). [`crate::publish`] needs that column.
/// - `outbox_event` is the append-only log. `tenant_id` duplicates the envelope's tenant so the
///   row-level security policies of decision 29 can filter on it.
///
/// A producer runs [`install_producer`], then service-kit's `sql/outbox-event.sql` and
/// `sql/outbox-aggregate.sql` once each with its roles; the migration job does all three.
pub const PRODUCER_DDL: &str = r"
CREATE TABLE IF NOT EXISTS outbox_aggregate (
    aggregate_type text NOT NULL,
    aggregate_id text NOT NULL,
    last_seq bigint NOT NULL,
    last_tx bigint NOT NULL,
    PRIMARY KEY (aggregate_type, aggregate_id)
);

CREATE TABLE IF NOT EXISTS outbox_event (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    tx_order bigint NOT NULL,
    event_id uuid NOT NULL UNIQUE,
    event_type text NOT NULL,
    tenant_id uuid,
    aggregate_type text NOT NULL,
    aggregate_id text NOT NULL,
    aggregate_seq bigint NOT NULL,
    envelope jsonb NOT NULL,
    stored_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    UNIQUE (aggregate_type, aggregate_id, aggregate_seq)
);

CREATE INDEX IF NOT EXISTS outbox_event_position ON outbox_event (tx_order, id);
";

/// Tables of a consuming service: where each subscription has read up to, which events it has
/// processed, which it gave up on, and the versions notification-mode consumers have stored.
pub const CONSUMER_DDL: &str = r"
CREATE TABLE IF NOT EXISTS outbox_subscription (
    name text PRIMARY KEY,
    source_schema text NOT NULL,
    position_tx bigint NOT NULL,
    position_id bigint NOT NULL,
    updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS outbox_inbox (
    subscription text NOT NULL,
    event_id uuid NOT NULL,
    position_tx bigint NOT NULL,
    position_id bigint NOT NULL,
    processed_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (subscription, event_id)
);

CREATE INDEX IF NOT EXISTS outbox_inbox_position
    ON outbox_inbox (subscription, position_tx, position_id);

CREATE TABLE IF NOT EXISTS outbox_dead_letter (
    subscription text NOT NULL,
    event_id uuid NOT NULL,
    position_tx bigint NOT NULL,
    position_id bigint NOT NULL,
    reason text NOT NULL,
    failed_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (subscription, event_id)
);

CREATE TABLE IF NOT EXISTS outbox_version (
    scope text NOT NULL,
    aggregate_type text NOT NULL,
    aggregate_id text NOT NULL,
    version bigint NOT NULL,
    PRIMARY KEY (scope, aggregate_type, aggregate_id)
);
";

/// Creates the producer tables in the first schema of the connection's `search_path`.
///
/// # Errors
///
/// When a statement fails, for example for lack of the `CREATE` privilege on the schema.
pub async fn install_producer(conn: &mut PgConnection) -> Result<(), sqlx::Error> {
    sqlx::raw_sql(PRODUCER_DDL).execute(conn).await.map(|_| ())
}

/// Creates the consumer tables in the first schema of the connection's `search_path`.
///
/// # Errors
///
/// When a statement fails, for example for lack of the `CREATE` privilege on the schema.
pub async fn install_consumer(conn: &mut PgConnection) -> Result<(), sqlx::Error> {
    sqlx::raw_sql(CONSUMER_DDL).execute(conn).await.map(|_| ())
}

/// Whether `name` can be used as a schema name in `search_path` without quoting: lower case
/// ASCII letters, digits, and underscores, starting with a letter or underscore, at most 63
/// bytes (Postgres' identifier limit).
#[must_use]
pub fn is_schema_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 63
        && (bytes[0].is_ascii_lowercase() || bytes[0] == b'_')
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'_')
        && !name.starts_with("pg_")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_names_need_no_quoting() {
        for name in ["identity", "field_context", "_x", "s1"] {
            assert!(is_schema_name(name), "{name}");
        }
        for name in [
            "",
            "Identity",
            "1s",
            "a-b",
            "a b",
            "a,b",
            "pg_catalog",
            "a\"b",
        ] {
            assert!(!is_schema_name(name), "{name}");
        }
        assert!(!is_schema_name(&"a".repeat(64)));
    }
}
