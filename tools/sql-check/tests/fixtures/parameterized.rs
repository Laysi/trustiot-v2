// Fixture for tools/sql-check: parameterized SQL that the check accepts. Not compiled.
use sqlx::{PgPool, Postgres, QueryBuilder};

const BY_NAME: &str = "SELECT id FROM devices WHERE name = $1";
const SCHEMA: &str = include_str!("schema.sql");
const BY_MODEL: &str = concat!("SELECT id FROM devices ", "WHERE model = $1");

async fn find_device(pool: &PgPool, name: &str) {
    sqlx::query(BY_NAME).bind(name).execute(pool).await.unwrap();
    sqlx::query_as::<_, (i64,)>("SELECT id FROM devices WHERE name = $1 AND model = $2")
        .bind(name)
        .bind("pi-5")
        .fetch_all(pool)
        .await
        .unwrap();
    let count: i64 = sqlx::query_scalar!("SELECT count(*) AS \"count!\" FROM devices")
        .fetch_one(pool)
        .await
        .unwrap();
    tracing::info!(count, "counted devices: {}", format!("{count} rows"));
}

fn insert_many(names: &[String]) -> QueryBuilder<'_, Postgres> {
    let mut builder = QueryBuilder::new("INSERT INTO devices (name, model) ");
    builder.push_values(names, |mut row, name| {
        row.push_bind(name).push_bind("pi-5");
    });
    builder.push(" RETURNING id");
    builder
}

fn filter(builder: &mut QueryBuilder<'_, Postgres>, value: &str) {
    builder.push(" AND name = ").push_bind(value.to_owned());
    let mut ids = builder.separated(", ");
    ids.push_bind(1_i64);
    ids.push_bind(2_i64);
}

async fn read_tenant(pool: &PgPool) -> Option<String> {
    sqlx::query_scalar("SELECT current_setting('trustiot.tenant_id', true)")
        .fetch_one(pool)
        .await
        .unwrap()
}
