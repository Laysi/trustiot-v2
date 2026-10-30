//! Sets a tenant in a platform transaction, which no code of the service may do: the attack the
//! restrictive policy `platform_expired_only` of `0002_conversation_threads.sql` stops. Kept in
//! its own file so that tools/sql-check waives its `SessionSetting` rule here and nowhere else in
//! the assistant's tests.

use contract::TenantId;
use sqlx::PgConnection;

/// Sets `trustiot.tenant_id` to `tenant` for the rest of the transaction on `connection`.
pub async fn set_tenant(connection: &mut PgConnection, tenant: TenantId) {
    sqlx::query("SELECT set_config('trustiot.tenant_id', $1, true)")
        .bind(tenant.as_uuid().to_string())
        .execute(connection)
        .await
        .expect("the platform role sets a tenant setting");
}
