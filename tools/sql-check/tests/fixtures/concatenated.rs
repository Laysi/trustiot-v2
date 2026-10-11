// Fixture for tools/sql-check: SQL built from strings in every way the check rejects. Not
// compiled; tests/check.rs expects a finding on exactly the lines whose trailing comment names
// a rule, and of that rule.
use sqlx::{AssertSqlSafe, PgPool, Postgres, QueryBuilder, query}; // AssertSqlSafe

async fn find_device(pool: &PgPool, name: &str) {
    let sql = format!("SELECT * FROM devices WHERE name = '{name}'"); // SqlBuiltFromStrings
    sqlx::query(AssertSqlSafe(sql)).execute(pool).await.unwrap(); // AssertSqlSafe DynamicSql
}

async fn by_column(pool: &PgPool, column: &'static str) {
    query(column).execute(pool).await.unwrap(); // DynamicSql
    let COLUMN = column;
    query(COLUMN).execute(pool).await.unwrap(); // DynamicSql
}

async fn leaked(pool: &PgPool, id: &str) {
    let mut sql = String::from("DELETE FROM devices");
    sql.push_str(" WHERE id = "); // SqlBuiltFromStrings
    sql += id;
    let sql: &'static str = sql.leak(); // LeakedString
    sqlx::query_scalar::<_, i64>(sql).fetch_one(pool).await.unwrap(); // DynamicSql
}

fn filter(builder: &mut QueryBuilder<'_, Postgres>, column: &str, value: &str) {
    builder.push(" AND ");
    builder.push(column); // DynamicQueryBuilder
    builder.push(" = ").push_bind(value.to_owned());
    add_filter!(builder, column); // DynamicQueryBuilder
}

macro_rules! add_filter {
    ($builder:expr, $column:expr) => {
        $builder.push(" AND ").push($column) // DynamicQueryBuilder
    };
}

fn split_config(builder: &mut QueryBuilder<'_, Postgres>, tenant: &str) {
    builder.push("SELECT set_");
    builder.push("config('trust"); // SessionSetting
    builder.push("iot.tenant_id', ").push_bind(tenant.to_owned()).push(", true)");
}

async fn switch_tenant(pool: &PgPool) {
    sqlx::query("SET LOCAL trustiot.tenant_id = '00000000-0000-0000-0000-00000000000b'") // SessionSetting
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("SELECT pg_catalog.set_config('trustiot.tenant_id', $1, true)") // SessionSetting
        .bind("00000000-0000-0000-0000-00000000000b")
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(concat!("SELECT set_", "config('trustiot.tenant_id', $1, true)")) // SessionSetting
        .bind("00000000-0000-0000-0000-00000000000b")
        .execute(pool)
        .await
        .unwrap();
    sqlx::raw_sql(include_str!("sets-tenant.sql")).execute(pool).await.unwrap(); // SessionSetting
}

async fn generated(pool: &PgPool) {
    sqlx::raw_sql(include_str!(concat!(env!("OUT_DIR"), "/schema.sql"))) // UncheckedInclude
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(concat!("SELECT * FROM ", env!("TABLE"))).execute(pool).await.unwrap(); // DynamicSql
}

macro_rules! run_sql {
    ($pool:expr, $sql:expr) => {
        sqlx::query($sql).execute($pool) // DynamicSql
    };
}
