//! The Postgres part of a run, on one session that holds the migration lock throughout.
//!
//! This is the one file outside service-kit's tenant module that switches roles in SQL
//! (`set_config('role', ...)`, tools/sql-check's `SESSION_SETTING_ALLOWED`): a service's
//! migrations run as its owner role, so the objects they create belong to it and the migration
//! role keeps no rights on them, and as that role they reach their own schema only (decision 4).
//! The owner role is a guard against mistakes, not against a hostile migration file: the file
//! could `RESET ROLE`. Migration files are reviewed code.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use service_kit::postgres::require_verify_full;
use sqlx::postgres::PgConnectOptions;
use sqlx::{Connection, Executor, PgConnection, Row};

use crate::config::{RunConfig, is_plain_identifier};
use crate::passwords::Passwords;
use crate::registry::{
    DATABASE, Migration, OUTBOX_EVENT_TEMPLATE, Registry, Service, TenantRoleHook,
};
use crate::secret::Password;
use crate::{Error, Report, sha256_hex};

/// The migration lock: a session-level advisory lock with this two-integer key ("trus",
/// "migr"). service-kit's job locks use one-bigint keys, which Postgres keeps apart from
/// two-integer keys, so they never collide.
pub const LOCK_KEY: (i32, i32) = (0x7472_7573, 0x6d69_6772);

/// How often a waiting run retries the database or the lock, and how often it says so.
const RETRY_EVERY: Duration = Duration::from_secs(2);
const REPORT_EVERY: Duration = Duration::from_secs(30);

const BOOTSTRAP: &str = include_str!("../sql/bootstrap.sql");

/// The Job's own numbered files (`sql/`), applied once each. Here rather than in the registry,
/// because the helpers switch roles (see the module documentation).
pub const OWN_MIGRATIONS: &[Migration] = &[Migration {
    name: "0001_helpers.sql",
    sql: include_str!("../sql/0001_helpers.sql"),
}];

const APPLICATION_NAME: &str = "SELECT set_config('application_name', 'trustiot-migrate', false)";
const TRY_LOCK: &str = "SELECT pg_try_advisory_lock($1, $2)";
const UNLOCK: &str = "SELECT pg_advisory_unlock($1, $2)";
const SET_ROLE: &str = "SELECT set_config('role', $1, true)";
const RESET_ROLE: &str = "SELECT set_config('role', 'none', true)";
const SET_SEARCH_PATH: &str = "SELECT set_config('search_path', $1, true)";
const APPLY_TEMPLATE: &str = "SELECT trustiot_migrate.apply_template($1, $2, $3::jsonb)";

/// Also counts the databases the role owns: roles are cluster-wide, so a migration role that owns
/// two databases (two TrustIoT instances on one Postgres cluster) would give both instances the
/// same service roles, passwords and grants.
const MIGRATOR: &str = "
SELECT r.rolname::text, r.rolsuper, r.rolcreaterole, r.rolbypassrls, d.datdba = r.oid,
       (SELECT count(*) FROM pg_database o WHERE o.datdba = r.oid)
FROM pg_roles r
JOIN pg_database d ON d.datname = current_database()
WHERE r.rolname = current_user";

const APPLIED: &str =
    "SELECT sha256 FROM trustiot_migrate.applied WHERE component = $1 AND name = $2";
const RECORD: &str =
    "INSERT INTO trustiot_migrate.applied (component, name, sha256) VALUES ($1, $2, $3)";

/// A role's attributes, what the migration role may do with it, and its memberships.
const ROLE_STATE: &str = "
SELECT r.rolsuper, r.rolcreaterole, r.rolcreatedb, r.rolcanlogin, r.rolreplication,
       r.rolbypassrls, r.rolinherit,
       EXISTS (SELECT 1 FROM pg_auth_members m JOIN pg_roles me ON me.oid = m.member
               WHERE m.roleid = r.oid AND me.rolname = current_user AND m.admin_option),
       pg_has_role(current_user, r.oid, 'SET'),
       coalesce(r.rolconfig, '{}')::text[],
       ARRAY(SELECT DISTINCT g.rolname::text FROM pg_auth_members m
             JOIN pg_roles g ON g.oid = m.roleid WHERE m.member = r.oid ORDER BY 1),
       ARRAY(SELECT DISTINCT u.rolname::text FROM pg_auth_members m
             JOIN pg_roles u ON u.oid = m.member WHERE m.roleid = r.oid ORDER BY 1),
       ARRAY(SELECT unnest(s.setconfig) FROM pg_db_role_setting s
             JOIN pg_database d ON d.oid = s.setdatabase
             WHERE s.setrole = r.oid AND d.datname = current_database()),
       EXISTS (SELECT 1 FROM pg_shdepend s
               WHERE s.refclassid = 'pg_authid'::regclass AND s.refobjid = r.oid AND s.dbid <> 0
                 AND s.dbid <> (SELECT oid FROM pg_database WHERE datname = current_database())),
       coalesce(shobj_description(r.oid, 'pg_authid'), '')
FROM pg_roles r
WHERE r.rolname = $1";

/// The comment the Job puts on every login role it creates: what marks a role as one whose login
/// the Job manages (`retire_login_roles`), never its name. Changing a role's comment needs
/// CREATEROLE and ADMIN OPTION on it, which service roles do not have.
const LOGIN_ROLE_MARKER: &str = "trustiot-migrate:login-role";

/// Login roles the Job marked that no service of the registry has any more (the service was
/// removed from the registry or renamed) and that can still log in. Only roles the migration role
/// administers: another instance's roles, the cluster's superuser and a customer's own accounts
/// are never among them, whatever their comment.
const RETIRED_LOGIN_ROLES: &str = "
SELECT r.rolname::text FROM pg_roles r
WHERE r.rolcanlogin AND NOT r.rolsuper
  AND shobj_description(r.oid, 'pg_authid') = $1
  AND r.rolname <> ALL ($2::text[])
  AND EXISTS (SELECT 1 FROM pg_auth_members m JOIN pg_roles me ON me.oid = m.member
              WHERE m.roleid = r.oid AND me.rolname = current_user AND m.admin_option)
ORDER BY 1";

/// What a login role may do beyond its grants: create in its own schema, or create temporary
/// tables (database.sql revoked TEMPORARY from PUBLIC once; the database owner can grant it back).
const LOGIN_EXTRAS: &str = "
SELECT has_schema_privilege($1, $2, 'CREATE'),
       has_database_privilege($1, current_database(), 'TEMPORARY')";

/// The file must end in the transaction it started in, as the role it started as.
const XACT_AND_ROLE: &str =
    "SELECT pg_current_xact_id()::text, current_user::text, current_setting('role')";

const SCHEMA_OWNER: &str =
    "SELECT pg_get_userbyid(nspowner)::text FROM pg_namespace WHERE nspname = $1";
/// From the catalog: the migration role has no USAGE on a service's schema (the schema belongs
/// to the owner role, which the migration role can only SET to), so to_regclass would fail.
const TABLE_EXISTS: &str = "SELECT EXISTS (SELECT 1 FROM pg_class c
    JOIN pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname = $1 AND c.relname = $2)";
const OUTBOX_READERS: &str = "
SELECT ARRAY(SELECT rolname::text FROM pg_roles WHERE oid = ANY (p.polroles) ORDER BY 1)
FROM pg_policy p
JOIN pg_class c ON c.oid = p.polrelid
JOIN pg_namespace n ON n.oid = c.relnamespace
WHERE n.nspname = $1 AND c.relname = 'outbox_event' AND p.polname = 'consumer_read'";

// Templates for trustiot_migrate.apply_template (sql/0001_helpers.sql): each `example_<name>`
// is replaced by quoted names or a literal, never by free text.

/// An owner role: never logs in. Since PostgreSQL 16, CREATE ROLE by a CREATEROLE role always
/// grants the new role to its creator WITH ADMIN OPTION (and neither INHERIT nor SET), which is
/// what `ensure_role` checks as "administered"; createrole_self_grant = 'set' adds a second grant
/// with SET only, so the migration role can run a service's migrations as the owner role and
/// still holds none of its rights. tests/fresh_database.rs checks both on a fresh database.
const CREATE_OWNER_ROLE: &str = "SET LOCAL createrole_self_grant = 'set';
CREATE ROLE example_role NOLOGIN NOINHERIT;
SET LOCAL createrole_self_grant = ''";
/// A service or platform role: inherits nothing, and logs in only once `ensure_login` has set
/// the password of its Secret. The migration role only administers it (Postgres gives the
/// creating role ADMIN OPTION, without SET or INHERIT). The comment marks it as the Job's.
const CREATE_LOGIN_ROLE: &str = "CREATE ROLE example_role NOLOGIN NOINHERIT;
COMMENT ON ROLE example_role IS example_marker";
const MARK_LOGIN_ROLE: &str = "COMMENT ON ROLE example_role IS example_marker";
/// A login role logs in exactly while a Secret holds its password; NOLOGIN keeps the role, what
/// it owns and its grants, so turning the service on again only needs LOGIN back.
const ALLOW_LOGIN: &str = "ALTER ROLE example_role LOGIN";
const REVOKE_LOGIN: &str = "ALTER ROLE example_role NOLOGIN";
/// A per-tenant role of a hook: created without login; the hook's template grants it what it
/// needs.
const CREATE_TENANT_ROLE: &str = "CREATE ROLE example_role NOLOGIN NOINHERIT";
const CREATE_SCHEMA: &str = "CREATE SCHEMA example_schema AUTHORIZATION example_owner";
const SET_ROLE_SEARCH_PATH: &str = "ALTER ROLE example_role SET search_path = example_schema";
/// The verifier is a SCRAM-SHA-256 hash (crate::secret), so the plaintext never reaches SQL.
const SET_PASSWORD: &str = "ALTER ROLE example_role PASSWORD example_verifier";
/// crates/outbox/README.md, "資料庫權限": the producer's roles.
const PRODUCER_GRANTS: &str = "GRANT USAGE ON SCHEMA example_schema TO example_roles;
GRANT SELECT, INSERT, UPDATE ON example_schema.outbox_aggregate TO example_roles";
/// The consumer's own tables, for its service role.
const CONSUMER_GRANTS: &str = "GRANT USAGE ON SCHEMA example_schema TO example_roles;
GRANT SELECT, INSERT, UPDATE, DELETE ON example_schema.outbox_subscription,
    example_schema.outbox_inbox, example_schema.outbox_dead_letter,
    example_schema.outbox_version TO example_roles";
/// The consumers' platform roles read the producer's events, nothing else of its schema.
const ALTER_READERS: &str =
    "ALTER POLICY consumer_read ON example_schema.outbox_event TO example_roles";
const GRANT_READERS: &str = "GRANT USAGE ON SCHEMA example_schema TO example_roles;
GRANT SELECT ON example_schema.outbox_event TO example_roles";
const REVOKE_READERS: &str = "REVOKE SELECT ON example_schema.outbox_event FROM example_roles;
REVOKE USAGE ON SCHEMA example_schema FROM example_roles";

/// What a role is for, which decides what the run accepts of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RoleKind {
    Owner,
    Login,
    Tenant,
}

struct RoleState {
    superuser: bool,
    createrole: bool,
    createdb: bool,
    login: bool,
    replication: bool,
    bypassrls: bool,
    inherit: bool,
    administered: bool,
    can_set: bool,
    config: Vec<String>,
    member_of: Vec<String>,
    members: Vec<String>,
    /// Settings for this role in this database (ALTER ROLE ... IN DATABASE ... SET).
    database_config: Vec<String>,
    /// Owns objects or holds privileges in another database of the cluster.
    elsewhere: bool,
    /// The role's comment (`LOGIN_ROLE_MARKER` on the Job's login roles), or empty.
    comment: String,
}

/// The run's Postgres session, connected as the migration role.
pub struct Session {
    conn: PgConnection,
    migrator: String,
    locked: bool,
}

/// Checks, before anything runs, that every role in `passwords.postgres` is a login role of the
/// registry: the chart binds a Secret to the role of every service of the deployment that uses
/// Postgres, and a role the Job does not create would get a password nobody logs in with.
///
/// # Errors
///
/// [`Error::Role`] for the first role that is not.
pub fn check_password_bindings(config: &RunConfig, registry: &Registry) -> Result<(), Error> {
    let Some(settings) = &config.passwords else {
        return Ok(());
    };
    let bindings = settings
        .postgres()
        .map_err(|error| Error::Kube(format!("{}: {}", error.key, error.message)))?;
    let login_roles: BTreeSet<String> = registry
        .services
        .iter()
        .flat_map(Service::login_roles)
        .collect();
    for binding in bindings {
        if !login_roles.contains(&binding.owner) {
            return Err(Error::Role {
                role: binding.owner,
                problem: "a service of this deployment logs in with it, but no service in \
                          tools/migrate's registry has it; register the service there"
                    .to_owned(),
            });
        }
    }
    Ok(())
}

/// Checks the registry before anything runs: names, outbox wiring, hooks.
///
/// # Errors
///
/// [`Error::Registry`] with the first problem.
pub fn check_registry(registry: &Registry) -> Result<(), Error> {
    let fail = |problem: String| Err(Error::Registry(problem));
    let mut seen = BTreeSet::new();
    for (index, service) in registry.services.iter().enumerate() {
        let roles = [service.owner_role(), service.app_role()];
        if !is_plain_identifier(&service.schema()) || roles.iter().any(|r| !is_plain_identifier(r))
        {
            return fail(format!(
                "{}: the name does not make plain role names",
                service.name
            ));
        }
        if !seen.insert(service.name) {
            return fail(format!("{} is listed twice", service.name));
        }
        let mut names = BTreeSet::new();
        for migration in service.migrations {
            if !names.insert(migration.name) {
                return fail(format!(
                    "{}: {} is listed twice",
                    service.name, migration.name
                ));
            }
        }
        let outbox = service.outbox;
        if (outbox.producer || !outbox.consumes.is_empty()) && !service.platform_role {
            return fail(format!(
                "{}: an outbox producer or consumer needs a platform role",
                service.name
            ));
        }
        for producer in outbox.consumes {
            let earlier = registry.services[..index]
                .iter()
                .any(|s| s.name == *producer && s.outbox.producer);
            let itself = *producer == service.name && outbox.producer;
            if !(earlier || itself) {
                return fail(format!(
                    "{}: consumes {producer}, which is not an outbox producer listed before it",
                    service.name
                ));
            }
        }
        if outbox.producer && readers(registry, service).is_empty() {
            return fail(format!(
                "{}: an outbox producer needs at least one consumer (the policy names its readers)",
                service.name
            ));
        }
    }
    for hook in registry.tenant_role_hooks {
        if registry.service(hook.service).is_none() {
            return fail(format!(
                "hook {}: unknown service {}",
                hook.name, hook.service
            ));
        }
        if !is_plain_identifier(hook.role_prefix) || hook.role_prefix.len() > 31 {
            return fail(format!(
                "hook {}: the role prefix must be a plain identifier of at most 31 bytes",
                hook.name
            ));
        }
    }
    Ok(())
}

/// The platform roles that read `producer`'s events, sorted.
fn readers(registry: &Registry, producer: &Service) -> Vec<String> {
    let mut roles: Vec<String> = registry
        .services
        .iter()
        .filter(|service| service.outbox.consumes.contains(&producer.name))
        .filter_map(Service::platform_role)
        .collect();
    roles.sort();
    roles
}

fn names(roles: &[String]) -> Value {
    json!(roles)
}

async fn template(
    conn: &mut PgConnection,
    run_as: Option<&str>,
    template: &'static str,
    replacements: Value,
) -> Result<(), sqlx::Error> {
    sqlx::query(APPLY_TEMPLATE)
        .bind(run_as)
        .bind(template)
        .bind(replacements.to_string())
        .execute(conn)
        .await
        .map(|_| ())
}

/// How a tracked file runs.
enum Apply {
    /// Its text, as is (several statements, simple query protocol).
    File,
    /// Through apply_template with these replacements.
    Template(Value),
}

impl Session {
    /// Connects as the migration role, verifying the server (verify-full against the configured
    /// CA only), and retries until `wait.databases`: on a fresh install the Job starts while
    /// CloudNativePG is still creating the database and the role.
    ///
    /// # Errors
    ///
    /// When the connection settings are not verify-full, or no connection succeeded in time.
    pub async fn connect(config: &RunConfig) -> Result<Self, Error> {
        let options = config.postgres().connect_options();
        require_verify_full(&options).map_err(|error| Error::MigrationRole {
            role: config.postgres.username.clone(),
            problem: error.to_string(),
        })?;
        let started = Instant::now();
        let mut reported = None::<Instant>;
        let conn = loop {
            match PgConnection::connect_with(&options).await {
                Ok(conn) => break conn,
                Err(error) => {
                    if started.elapsed() >= config.wait.databases {
                        return Err(Error::PostgresUnavailable(
                            config.wait.databases,
                            error.to_string(),
                        ));
                    }
                    if reported.is_none_or(|at| at.elapsed() >= REPORT_EVERY) {
                        tracing::warn!(error = %error, "Postgres does not accept the migration role yet; retrying");
                        reported = Some(Instant::now());
                    }
                    tokio::time::sleep(RETRY_EVERY).await;
                }
            }
        };
        let mut session = Self {
            conn,
            migrator: config.postgres.username.clone(),
            locked: false,
        };
        sqlx::query(APPLICATION_NAME)
            .execute(&mut session.conn)
            .await
            .map_err(Error::postgres("set the application name"))?;
        Ok(session)
    }

    /// Takes the migration lock, waiting up to `wait` while another run holds it.
    ///
    /// # Errors
    ///
    /// [`Error::LockTimeout`] when the other run did not finish in time.
    pub async fn lock(&mut self, wait: Duration) -> Result<(), Error> {
        let started = Instant::now();
        let mut reported = None::<Instant>;
        loop {
            let locked: bool = sqlx::query_scalar(TRY_LOCK)
                .bind(LOCK_KEY.0)
                .bind(LOCK_KEY.1)
                .fetch_one(&mut self.conn)
                .await
                .map_err(Error::postgres("take the migration lock"))?;
            if locked {
                self.locked = true;
                tracing::info!(
                    waited_ms = started.elapsed().as_millis(),
                    "took the migration lock"
                );
                return Ok(());
            }
            if started.elapsed() >= wait {
                return Err(Error::LockTimeout(wait));
            }
            if reported.is_none_or(|at| at.elapsed() >= REPORT_EVERY) {
                tracing::info!("another migration run holds the lock; waiting for it to finish");
                reported = Some(Instant::now());
            }
            tokio::time::sleep(RETRY_EVERY).await;
        }
    }

    /// Releases the lock and ends the session. Fails when the session that held the lock ended
    /// during the run.
    ///
    /// # Errors
    ///
    /// [`Error::LockLost`].
    pub async fn unlock(mut self) -> Result<(), Error> {
        let released: Result<bool, sqlx::Error> = sqlx::query_scalar(UNLOCK)
            .bind(LOCK_KEY.0)
            .bind(LOCK_KEY.1)
            .fetch_one(&mut self.conn)
            .await;
        if !matches!(released, Ok(true)) || !self.locked {
            return Err(Error::LockLost);
        }
        // The session ending would release the lock anyway; a failed close changes nothing.
        let _ = self.conn.close().await;
        Ok(())
    }

    /// The whole Postgres part, after [`Self::check_migrator`].
    ///
    /// # Errors
    ///
    /// The first step that fails.
    pub async fn migrate(
        &mut self,
        config: &RunConfig,
        registry: &Registry,
        passwords: &Passwords,
        report: &mut Report,
    ) -> Result<(), Error> {
        sqlx::raw_sql(BOOTSTRAP)
            .execute(&mut self.conn)
            .await
            .map_err(Error::postgres(
                "create the bookkeeping schema trustiot_migrate",
            ))?;
        for migration in OWN_MIGRATIONS {
            self.apply_once("migrate", migration, None, None, Apply::File, report)
                .await?;
        }
        self.apply_once("database", &DATABASE, None, None, Apply::File, report)
            .await?;
        for service in registry.services {
            self.ensure_service(config, passwords, service, report)
                .await?;
        }
        self.retire_login_roles(registry, report).await?;
        for service in registry.services {
            self.ensure_outbox(registry, service, report).await?;
        }
        for service in registry.services {
            for migration in service.migrations {
                let (owner, schema) = (service.owner_role(), service.schema());
                self.apply_once(
                    service.name,
                    migration,
                    Some(&owner),
                    Some(&schema),
                    Apply::File,
                    report,
                )
                .await?;
            }
        }
        for hook in registry.tenant_role_hooks {
            self.run_tenant_hook(registry, hook, report).await?;
        }
        Ok(())
    }

    /// The migration role must own the database (for database.sql and to create schemas) and
    /// create roles; it must not be a superuser or bypass row-level security, so nothing it
    /// creates can be more than it is.
    ///
    /// # Errors
    ///
    /// [`Error::MigrationRole`] when it does not qualify.
    pub async fn check_migrator(&mut self) -> Result<(), Error> {
        let (name, superuser, createrole, bypassrls, owns_database, owned): (
            String,
            bool,
            bool,
            bool,
            bool,
            i64,
        ) = sqlx::query_as(MIGRATOR)
            .fetch_one(&mut self.conn)
            .await
            .map_err(Error::postgres("read the migration role"))?;
        let problem = if superuser {
            Some("it is a superuser; use the database owner with CREATEROLE, never a superuser")
        } else if bypassrls {
            Some("it has BYPASSRLS, which it does not need")
        } else if !createrole {
            Some(
                "it lacks CREATEROLE, which creating the service roles needs \
                 (docs/runbooks/migrations.md)",
            )
        } else if !owns_database {
            Some("it does not own the database")
        } else if owned != 1 {
            Some(
                "it owns more than one database; roles are cluster-wide and the Job's role \
                 names carry no instance name, so one Postgres cluster holds one TrustIoT \
                 database (docs/runbooks/migrations.md)",
            )
        } else {
            None
        };
        match problem {
            Some(problem) => Err(Error::MigrationRole {
                role: name,
                problem: problem.to_owned(),
            }),
            None => Ok(()),
        }
    }

    /// Applies `migration` once, recorded under `component`, in one transaction.
    async fn apply_once(
        &mut self,
        component: &str,
        migration: &Migration,
        run_as: Option<&str>,
        search_path: Option<&str>,
        apply: Apply,
        report: &mut Report,
    ) -> Result<(), Error> {
        let label = format!("{component}/{}", migration.name);
        let sha256 = sha256_hex(migration.sql);
        let recorded: Option<String> = sqlx::query_scalar(APPLIED)
            .bind(component)
            .bind(migration.name)
            .fetch_optional(&mut self.conn)
            .await
            .map_err(Error::postgres(format!("read whether {label} was applied")))?;
        match recorded {
            Some(recorded) if recorded == sha256 => {
                tracing::debug!(migration = %label, "already applied");
                return Ok(());
            }
            Some(_) => {
                return Err(Error::Changed {
                    component: component.to_owned(),
                    name: migration.name.to_owned(),
                });
            }
            None => {}
        }
        let context = format!("apply {label}");
        let mut tx = self
            .conn
            .begin()
            .await
            .map_err(Error::postgres(context.clone()))?;
        let (xact, _, _): (String, String, String) = sqlx::query_as(XACT_AND_ROLE)
            .fetch_one(&mut *tx)
            .await
            .map_err(Error::postgres(context.clone()))?;
        if let Some(path) = search_path {
            sqlx::query(SET_SEARCH_PATH)
                .bind(path)
                .execute(&mut *tx)
                .await
                .map_err(Error::postgres(context.clone()))?;
        }
        match apply {
            Apply::File => {
                if let Some(role) = run_as {
                    sqlx::query(SET_ROLE)
                        .bind(role)
                        .execute(&mut *tx)
                        .await
                        .map_err(Error::postgres(context.clone()))?;
                }
                // A `&str` without arguments goes over the simple query protocol (sqlx prepares
                // only queries with arguments), which runs a file of several commands as is:
                // database.sql and every service file have several, and tests/fresh_database.rs
                // applies them. The text is the registry's compiled-in file, never built here.
                (&mut *tx)
                    .execute(migration.sql)
                    .await
                    .map_err(Error::postgres(context.clone()))?;
                // The file must end where it started: in this transaction, as the role it runs
                // as. One that ended the transaction (END, COMMIT AND CHAIN, ...) or switched
                // roles (SET LOCAL ROLE, SET SESSION ROLE) stops the run, and the file is not
                // recorded. A role switch inside the transaction rolls back with it. After an
                // END, though, what the file ran up to the END, and what it ran afterwards
                // outside any transaction, is already committed when this check runs; since the
                // file is not recorded, the next run applies it again and fails on what is
                // already there, so it fails closed until a fixed file replaces it. This does
                // not isolate one service's files from another's: a file can switch to another
                // owner role and back (tools/migrate/README.md, "migration 檔案的規則").
                let after: (String, String, String) = sqlx::query_as(XACT_AND_ROLE)
                    .fetch_one(&mut *tx)
                    .await
                    .map_err(Error::postgres(context.clone()))?;
                let (user, role) = (run_as.unwrap_or(&self.migrator), run_as.unwrap_or("none"));
                if after.0 != xact || after.1 != user || after.2 != role {
                    return Err(Error::LeftItsContext {
                        file: label,
                        now: format!(
                            "transaction {} instead of {xact}, current_user {}, role setting {}",
                            after.0, after.1, after.2
                        ),
                    });
                }
                sqlx::query(RESET_ROLE)
                    .execute(&mut *tx)
                    .await
                    .map_err(Error::postgres(context.clone()))?;
            }
            Apply::Template(replacements) => {
                template(&mut tx, run_as, migration.sql, replacements)
                    .await
                    .map_err(Error::postgres(context.clone()))?;
            }
        }
        sqlx::query(RECORD)
            .bind(component)
            .bind(migration.name)
            .bind(&sha256)
            .execute(&mut *tx)
            .await
            .map_err(Error::postgres(context.clone()))?;
        tx.commit().await.map_err(Error::postgres(context))?;
        report.change(format!("applied {label}"));
        Ok(())
    }

    async fn role_state(&mut self, role: &str) -> Result<Option<RoleState>, Error> {
        type Row = (
            bool,
            bool,
            bool,
            bool,
            bool,
            bool,
            bool,
            bool,
            bool,
            Vec<String>,
            Vec<String>,
            Vec<String>,
            Vec<String>,
            bool,
            String,
        );
        let row: Option<Row> = sqlx::query_as(ROLE_STATE)
            .bind(role)
            .fetch_optional(&mut self.conn)
            .await
            .map_err(Error::postgres(format!("read role {role}")))?;
        Ok(row.map(
            |(
                superuser,
                createrole,
                createdb,
                login,
                replication,
                bypassrls,
                inherit,
                administered,
                can_set,
                config,
                member_of,
                members,
                database_config,
                elsewhere,
                comment,
            )| RoleState {
                superuser,
                createrole,
                createdb,
                login,
                replication,
                bypassrls,
                inherit,
                administered,
                can_set,
                config,
                member_of,
                members,
                database_config,
                elsewhere,
                comment,
            },
        ))
    }

    /// Creates `role` when it is missing, then checks it is what `kind` allows.
    async fn ensure_role(
        &mut self,
        role: &str,
        kind: RoleKind,
        report: &mut Report,
    ) -> Result<RoleState, Error> {
        let state = match self.role_state(role).await? {
            Some(state) => state,
            None => {
                let create = match kind {
                    RoleKind::Owner => CREATE_OWNER_ROLE,
                    RoleKind::Login => CREATE_LOGIN_ROLE,
                    RoleKind::Tenant => CREATE_TENANT_ROLE,
                };
                let replacements = if kind == RoleKind::Login {
                    json!({ "example_role": [role], "example_marker": LOGIN_ROLE_MARKER })
                } else {
                    json!({ "example_role": [role] })
                };
                template(&mut self.conn, None, create, replacements)
                    .await
                    .map_err(Error::postgres(format!("create role {role}")))?;
                report.change(format!("created role {role}"));
                self.role_state(role).await?.ok_or_else(|| Error::Role {
                    role: role.to_owned(),
                    problem: "missing right after it was created".to_owned(),
                })?
            }
        };
        let problem = |text: &str| {
            Err(Error::Role {
                role: role.to_owned(),
                problem: text.to_owned(),
            })
        };
        if state.superuser || state.createrole || state.createdb || state.replication {
            return problem(
                "has SUPERUSER, CREATEROLE, CREATEDB or REPLICATION; the migration role cannot \
                 remove them, so a database administrator must",
            );
        }
        if state.bypassrls {
            return problem(
                "has BYPASSRLS, which defeats row-level security (decision 29); a database \
                 administrator must remove it",
            );
        }
        // Roles are cluster-wide and their names carry no instance name, so one Postgres cluster
        // holds one TrustIoT database. A role that owns objects or holds privileges in another
        // database belongs to another instance (or was given to this one by hand): sharing it
        // would let one instance's credentials reach the other's rows.
        if state.elsewhere {
            return problem(
                "owns objects or holds privileges in another database of this Postgres cluster; \
                 roles are cluster-wide, so one Postgres cluster holds one TrustIoT database \
                 (docs/runbooks/migrations.md)",
            );
        }
        if !state.database_config.is_empty() {
            return problem(
                "has settings for this database (ALTER ROLE ... IN DATABASE ... SET), which \
                 override the Job's; a database administrator must reset them",
            );
        }
        if !state.administered {
            return problem(
                "exists but the migration role did not create it: another TrustIoT instance on \
                 this Postgres cluster, or someone else, uses this name. One Postgres cluster \
                 holds one TrustIoT database; never share the roles by granting the migration \
                 role ADMIN OPTION on them (docs/runbooks/migrations.md)",
            );
        }
        match kind {
            RoleKind::Owner => {
                if state.login {
                    return problem("an owner role must not log in");
                }
                if !state.can_set {
                    return problem(
                        "the migration role cannot SET ROLE to it, so it cannot run the service's \
                         migrations as it",
                    );
                }
                if !state.member_of.is_empty() {
                    return problem("an owner role must not be a member of other roles");
                }
                if state.members.iter().any(|member| *member != self.migrator) {
                    return problem("only the migration role may be a member of an owner role");
                }
            }
            RoleKind::Login => {
                if state
                    .config
                    .iter()
                    .any(|setting| !setting.starts_with("search_path="))
                {
                    return problem(
                        "has role settings other than its search_path (for example a preset \
                         trustiot.tenant_id); a database administrator must reset them",
                    );
                }
                if !state.member_of.is_empty() {
                    return problem(
                        "a service or platform role must not be a member of any role (decision 29)",
                    );
                }
                if state.members.iter().any(|member| *member != self.migrator) {
                    return problem(
                        "another role is a member of this service or platform role and could use \
                         its rights; a database administrator must revoke it",
                    );
                }
                if state.comment != LOGIN_ROLE_MARKER {
                    // A role created before the Job marked its login roles.
                    template(
                        &mut self.conn,
                        None,
                        MARK_LOGIN_ROLE,
                        json!({ "example_role": [role], "example_marker": LOGIN_ROLE_MARKER }),
                    )
                    .await
                    .map_err(Error::postgres(format!("mark role {role}")))?;
                    report.change(format!("marked role {role} as a login role of the Job"));
                }
            }
            RoleKind::Tenant => {
                if state.login || state.inherit {
                    return problem(
                        "a per-tenant role must be NOLOGIN and NOINHERIT; the hook creates it so",
                    );
                }
                if !state.member_of.is_empty() {
                    return problem(
                        "a per-tenant role must not be a member of other roles, which would give \
                         whoever uses it their rights",
                    );
                }
                // A hook that lets a service role use its tenant roles (Laysi/trustiot-v2#176)
                // widens this to that service's own roles.
                if state.members.iter().any(|member| *member != self.migrator) {
                    return problem(
                        "another role is a member of this per-tenant role and could use its rights",
                    );
                }
            }
        }
        Ok(state)
    }

    async fn ensure_service(
        &mut self,
        config: &RunConfig,
        passwords: &Passwords,
        service: &Service,
        report: &mut Report,
    ) -> Result<(), Error> {
        let schema = service.schema();
        let owner = service.owner_role();
        self.ensure_role(&owner, RoleKind::Owner, report).await?;
        let mut login_states = Vec::new();
        for role in service.login_roles() {
            let state = self.ensure_role(&role, RoleKind::Login, report).await?;
            login_states.push((role, state));
        }
        let schema_owner: Option<String> = sqlx::query_scalar(SCHEMA_OWNER)
            .bind(&schema)
            .fetch_optional(&mut self.conn)
            .await
            .map_err(Error::postgres(format!("read schema {schema}")))?;
        match schema_owner {
            None => {
                template(
                    &mut self.conn,
                    None,
                    CREATE_SCHEMA,
                    json!({ "example_schema": [schema], "example_owner": [owner] }),
                )
                .await
                .map_err(Error::postgres(format!("create schema {schema}")))?;
                report.change(format!("created schema {schema} owned by {owner}"));
            }
            Some(found) if found == owner => {}
            Some(found) => {
                return Err(Error::Schema {
                    schema,
                    problem: format!("is owned by {found}, not {owner}"),
                });
            }
        }
        let wanted = format!("search_path={schema}");
        for (role, _) in &login_states {
            let (create, temporary): (bool, bool) = sqlx::query_as(LOGIN_EXTRAS)
                .bind(role)
                .bind(&schema)
                .fetch_one(&mut self.conn)
                .await
                .map_err(Error::postgres(format!("read the rights of {role}")))?;
            if create || temporary {
                return Err(Error::Role {
                    role: role.clone(),
                    problem: "may create objects in its schema or temporary tables; a service \
                              role only uses what the migrations grant it (decision 29)"
                        .to_owned(),
                });
            }
        }
        for (role, state) in login_states {
            if !state.config.contains(&wanted) {
                template(
                    &mut self.conn,
                    None,
                    SET_ROLE_SEARCH_PATH,
                    json!({ "example_role": [role], "example_schema": [schema] }),
                )
                .await
                .map_err(Error::postgres(format!("set the search_path of {role}")))?;
                report.change(format!("set the search_path of {role} to {schema}"));
            }
            self.ensure_login(
                config,
                passwords.postgres(&role),
                &role,
                state.login,
                report,
            )
            .await?;
        }
        Ok(())
    }

    /// A login role logs in exactly while a Secret is bound to it (the chart binds the roles of
    /// the services this deployment runs). Bound, it gets the Secret's password and LOGIN, the
    /// password first, so it never logs in with an older one; unbound (the service was turned
    /// off), it loses LOGIN and keeps everything else.
    async fn ensure_login(
        &mut self,
        config: &RunConfig,
        password: Option<&Password>,
        role: &str,
        can_log_in: bool,
        report: &mut Report,
    ) -> Result<(), Error> {
        match (password, can_log_in) {
            (Some(password), true) => self.ensure_password(config, password, role, report).await,
            (Some(password), false) => {
                self.set_password(password, role, report).await?;
                self.set_login(role, ALLOW_LOGIN).await?;
                report.change(format!("let role {role} log in"));
                Ok(())
            }
            (None, can_log_in) => {
                tracing::warn!(
                    role,
                    "no Secret is bound to this role, so it does not log in"
                );
                report.roles_without_password.push(role.to_owned());
                if can_log_in {
                    self.set_login(role, REVOKE_LOGIN).await?;
                    report.change(format!(
                        "revoked the login of role {role}: no Secret is bound to it"
                    ));
                }
                Ok(())
            }
        }
    }

    async fn set_login(&mut self, role: &str, statement: &'static str) -> Result<(), Error> {
        template(
            &mut self.conn,
            None,
            statement,
            json!({ "example_role": [role] }),
        )
        .await
        .map_err(Error::postgres(format!("change the login of {role}")))
    }

    /// Revokes the login of every role the Job marked that no service of `registry` has any
    /// more. Never drops it: it may own objects, and NOLOGIN is undone by the next run whose
    /// registry has it again.
    async fn retire_login_roles(
        &mut self,
        registry: &Registry,
        report: &mut Report,
    ) -> Result<(), Error> {
        let current: Vec<String> = registry
            .services
            .iter()
            .flat_map(Service::login_roles)
            .collect();
        let retired: Vec<String> = sqlx::query_scalar(RETIRED_LOGIN_ROLES)
            .bind(LOGIN_ROLE_MARKER)
            .bind(&current)
            .fetch_all(&mut self.conn)
            .await
            .map_err(Error::postgres(
                "list the login roles no service has any more",
            ))?;
        for role in retired {
            self.set_login(&role, REVOKE_LOGIN).await?;
            report.change(format!(
                "revoked the login of role {role}: no service in tools/migrate's registry has it"
            ));
        }
        Ok(())
    }

    /// Sets `role`'s password to the one in its Secret when the role cannot log in with it yet.
    /// The check is a real login, since only a superuser can read the stored verifier; so a run
    /// with the right passwords in place changes nothing.
    async fn ensure_password(
        &mut self,
        config: &RunConfig,
        password: &Password,
        role: &str,
        report: &mut Report,
    ) -> Result<(), Error> {
        let options: PgConnectOptions = config
            .postgres()
            .connect_options()
            .username(role)
            .password(password.expose());
        match PgConnection::connect_with(&options).await {
            Ok(conn) => {
                let _ = conn.close().await;
                return Ok(());
            }
            Err(sqlx::Error::Database(error)) if error.code().as_deref() == Some("28P01") => {}
            Err(error) => {
                return Err(Error::Postgres {
                    context: format!("log in as {role} to check its password"),
                    source: error,
                });
            }
        }
        self.set_password(password, role, report).await
    }

    /// Sets `role`'s password to `password`, as a SCRAM-SHA-256 verifier.
    async fn set_password(
        &mut self,
        password: &Password,
        role: &str,
        report: &mut Report,
    ) -> Result<(), Error> {
        let verifier = password
            .scram_verifier()
            .map_err(|error| Error::Random(error.to_string()))?;
        template(
            &mut self.conn,
            None,
            SET_PASSWORD,
            json!({ "example_role": [role], "example_verifier": verifier }),
        )
        .await
        .map_err(Error::postgres(format!("set the password of {role}")))?;
        report.change(format!("set the password of {role}"));
        Ok(())
    }

    async fn table_exists(&mut self, schema: &str, table: &str) -> Result<bool, Error> {
        sqlx::query_scalar(TABLE_EXISTS)
            .bind(schema)
            .bind(table)
            .fetch_one(&mut self.conn)
            .await
            .map_err(Error::postgres(format!("look for {schema}.{table}")))
    }

    /// Runs idempotent DDL (the outbox crate's) as `owner` in `schema`.
    async fn run_ddl(&mut self, owner: &str, schema: &str, ddl: &'static str) -> Result<(), Error> {
        let context = format!("create the outbox tables in {schema}");
        let mut tx = self
            .conn
            .begin()
            .await
            .map_err(Error::postgres(context.clone()))?;
        sqlx::query(SET_SEARCH_PATH)
            .bind(schema)
            .execute(&mut *tx)
            .await
            .map_err(Error::postgres(context.clone()))?;
        sqlx::query(SET_ROLE)
            .bind(owner)
            .execute(&mut *tx)
            .await
            .map_err(Error::postgres(context.clone()))?;
        // Several commands over the simple query protocol, as in `apply_once`;
        // tests/outbox_and_tenant_roles.rs creates every outbox table this way.
        (&mut *tx)
            .execute(ddl)
            .await
            .map_err(Error::postgres(context.clone()))?;
        sqlx::query(RESET_ROLE)
            .execute(&mut *tx)
            .await
            .map_err(Error::postgres(context.clone()))?;
        tx.commit().await.map_err(Error::postgres(context))
    }

    /// The outbox tables, the event policies and the grants of crates/outbox/README.md.
    async fn ensure_outbox(
        &mut self,
        registry: &Registry,
        service: &Service,
        report: &mut Report,
    ) -> Result<(), Error> {
        let (schema, owner) = (service.schema(), service.owner_role());
        let roles = service.login_roles();
        if service.outbox.producer {
            if !self.table_exists(&schema, "outbox_event").await? {
                report.change(format!("created the outbox producer tables in {schema}"));
            }
            self.run_ddl(&owner, &schema, outbox::schema::PRODUCER_DDL)
                .await?;
            let readers = readers(registry, service);
            let platform = service.platform_role().unwrap_or_default();
            let replacements = json!({
                "example_app": [service.app_role()],
                "example_platform": [platform],
                "example_reader": readers,
            });
            let component = format!("{}.outbox", service.name);
            self.apply_once(
                &component,
                &OUTBOX_EVENT_TEMPLATE,
                Some(&owner),
                Some(&schema),
                Apply::Template(replacements),
                report,
            )
            .await?;
            template(
                &mut self.conn,
                Some(&owner),
                PRODUCER_GRANTS,
                json!({ "example_schema": [schema], "example_roles": names(&roles) }),
            )
            .await
            .map_err(Error::postgres(format!(
                "grant the outbox tables of {schema}"
            )))?;
            self.sync_readers(&schema, &owner, &readers, report).await?;
        }
        if !service.outbox.consumes.is_empty() {
            if !self.table_exists(&schema, "outbox_subscription").await? {
                report.change(format!("created the outbox consumer tables in {schema}"));
            }
            self.run_ddl(&owner, &schema, outbox::schema::CONSUMER_DDL)
                .await?;
            template(
                &mut self.conn,
                Some(&owner),
                CONSUMER_GRANTS,
                json!({ "example_schema": [schema], "example_roles": [service.app_role()] }),
            )
            .await
            .map_err(Error::postgres(format!(
                "grant the outbox consumer tables of {schema}"
            )))?;
        }
        Ok(())
    }

    /// Makes the producer's `consumer_read` policy name exactly the consumers' platform roles.
    async fn sync_readers(
        &mut self,
        schema: &str,
        owner: &str,
        readers: &[String],
        report: &mut Report,
    ) -> Result<(), Error> {
        let current: Option<Vec<String>> = sqlx::query_scalar(OUTBOX_READERS)
            .bind(schema)
            .fetch_optional(&mut self.conn)
            .await
            .map_err(Error::postgres(format!(
                "read the outbox readers of {schema}"
            )))?;
        let Some(current) = current else {
            return Err(Error::Schema {
                schema: schema.to_owned(),
                problem: "outbox_event has no consumer_read policy".to_owned(),
            });
        };
        if current != readers {
            template(
                &mut self.conn,
                Some(owner),
                ALTER_READERS,
                json!({ "example_schema": [schema], "example_roles": readers }),
            )
            .await
            .map_err(Error::postgres(format!(
                "change the outbox readers of {schema}"
            )))?;
            let removed: Vec<String> = current
                .iter()
                .filter(|role| !readers.contains(role))
                .cloned()
                .collect();
            if !removed.is_empty() {
                template(
                    &mut self.conn,
                    Some(owner),
                    REVOKE_READERS,
                    json!({ "example_schema": [schema], "example_roles": removed }),
                )
                .await
                .map_err(Error::postgres(format!(
                    "revoke former outbox readers of {schema}"
                )))?;
            }
            report.change(format!(
                "the outbox readers of {schema} are now {}",
                readers.join(", ")
            ));
        }
        template(
            &mut self.conn,
            Some(owner),
            GRANT_READERS,
            json!({ "example_schema": [schema], "example_roles": readers }),
        )
        .await
        .map_err(Error::postgres(format!(
            "grant the outbox readers of {schema}"
        )))
    }

    async fn run_tenant_hook(
        &mut self,
        registry: &Registry,
        hook: &TenantRoleHook,
        report: &mut Report,
    ) -> Result<(), Error> {
        let hook_error = |problem: String| Error::TenantHook {
            hook: hook.name.to_owned(),
            problem,
        };
        let owner = registry
            .service(hook.service)
            .map(Service::owner_role)
            .ok_or_else(|| hook_error(format!("unknown service {}", hook.service)))?;
        let context = format!("read the tenants of hook {}", hook.name);
        let mut tx = self
            .conn
            .begin()
            .await
            .map_err(Error::postgres(context.clone()))?;
        sqlx::query(SET_ROLE)
            .bind(&owner)
            .execute(&mut *tx)
            .await
            .map_err(Error::postgres(context.clone()))?;
        let rows = (&mut *tx)
            .fetch_all(hook.tenants)
            .await
            .map_err(Error::postgres(context.clone()))?;
        tx.commit().await.map_err(Error::postgres(context))?;
        let mut tenants = Vec::with_capacity(rows.len());
        for row in rows {
            let text: String = row
                .try_get(0)
                .map_err(|error| hook_error(format!("the tenant column must be text: {error}")))?;
            let tenant = uuid::Uuid::parse_str(&text)
                .map_err(|_| hook_error("the tenant column must hold UUIDs".to_owned()))?;
            tenants.push(tenant);
        }
        tracing::info!(
            hook = hook.name,
            tenants = tenants.len(),
            "rebuilding per-tenant roles"
        );
        for tenant in tenants {
            let role = format!("{}{}", hook.role_prefix, tenant.simple());
            self.ensure_role(&role, RoleKind::Tenant, report).await?;
            template(
                &mut self.conn,
                Some(&owner),
                hook.template,
                json!({ "example_tenant_role": [role], "example_tenant_id": tenant.to_string() }),
            )
            .await
            .map_err(Error::postgres(format!(
                "run hook {} for tenant {tenant}",
                hook.name
            )))?;
        }
        Ok(())
    }
}
