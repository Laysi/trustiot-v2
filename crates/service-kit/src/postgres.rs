//! Postgres connections for services (decision 29; M3 in `docs/requirements/rebuild-checklist.md`).
//!
//! - Every connection verifies the server's certificate chain and host name (`verify-full`).
//!   [`SslMode`] is the configuration type of `TRUSTIOT_POSTGRES_SSLMODE` and accepts nothing
//!   else, so a service configured otherwise refuses to start; [`connect_lazy`] refuses
//!   connection options that do not verify the server.
//! - The chain must lead to the CA the service is configured with
//!   ([`PostgresConfig::ca_file`], required) and to nothing else: sqlx trusts no public or system
//!   roots, because the workspace replaces its `webpki-roots` with an empty list
//!   (`crates/webpki-roots-empty`, Laysi/trustiot-v2#272). Options without a root certificate
//!   therefore reach no server at all.
//! - Every new session is checked before the pool hands it out: the role must not be a
//!   superuser, bypass row-level security, or own (or belong to the owner of) any table, and
//!   every table it can reach that has a `tenant_id` column must be protected by row-level
//!   security. A platform role's session must also have, for every command it may run on such a
//!   table, a policy that opens the table to it beyond the tenant condition. A session that fails
//!   the check is closed, so a misconfigured role or a migration that forgot `ENABLE ROW LEVEL
//!   SECURITY` makes the service unready instead of leaking rows ([`UnsafeSession`]).
//! - Every session is checked again when it goes back to the pool: one left with a
//!   session-level tenant (`set_config(..., false)` or `SET`, which outlive the transaction) is
//!   closed instead, so the next user never starts in that tenant.
//!
//! Tenant-scoped transactions on top of these pools are in `tenant` (`contract` feature).

use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use serde::Deserialize;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions, PgSslMode};
use sqlx::{PgConnection, PgPool};

/// The custom setting that holds the tenant of the current transaction. The kit sets it with
/// `set_config(..., true)`, which is `SET LOCAL`: it lasts until the transaction ends, so a
/// pooled connection never carries it into the next transaction. Only the `tenant` module may
/// set it; `tools/sql-check` rejects SQL anywhere else that sets or resets it.
pub const TENANT_SETTING: &str = "trustiot.tenant_id";

/// The tenant condition every row-level security policy on a tenant table uses. Without a
/// tenant set, the setting is absent or empty, the right-hand side is NULL, and no row matches.
pub const TENANT_CONDITION: &str =
    "tenant_id = NULLIF(current_setting('trustiot.tenant_id', true), '')::uuid";

/// The `sslmode` a service connects with: always `verify-full`, which checks the server's
/// certificate chain against the trusted CAs and its host name (checklist M3,
/// Laysi/trustiot#109). The Helm chart accepts only this value too
/// (`postgres.external.sslMode`).
///
/// As a configuration type it accepts only the string `verify-full`. Declare the platform
/// setting with it, and a deployment that asks for less refuses to start:
///
/// ```
/// use serde::Deserialize;
/// use service_kit::postgres::SslMode;
///
/// #[derive(Debug, Deserialize)]
/// struct Platform {
///     postgres_sslmode: SslMode, // TRUSTIOT_POSTGRES_SSLMODE
/// }
///
/// assert!("verify-full".parse::<SslMode>().is_ok());
/// for weaker in ["disable", "allow", "prefer", "require", "verify-ca", "VERIFY-FULL"] {
///     assert!(weaker.parse::<SslMode>().is_err());
/// }
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SslMode {
    /// Verify the certificate chain and the host name.
    #[default]
    VerifyFull,
}

impl SslMode {
    /// The libpq name of the mode.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::VerifyFull => "verify-full",
        }
    }
}

impl fmt::Display for SslMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for SslMode {
    type Err = InsecureSslMode;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        if text == Self::VerifyFull.as_str() {
            Ok(Self::VerifyFull)
        } else {
            Err(InsecureSslMode)
        }
    }
}

impl<'de> Deserialize<'de> for SslMode {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct SslModeVisitor;

        impl serde::de::Visitor<'_> for SslModeVisitor {
            type Value = SslMode;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("the string \"verify-full\"")
            }

            fn visit_str<E: serde::de::Error>(self, text: &str) -> Result<SslMode, E> {
                // The message never repeats the configured value.
                text.parse()
                    .map_err(|error: InsecureSslMode| E::custom(error))
            }
        }

        deserializer.deserialize_str(SslModeVisitor)
    }
}

/// An `sslmode` other than `verify-full`. The message does not repeat the value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error(
    "only `verify-full` is accepted: services always verify the database server's certificate \
     and host name (checklist M3)"
)]
pub struct InsecureSslMode;

/// Where and as whom a service connects. The host, port, database, `sslmode`, and CA file come
/// from the platform settings `TRUSTIOT_POSTGRES_{HOST,PORT,DATABASE,SSLMODE,CA_FILE}`; the role
/// and its password are the service's own credentials. `Debug` does not print the password.
#[derive(Clone)]
pub struct PostgresConfig {
    /// Host name or IP address; the server certificate must name it.
    pub host: String,
    /// TCP port.
    pub port: u16,
    /// Database name.
    pub database: String,
    /// Always [`SslMode::VerifyFull`]; a field so the configuration states it.
    pub ssl_mode: SslMode,
    /// PEM file with the CA certificates that sign the server certificate. They are the only
    /// trust anchors: no public or system root is trusted, so a service without this setting
    /// refuses to start (declare `TRUSTIOT_POSTGRES_CA_FILE` without `Option`). Read on every
    /// new connection, so a replaced file takes effect without a restart. The chart mounts it at
    /// `/etc/trustiot/postgres-ca/ca.crt`.
    pub ca_file: PathBuf,
    /// The database role.
    pub username: String,
    /// The role's password.
    pub password: String,
}

impl fmt::Debug for PostgresConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PostgresConfig")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("database", &self.database)
            .field("ssl_mode", &self.ssl_mode)
            .field("ca_file", &self.ca_file)
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .finish()
    }
}

impl PostgresConfig {
    /// sqlx connection options that verify the server against [`Self::ca_file`] only. Settings
    /// from `PG*` environment variables that these fields cover (including `PGSSLROOTCERT`) are
    /// overridden.
    #[must_use]
    pub fn connect_options(&self) -> PgConnectOptions {
        PgConnectOptions::new_without_pgpass()
            .host(&self.host)
            .port(self.port)
            .database(&self.database)
            .username(&self.username)
            .password(&self.password)
            .ssl_mode(match self.ssl_mode {
                SslMode::VerifyFull => PgSslMode::VerifyFull,
            })
            .ssl_root_cert(&self.ca_file)
    }
}

/// Connection options that would not verify the server.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum InsecureConnection {
    /// The options' `sslmode` is weaker than `verify-full`.
    #[error(
        "the database connection must use sslmode verify-full: services always verify the \
         database server's certificate and host name (checklist M3)"
    )]
    SslMode,
    /// The options connect through a Unix socket, which does not use TLS.
    #[error("the database connection must use TCP with TLS, not a Unix socket (checklist M3)")]
    UnixSocket,
}

/// Checks that `options` verify the server: `sslmode` `verify-full` over TCP.
///
/// # Errors
///
/// When they do not.
pub fn require_verify_full(options: &PgConnectOptions) -> Result<(), InsecureConnection> {
    if options.get_socket().is_some() {
        return Err(InsecureConnection::UnixSocket);
    }
    if !matches!(options.get_ssl_mode(), PgSslMode::VerifyFull) {
        return Err(InsecureConnection::SslMode);
    }
    Ok(())
}

/// Which role a pool connects as; the session check allows the platform role the policies
/// granted to it by name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionRole {
    /// The service role: every policy that applies to it must be the tenant condition.
    Service,
    /// The platform role: policies granted to it by name (`platform_access`) may open a table
    /// across tenants; policies for every role (`PUBLIC`) must still be the tenant condition.
    /// Where it, or a role it can become, may run a command on a table with row-level security,
    /// a policy that opens the table beyond the session's tenant must apply to that role for
    /// that command ([`UnsafeSession::PlatformTenantOnly`]).
    Platform,
}

/// A service-role pool whose connections verify the server and whose sessions pass
/// [`check_session`] before they are used. Connections are opened on first use, so the service
/// starts while the database is down; register a readiness check that runs a query on the pool.
///
/// A refused session is closed and logged at ERROR (`refused a database session that row-level
/// security would not protect`, with the `problem`). sqlx keeps retrying new sessions until the
/// pool's acquire timeout, so a query on such a pool fails with
/// [`sqlx::Error::PoolTimedOut`]; `tenant::TenantDb` reports it as the [`UnsafeSession`].
///
/// A session that goes back to the pool with a session-level tenant setting (a transaction-level
/// one ends with its transaction) is closed rather than kept, and logged at ERROR (`closed a
/// database session left with a session-level tenant`): its next user would otherwise start in
/// that tenant, including transactions meant to have none. The check is one query per release.
///
/// The kit sets `pool`'s `after_connect` and `after_release` callbacks; ones set earlier are
/// replaced.
///
/// The server certificate must chain to the root certificate in `options`
/// ([`PostgresConfig::connect_options`] sets it from [`PostgresConfig::ca_file`]); no public or
/// system root is trusted, so options without one never get a connection.
///
/// # Errors
///
/// When `options` would not verify the server ([`require_verify_full`]).
pub fn connect_lazy(
    options: PgConnectOptions,
    pool: PgPoolOptions,
) -> Result<PgPool, InsecureConnection> {
    connect_lazy_recording(options, pool, SessionRole::Service).map(|(pool, _refusals)| pool)
}

/// The last session a pool refused, cleared when a session passes the check.
#[derive(Clone, Debug, Default)]
pub(crate) struct Refusals(Arc<Mutex<Option<UnsafeSession>>>);

impl Refusals {
    fn record(&self, outcome: Option<UnsafeSession>) {
        if let Ok(mut last) = self.0.lock() {
            *last = outcome;
        }
    }

    /// The problem of the last refused session, if the last new session was refused.
    #[cfg_attr(
        not(feature = "contract"),
        allow(
            dead_code,
            reason = "read by the tenant module, which needs `contract`"
        )
    )]
    pub(crate) fn last(&self) -> Option<UnsafeSession> {
        self.0.lock().ok().and_then(|last| last.clone())
    }
}

/// [`connect_lazy`] for `role`, also returning the pool's refusals.
pub(crate) fn connect_lazy_recording(
    options: PgConnectOptions,
    pool: PgPoolOptions,
    role: SessionRole,
) -> Result<(PgPool, Refusals), InsecureConnection> {
    require_verify_full(&options)?;
    let refusals = Refusals::default();
    let recorder = refusals.clone();
    let pool = pool
        .after_connect(move |connection, _metadata| {
            let recorder = recorder.clone();
            Box::pin(async move { check_new_session(connection, role, &recorder).await })
        })
        .after_release(|connection, _metadata| Box::pin(keep_released_session(connection)))
        .connect_lazy_with(options);
    Ok((pool, refusals))
}

/// Whether a session going back to the pool has no tenant setting left. The query runs after
/// whatever sqlx queued on the connection (the `ROLLBACK` of a dropped transaction), so it sees
/// the session as its next user would. A session with a tenant is kept out of the pool (closed);
/// a failed query closes it too.
async fn keep_released_session(connection: &mut PgConnection) -> Result<bool, sqlx::Error> {
    let clean: bool = sqlx::query_scalar(NO_SESSION_TENANT)
        .fetch_one(connection)
        .await?;
    if !clean {
        // Only service-kit's transactions may choose a tenant, and only for one transaction.
        tracing::error!("closed a database session left with a session-level tenant");
    }
    Ok(clean)
}

/// True when the session has no tenant setting outside a transaction.
const NO_SESSION_TENANT: &str =
    "SELECT NULLIF(current_setting('trustiot.tenant_id', true), '') IS NULL";

async fn check_new_session(
    connection: &mut PgConnection,
    role: SessionRole,
    refusals: &Refusals,
) -> Result<(), sqlx::Error> {
    match check_session(connection, role).await? {
        Ok(()) => {
            refusals.record(None);
            Ok(())
        }
        Err(problem) => {
            tracing::error!(
                problem = %problem,
                "refused a database session that row-level security would not protect"
            );
            refusals.record(Some(problem.clone()));
            Err(sqlx::Error::Configuration(Box::new(problem)))
        }
    }
}

/// Why a database session cannot be trusted with tenant data (decision 29). Each one either lets
/// the session bypass row-level security or leaves tenant rows unprotected.
///
/// Tenant data is a relation with a `tenant_id` column, or a view or materialized view built on
/// one, directly or through other views.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum UnsafeSession {
    /// The role is a superuser or has `BYPASSRLS`, or belongs to a role that is or has.
    #[error(
        "the database role is a superuser or bypasses row-level security, itself or through a \
         role it belongs to"
    )]
    BypassesRls,
    /// The session starts with a tenant set, from a role or database default or the connection
    /// options; queries outside a tenant transaction would see that tenant's rows.
    #[error(
        "the database session starts with trustiot.tenant_id set; remove the role, database, or \
         connection default"
    )]
    TenantPreset,
    /// The role owns, or belongs to the role that owns, a relation. Owners are exempt from
    /// row-level security and can turn it off; relations belong to the migration role.
    #[error(
        "the database role owns, or belongs to the owner of, `{0}`; relations must belong to the \
         migration role"
    )]
    OwnsRelation(String),
    /// A table the role can use has a `tenant_id` column but row-level security is off.
    #[error("table `{0}` has a tenant_id column but row-level security is not enabled")]
    UnprotectedTable(String),
    /// A permissive policy that applies to the role is not exactly the tenant condition
    /// ([`TENANT_CONDITION`]), so it may let the role see or write other tenants' rows. Names
    /// the table and the policy.
    #[error(
        "{0} applies to the database role and is not exactly the tenant condition, so it may \
         expose other tenants' rows"
    )]
    PermissivePolicy(String),
    /// A view the role can use reads tenant data but runs with its owner's rights, so the
    /// owner's exemption from row-level security applies.
    #[error("view `{0}` reads tenant data but is not security_invoker")]
    UnprotectedView(String),
    /// A materialized view or foreign table the role can use holds tenant data; row-level
    /// security cannot protect either.
    #[error(
        "`{0}` holds tenant data and is a materialized view or foreign table, which row-level \
         security cannot protect"
    )]
    Unprotectable(String),
    /// The role may run a `SECURITY DEFINER` function owned by another role. The function runs
    /// with its owner's rights and row-level security visibility (an owner of tenant data, a
    /// superuser, or a role with cross-tenant policies sees other tenants), so the role must
    /// not be able to call any.
    #[error(
        "the database role may execute SECURITY DEFINER function `{0}`, which runs with another \
         role's rights and row-level security visibility"
    )]
    DefinerFunction(String),
    /// The role may create temporary tables in the database. They are not subject to row-level
    /// security and outlive the transaction on a pooled session, so one tenant's rows could
    /// reach the next transaction on the connection (`sql/database.sql` revokes the right).
    #[error(
        "the database role may create temporary tables, which row-level security does not cover \
         and which outlive the transaction on a pooled connection"
    )]
    TemporaryTables,
    /// The role may `TRUNCATE` a tenant table; `TRUNCATE` is not subject to row-level security.
    #[error("the database role may TRUNCATE tenant table `{0}`, which row-level security ignores")]
    TruncateAllowed(String),
    /// Platform sessions only ([`SessionRole::Platform`]): `role`, the platform role or a role it
    /// can become with `SET ROLE`, may run `command` (`SELECT`, `INSERT`, `UPDATE`, or `DELETE`)
    /// on `table`, which has row-level security, but the permissive policies that apply to `role`
    /// for that command open only the session's tenant: each is the tenant condition, reads
    /// `trustiot.tenant_id` some other way, or is `false`. `PlatformDb` never sets a tenant, so
    /// the grant gives it nothing; a platform session that set `trustiot.tenant_id` itself would
    /// reach that tenant's rows. Revoke the grant, or add a policy for the platform role by name
    /// (`platform_access`).
    #[error(
        "database role `{role}` may {command} on `{table}`, but the only permissive policies that \
         apply to it for {command} are the tenant condition, so a platform session that sets \
         trustiot.tenant_id itself would reach that tenant's rows; revoke the grant or add a \
         policy for the platform role by name (platform_access)"
    )]
    PlatformTenantOnly {
        /// The table, as `schema.table`.
        table: String,
        /// The role that holds the privilege: the session's role or a role it can become.
        role: String,
        /// `SELECT`, `INSERT`, `UPDATE`, or `DELETE`.
        command: String,
    },
}

/// [`TENANT_CONDITION`] as Postgres prints a policy expression (`pg_get_expr`). A tenant
/// policy must be exactly this, so that `OR true` or any other widening is refused;
/// `tests/tenant_db.rs` checks that the template's policy prints this way.
pub const TENANT_CONDITION_AS_STORED: &str =
    "(tenant_id = (NULLIF(current_setting('trustiot.tenant_id'::text, true), ''::text))::uuid)";

/// The session check. `$1`: whether the session is the platform role's; `$2`:
/// [`TENANT_CONDITION_AS_STORED`]. Problems come back in order of severity; the check reports
/// the first. Each row is the problem, the relation (or function) it concerns, and, for
/// `platform_tenant_only`, the role and the command.
const SESSION_CHECK: &str = r"
WITH RECURSIVE rels AS (
    SELECT c.oid, c.relkind, c.relowner, c.relrowsecurity, c.reloptions, c.relnamespace,
           format('%I.%I', n.nspname, c.relname) AS name,
           EXISTS (
               SELECT 1 FROM pg_attribute a
               WHERE a.attrelid = c.oid AND a.attname = 'tenant_id'
                 AND a.attnum > 0 AND NOT a.attisdropped
           ) AS has_tenant
    FROM pg_class c
    JOIN pg_namespace n ON n.oid = c.relnamespace
    WHERE c.relkind IN ('r', 'p', 'v', 'm', 'f')
      AND n.nspname NOT IN ('pg_catalog', 'information_schema')
      AND n.nspname NOT LIKE 'pg\_toast%'
      AND n.nspname NOT LIKE 'pg\_temp%'
),
tenant_data(oid) AS (
    SELECT oid FROM rels WHERE has_tenant
    UNION
    SELECT rw.ev_class
    FROM pg_rewrite rw
    JOIN pg_depend d ON d.classid = 'pg_rewrite'::regclass AND d.objid = rw.oid
                    AND d.refclassid = 'pg_class'::regclass
    JOIN tenant_data t ON t.oid = d.refobjid
    WHERE rw.ev_class <> d.refobjid
),
reachable AS (
    SELECT * FROM rels
    WHERE oid IN (SELECT oid FROM tenant_data)
      AND has_schema_privilege(relnamespace, 'USAGE')
      AND (has_table_privilege(oid, 'SELECT, INSERT, UPDATE, DELETE, TRUNCATE')
           OR has_any_column_privilege(oid, 'SELECT, INSERT, UPDATE'))
),
-- Platform sessions only: every role the session can become with SET ROLE, itself included.
-- The rights of roles it only inherits are already the session role's own; a role it can
-- neither inherit nor become gives it nothing.
platform_roles AS (
    SELECT oid, quote_ident(rolname) AS role_name FROM pg_roles
    WHERE $1 AND pg_has_role(current_user, oid, 'SET')
),
-- The tables with row-level security that one of those roles may use at all.
platform_tables AS MATERIALIZED (
    SELECT pr.oid AS role_oid, pr.role_name, r.oid AS rel, r.name AS relation
    FROM platform_roles pr
    CROSS JOIN rels r
    WHERE r.relkind IN ('r', 'p') AND r.relrowsecurity
      AND has_schema_privilege(pr.oid, r.relnamespace, 'USAGE')
      AND (has_table_privilege(pr.oid, r.oid, 'SELECT, INSERT, UPDATE, DELETE')
           OR has_any_column_privilege(pr.oid, r.oid, 'SELECT, INSERT, UPDATE'))
),
-- The permissive policies, each printed once. Restrictive policies only narrow access, so
-- they are not counted.
platform_policies AS MATERIALIZED (
    SELECT polrelid, polcmd::text AS polcmd, polroles,
           pg_get_expr(polqual, polrelid) AS using_expr,
           pg_get_expr(coalesce(polwithcheck, polqual), polrelid) AS check_expr
    FROM pg_policy
    WHERE $1 AND polpermissive
),
-- A command evaluates USING to filter rows (SELECT, UPDATE, DELETE) and WITH CHECK, or USING
-- when a policy has none, to check new rows (INSERT, UPDATE). A policy applies to a role that
-- has the privileges of (inherits) one of the policy's roles, or to every role (PUBLIC). Per
-- role, table, command, and side: how many expressions the permissive policies that apply to
-- the role give, and how many of them open the table beyond the session's tenant. An
-- expression that reads the tenant setting, however it is written, or that is false counts as
-- opening nothing beyond it; any other expression counts as opening the table. Counting an
-- expression as opening nothing can only refuse more sessions, never accept one, so an
-- expression that reads the setting and still opens every row (`<tenant condition> OR true`)
-- is refused too: the check does not try to tell such expressions apart.
platform_sides AS (
    SELECT t.role_oid, t.role_name, t.rel, t.relation, s.command, s.uses_qual,
           count(e.expr) AS expressions,
           count(e.expr) FILTER (WHERE e.expr <> $2 AND e.expr <> 'false'
                                 AND strpos(e.expr, '''trustiot.tenant_id''') = 0) AS wider
    FROM platform_tables t
    CROSS JOIN (VALUES ('SELECT', 'r', true), ('INSERT', 'a', false), ('UPDATE', 'w', true),
                       ('UPDATE', 'w', false), ('DELETE', 'd', true))
               AS s(command, polcmd, uses_qual)
    LEFT JOIN platform_policies p
      ON p.polrelid = t.rel AND p.polcmd IN (s.polcmd, '*')
         AND (0 = ANY (p.polroles)
              OR EXISTS (SELECT 1 FROM unnest(p.polroles) AS role
                         WHERE role <> 0 AND pg_has_role(t.role_oid, role, 'USAGE')))
    CROSS JOIN LATERAL (
        SELECT CASE WHEN s.uses_qual THEN p.using_expr ELSE p.check_expr END AS expr
    ) AS e
    GROUP BY t.role_oid, t.role_name, t.rel, t.relation, s.command, s.uses_qual
),
-- The commands only the tenant condition opens to the role: every side the command evaluates
-- has an expression, and on at least one side none of them opens the table beyond the tenant.
platform_tenant_only AS MATERIALIZED (
    SELECT role_oid, role_name, rel, relation, command FROM platform_sides
    GROUP BY role_oid, role_name, rel, relation, command
    HAVING bool_and(expressions > 0) AND bool_or(wider = 0)
)
SELECT problem, relation, role_name, command FROM (
    SELECT 1 AS rank, 'bypasses_rls' AS problem, NULL::text AS relation,
           NULL::text AS role_name, NULL::text AS command
    WHERE EXISTS (
        SELECT 1 FROM pg_roles r
        WHERE (r.rolsuper OR r.rolbypassrls) AND pg_has_role(current_user, r.oid, 'MEMBER')
    )
    UNION ALL
    SELECT 2, 'tenant_preset', NULL, NULL, NULL
    WHERE NULLIF(current_setting('trustiot.tenant_id', true), '') IS NOT NULL
    UNION ALL
    SELECT 3, 'owns_relation', name, NULL, NULL FROM rels
    WHERE pg_has_role(current_user, relowner, 'MEMBER')
    UNION ALL
    SELECT 4, 'unprotected_table', name, NULL, NULL FROM reachable
    WHERE relkind IN ('r', 'p') AND NOT relrowsecurity
    UNION ALL
    SELECT 4, 'permissive_policy', r.name || ' policy ' || quote_ident(p.polname), NULL, NULL
    FROM reachable r
    JOIN pg_policy p ON p.polrelid = r.oid
    WHERE r.relkind IN ('r', 'p') AND r.relrowsecurity AND p.polpermissive
      AND (0 = ANY (p.polroles)
           OR EXISTS (SELECT 1 FROM unnest(p.polroles) AS role
                      WHERE role <> 0 AND pg_has_role(current_user, role, 'MEMBER')))
      AND NOT ($1 AND NOT 0 = ANY (p.polroles)
               AND (SELECT oid FROM pg_roles WHERE rolname = current_user) = ANY (p.polroles))
      AND NOT ((p.polqual IS NOT NULL OR p.polwithcheck IS NOT NULL)
               AND (p.polqual IS NULL OR pg_get_expr(p.polqual, p.polrelid) = $2)
               AND (p.polwithcheck IS NULL OR pg_get_expr(p.polwithcheck, p.polrelid) = $2))
    UNION ALL
    SELECT 4, 'unprotected_view', name, NULL, NULL FROM reachable
    WHERE relkind = 'v'
      AND NOT EXISTS (
          SELECT 1 FROM unnest(reloptions) AS option
          WHERE lower(option) IN ('security_invoker=true', 'security_invoker=on',
                                  'security_invoker=yes', 'security_invoker=1')
      )
    UNION ALL
    SELECT 4, 'unprotectable', name, NULL, NULL FROM reachable WHERE relkind IN ('m', 'f')
    UNION ALL
    SELECT 4, 'definer_function', p.oid::regprocedure::text, NULL, NULL
    FROM pg_proc p
    JOIN pg_namespace n ON n.oid = p.pronamespace
    WHERE p.prosecdef
      AND n.nspname NOT IN ('pg_catalog', 'information_schema')
      AND NOT pg_has_role(current_user, p.proowner, 'MEMBER')
      AND has_schema_privilege(n.oid, 'USAGE')
      AND has_function_privilege(p.oid, 'EXECUTE')
      AND NOT EXISTS (
          SELECT 1 FROM pg_depend d
          WHERE d.classid = 'pg_proc'::regclass AND d.objid = p.oid AND d.deptype = 'e'
      )
    UNION ALL
    SELECT 4, 'temporary_tables', NULL, NULL, NULL
    WHERE has_database_privilege(current_database(), 'TEMPORARY')
    UNION ALL
    SELECT 5, 'truncate_allowed', name, NULL, NULL FROM reachable
    WHERE relkind IN ('r', 'p') AND has_table_privilege(oid, 'TRUNCATE')
    UNION ALL
    -- The commands only the tenant condition opens to a platform role that the role may run,
    -- through a table or column privilege. DELETE has no column privilege, and NULLIF makes its
    -- column test NULL.
    SELECT 4, 'platform_tenant_only', relation, role_name, command FROM platform_tenant_only
    WHERE has_table_privilege(role_oid, rel, command)
       OR coalesce(has_any_column_privilege(role_oid, rel, NULLIF(command, 'DELETE')), false)
) AS problems
ORDER BY rank, relation, problem, role_name, command
LIMIT 1
";

/// A row of [`SESSION_CHECK`]: the problem, the relation, the role, and the command.
type SessionCheckRow = (String, Option<String>, Option<String>, Option<String>);

/// Checks that a session of `role` is safe for tenant data: see [`UnsafeSession`]. Pools from
/// [`connect_lazy`] and the `tenant` module run it on every new connection.
///
/// # Errors
///
/// The outer error when the check query fails; the inner one names the first problem found.
pub async fn check_session(
    connection: &mut PgConnection,
    role: SessionRole,
) -> Result<Result<(), UnsafeSession>, sqlx::Error> {
    let problem: Option<SessionCheckRow> = sqlx::query_as(SESSION_CHECK)
        .bind(role == SessionRole::Platform)
        .bind(TENANT_CONDITION_AS_STORED)
        .fetch_optional(&mut *connection)
        .await?;
    let Some((problem, relation, role_name, command)) = problem else {
        return Ok(Ok(()));
    };
    let relation = relation.unwrap_or_default();
    Ok(Err(match problem.as_str() {
        "bypasses_rls" => UnsafeSession::BypassesRls,
        "tenant_preset" => UnsafeSession::TenantPreset,
        "owns_relation" => UnsafeSession::OwnsRelation(relation),
        "unprotected_table" => UnsafeSession::UnprotectedTable(relation),
        "permissive_policy" => UnsafeSession::PermissivePolicy(relation),
        "unprotected_view" => UnsafeSession::UnprotectedView(relation),
        "unprotectable" => UnsafeSession::Unprotectable(relation),
        "definer_function" => UnsafeSession::DefinerFunction(relation),
        "temporary_tables" => UnsafeSession::TemporaryTables,
        "truncate_allowed" => UnsafeSession::TruncateAllowed(relation),
        "platform_tenant_only" => UnsafeSession::PlatformTenantOnly {
            table: relation,
            role: role_name.unwrap_or_default(),
            command: command.unwrap_or_default(),
        },
        other => {
            return Err(sqlx::Error::Protocol(format!(
                "unknown session check result `{other}`"
            )));
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> PostgresConfig {
        PostgresConfig {
            host: "postgres.example.internal".to_owned(),
            port: 5432,
            database: "trustiot".to_owned(),
            ssl_mode: SslMode::VerifyFull,
            ca_file: PathBuf::from("/etc/trustiot/postgres-ca/ca.crt"),
            username: "assets_app".to_owned(),
            password: "do-not-print".to_owned(),
        }
    }

    #[test]
    fn only_verify_full_parses() {
        assert_eq!("verify-full".parse(), Ok(SslMode::VerifyFull));
        for weaker in [
            "",
            "disable",
            "allow",
            "prefer",
            "require",
            "verify-ca",
            "verify_full",
            "VERIFY-FULL",
            " verify-full",
        ] {
            assert_eq!(
                weaker.parse::<SslMode>(),
                Err(InsecureSslMode),
                "{weaker:?}"
            );
        }
        assert_eq!(SslMode::VerifyFull.to_string(), "verify-full");
    }

    #[test]
    fn deserializing_a_weaker_mode_fails_without_repeating_it() {
        let parsed: SslMode = serde_json::from_str("\"verify-full\"").expect("verify-full");
        assert_eq!(parsed, SslMode::VerifyFull);
        let error = serde_json::from_str::<SslMode>("\"verify-ca\"")
            .expect_err("verify-ca is refused")
            .to_string();
        assert!(error.contains("only `verify-full` is accepted"), "{error}");
        assert!(!error.contains("verify-ca"), "{error}");
        assert!(serde_json::from_str::<SslMode>("true").is_err());
    }

    #[test]
    fn connect_options_verify_the_server_with_the_ca_file() {
        let options = config().connect_options();
        assert!(matches!(options.get_ssl_mode(), PgSslMode::VerifyFull));
        assert_eq!(options.get_host(), "postgres.example.internal");
        assert_eq!(options.get_port(), 5432);
        assert_eq!(options.get_database(), Some("trustiot"));
        assert_eq!(options.get_username(), "assets_app");
        assert_eq!(require_verify_full(&options), Ok(()));
        // sqlx has no getter for the root certificate; its Debug output shows it.
        let debug = format!("{options:?}");
        assert!(
            debug.contains(r#"ssl_root_cert: Some(File("/etc/trustiot/postgres-ca/ca.crt"))"#),
            "the CA file is the root certificate"
        );
    }

    #[test]
    fn debug_hides_the_password() {
        let text = format!("{:?}", config());
        assert!(!text.contains("do-not-print"), "{text}");
        assert!(text.contains("<redacted>"), "{text}");
    }

    #[test]
    fn weaker_connect_options_are_refused() {
        for mode in [
            PgSslMode::Disable,
            PgSslMode::Allow,
            PgSslMode::Prefer,
            PgSslMode::Require,
            PgSslMode::VerifyCa,
        ] {
            let options = config().connect_options().ssl_mode(mode);
            assert_eq!(
                require_verify_full(&options),
                Err(InsecureConnection::SslMode),
                "{mode:?}"
            );
        }
        let socket = config().connect_options().socket("/run/postgresql");
        assert_eq!(
            require_verify_full(&socket),
            Err(InsecureConnection::UnixSocket)
        );
    }

    #[tokio::test]
    async fn connect_lazy_refuses_options_that_do_not_verify_the_server() {
        let options = config().connect_options().ssl_mode(PgSslMode::Require);
        let error = connect_lazy(options, PgPoolOptions::new()).expect_err("refused");
        assert_eq!(error, InsecureConnection::SslMode);
        // A URL without sslmode means `prefer` to sqlx.
        let from_url: PgConnectOptions = "postgres://app@db.example.internal/trustiot"
            .parse()
            .expect("url");
        assert!(connect_lazy(from_url, PgPoolOptions::new()).is_err());
    }

    #[test]
    fn refusals_keep_the_last_outcome() {
        let refusals = Refusals::default();
        assert_eq!(refusals.last(), None);
        let problem = UnsafeSession::UnprotectedTable("public.devices".to_owned());
        refusals.record(Some(problem.clone()));
        assert_eq!(refusals.clone().last(), Some(problem));
        refusals.record(None);
        assert_eq!(refusals.last(), None, "a session that passes clears it");
    }
}
