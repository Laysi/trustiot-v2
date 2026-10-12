//! Helpers of the migration Job's integration tests: a throwaway Postgres with TLS (shared with
//! crates/service-kit's tests), a throwaway ClickHouse, run settings pointing at them, and a
//! snapshot of what a run can change, to prove that a second run changes nothing.
//!
//! Roles are cluster-wide, so each test binary that runs the product registry has one test and
//! its own Postgres container (the shared container lives as long as one binary).
#![allow(
    dead_code,
    reason = "each test binary includes this module and uses a different subset of it"
)]

#[path = "../../../../crates/service-kit/tests/support/postgres.rs"]
pub mod postgres;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use migrate::config::{ClickHouseSettings, Platform, PostgresSettings, RunConfig, WaitSettings};
use service_kit::postgres::SslMode;
use sqlx::{Connection, PgConnection};

use postgres::{SERVER_HOST, TEST_PASSWORD, TlsPostgres};

/// The migration role the tests give the Job: a database owner with CREATEROLE, as the chart's
/// CloudNativePG bootstrap makes `trustiot_owner`; and a role without CREATEROLE, which the Job
/// refuses. Static SQL for the shared container's init (the shared test password).
pub const MIGRATOR_INIT: &str = "
CREATE ROLE trustiot_owner LOGIN CREATEROLE PASSWORD 'service-kit-test';
CREATE ROLE no_createrole LOGIN PASSWORD 'service-kit-test';
";

pub const MIGRATOR: &str = "trustiot_owner";

/// The ClickHouse image of services/ingest/tests (the chart and the dev environment pin the same
/// digest; `bun test deploy/chart` checks all of them agree).
pub const CLICKHOUSE_IMAGE: &str = "docker.io/clickhouse/clickhouse-server@sha256:110d3d1f8e1ed7e451a6b5d85a16761e948ad43bc2d9ddf10b155964a8b2b40c";
pub const CLICKHOUSE_ADMIN: &str = "trustiot_admin";

/// Generous bounds for a loaded machine; each names what it waits for when it fails.
pub const CLICKHOUSE_START: Duration = Duration::from_secs(180);

/// Fails the test in CI, where every prerequisite must be present; skips it elsewhere.
#[allow(
    clippy::print_stderr,
    reason = "tells the developer why a test did not run"
)]
pub fn unavailable(what: &str) {
    assert!(
        std::env::var_os("CI").is_none(),
        "{what} is required when CI is set"
    );
    eprintln!("skipped: {what} is not available (set CI to make this a failure)");
}

/// `podman` or `docker`, whichever works.
pub fn container_runtime() -> Option<&'static str> {
    ["podman", "docker"].into_iter().find(|runtime| {
        Command::new(runtime)
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    })
}

/// Runs a container runtime command and returns its trimmed stdout.
pub fn runtime_output(runtime: &str, args: &[&str]) -> String {
    let output = Command::new(runtime)
        .args(args)
        .output()
        .unwrap_or_else(|error| {
            panic!(
                "{runtime} {}: {error}",
                args.first().copied().unwrap_or_default()
            )
        });
    assert!(
        output.status.success(),
        "{runtime} {} failed: {}",
        args.first().copied().unwrap_or_default(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn next_id() -> u32 {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// A unique name for a container or database created by this test process.
pub fn unique_name(kind: &str) -> String {
    format!("migrate-test-{kind}-{}-{}", std::process::id(), next_id())
}

/// A container that is removed when dropped.
pub struct Container {
    pub runtime: &'static str,
    pub name: String,
}

impl Container {
    /// The host address mapped to `port` inside the container.
    pub fn host_address(&self, port: &str) -> SocketAddr {
        let mapping = runtime_output(self.runtime, &["port", &self.name, port]);
        mapping
            .lines()
            .find_map(|line| line.trim().parse().ok())
            .unwrap_or_else(|| panic!("no host address for {port}: {mapping}"))
    }
}

impl Drop for Container {
    fn drop(&mut self) {
        let _ = Command::new(self.runtime)
            .args(["rm", "--force", &self.name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// A random value for a test password, made at run time (64 hex digits).
pub fn random_hex() -> String {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes).expect("operating system randomness");
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// A scratch directory for this test process.
pub fn scratch_dir(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(unique_name(name));
    std::fs::create_dir_all(&dir).expect("create the scratch directory");
    dir
}

/// A fresh database owned by the migration role, with the server's CA in a file.
pub struct Database {
    pub postgres: std::sync::Arc<TlsPostgres>,
    pub name: String,
    pub ca_file: PathBuf,
    /// Where the role password files go (`postgres.role_passwords_dir`).
    pub passwords: PathBuf,
}

impl Database {
    /// Creates database `name` with `create` (a static `CREATE DATABASE <name> OWNER
    /// trustiot_owner`); each test binary has its own Postgres, so fixed names do not collide.
    pub async fn create(
        postgres: std::sync::Arc<TlsPostgres>,
        create: &'static str,
        name: &str,
    ) -> Self {
        let mut admin = superuser(&postgres).await;
        sqlx::raw_sql(create)
            .execute(&mut admin)
            .await
            .expect("create the database");
        let name = name.to_owned();
        let dir = scratch_dir(&name);
        let ca_file = dir.join("ca.crt");
        std::fs::write(&ca_file, &postgres.ca_pem).expect("write the CA");
        let passwords = dir.join("role-passwords");
        std::fs::create_dir_all(&passwords).expect("create the password directory");
        Self {
            postgres,
            name,
            ca_file,
            passwords,
        }
    }

    pub async fn connect(&self, role: &str) -> PgConnection {
        PgConnection::connect_with(&self.postgres.options(role, &self.name))
            .await
            .unwrap_or_else(|error| panic!("connect as {role}: {error}"))
    }

    pub async fn connect_with_password(
        &self,
        role: &str,
        password: &str,
    ) -> Result<PgConnection, sqlx::Error> {
        PgConnection::connect_with(&self.postgres.options(role, &self.name).password(password))
            .await
    }

    /// Writes `password` as the password file of `role`.
    pub fn set_password_file(&self, role: &str, password: &str) {
        std::fs::write(self.passwords.join(role), password).expect("write a password file");
    }

    /// Run settings: Postgres only (no ClickHouse), as `role`, short waits.
    pub fn config(&self, role: &str) -> RunConfig {
        RunConfig {
            platform: Platform {
                postgres_host: SERVER_HOST.to_owned(),
                postgres_port: self.postgres.address.port(),
                postgres_database: self.name.clone(),
                postgres_sslmode: SslMode::VerifyFull,
                postgres_ca_file: self.ca_file.clone(),
                clickhouse_url: None,
                clickhouse_ca_file: None,
            },
            postgres: PostgresSettings {
                username: role.to_owned(),
                password: TEST_PASSWORD.to_owned(),
                role_passwords_dir: Some(self.passwords.clone()),
                required_passwords: None,
            },
            clickhouse: None,
            wait: WaitSettings {
                databases: Duration::from_secs(30),
                lock: Duration::from_secs(120),
            },
        }
    }
}

/// The superuser on the `postgres` database.
pub async fn superuser(postgres: &TlsPostgres) -> PgConnection {
    PgConnection::connect_with(&postgres.superuser("postgres"))
        .await
        .expect("connect as the superuser")
}

/// Everything a run can change in a database and in the cluster's roles, as sorted text rows,
/// so two snapshots compare equal exactly when nothing changed. Password verifiers are not
/// readable without a superuser; the snapshot is taken as one (`pg_authid`), so a password that
/// was set again with a new salt shows up too.
pub async fn snapshot(db: &Database) -> Vec<String> {
    let mut conn = PgConnection::connect_with(&db.postgres.superuser(&db.name))
        .await
        .expect("connect as the superuser");
    let queries: [&'static str; 8] = [
        "SELECT format('role %s super=%s inherit=%s createrole=%s createdb=%s login=%s repl=%s bypassrls=%s config=%s password=%s',
            rolname, rolsuper, rolinherit, rolcreaterole, rolcreatedb, rolcanlogin, rolreplication,
            rolbypassrls,
            (SELECT array_agg(setconfig::text ORDER BY setdatabase) FROM pg_db_role_setting s
             WHERE s.setrole = a.oid), rolpassword)
         FROM pg_authid a WHERE rolname !~ '^pg_' AND rolname <> 'postgres'",
        "SELECT format('member %s of %s admin=%s inherit=%s set=%s by %s', u.rolname, g.rolname,
            m.admin_option, m.inherit_option, m.set_option, b.rolname)
         FROM pg_auth_members m JOIN pg_roles u ON u.oid = m.member
         JOIN pg_roles g ON g.oid = m.roleid JOIN pg_roles b ON b.oid = m.grantor",
        "SELECT format('schema %s owner=%s acl=%s', nspname, pg_get_userbyid(nspowner), nspacl)
         FROM pg_namespace WHERE nspname !~ '^(pg_|information_schema)'",
        "SELECT format('relation %s.%s kind=%s owner=%s acl=%s rls=%s cols=%s', n.nspname, c.relname,
            c.relkind, pg_get_userbyid(c.relowner), c.relacl, c.relrowsecurity,
            (SELECT string_agg(attname || ':' || format_type(atttypid, atttypmod), ',' ORDER BY attnum)
             FROM pg_attribute WHERE attrelid = c.oid AND attnum > 0 AND NOT attisdropped))
         FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
         WHERE n.nspname !~ '^(pg_|information_schema)'",
        "SELECT format('policy %s on %s roles=%s cmd=%s permissive=%s qual=%s check=%s', polname,
            polrelid::regclass, polroles::regrole[], polcmd, polpermissive,
            pg_get_expr(polqual, polrelid), pg_get_expr(polwithcheck, polrelid))
         FROM pg_policy",
        "SELECT format('function %s.%s owner=%s acl=%s body=%s', n.nspname, p.proname,
            pg_get_userbyid(p.proowner), p.proacl, md5(p.prosrc))
         FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace
         WHERE n.nspname !~ '^(pg_|information_schema)'",
        "SELECT format('database %s owner=%s acl=%s', datname, pg_get_userbyid(datdba), datacl)
         FROM pg_database WHERE datname = current_database()",
        "SELECT format('applied %s/%s %s at %s', component, name, sha256, applied_at)
         FROM trustiot_migrate.applied",
    ];
    let mut rows = Vec::new();
    for query in queries {
        let mut found: Vec<String> = sqlx::query_scalar(query)
            .fetch_all(&mut conn)
            .await
            .unwrap_or_else(|error| panic!("snapshot query failed: {error}\n{query}"));
        rows.append(&mut found);
    }
    rows.sort();
    rows
}

/// A throwaway ClickHouse (memory capped at 1500 MB), removed when dropped.
pub struct ClickHouse {
    pub url: String,
    pub admin_password: String,
    client: migrate::http::Client,
    _container: Container,
}

impl ClickHouse {
    pub async fn start() -> Option<Self> {
        let Some(runtime) = container_runtime() else {
            unavailable("podman or docker");
            return None;
        };
        let admin_password = format!("ch-admin-{}", unique_name("pw"));
        let container = Container {
            runtime,
            name: unique_name("clickhouse"),
        };
        let user_env = format!("CLICKHOUSE_USER={CLICKHOUSE_ADMIN}");
        let password_env = format!("CLICKHOUSE_PASSWORD={admin_password}");
        runtime_output(
            runtime,
            &[
                "run",
                "--detach",
                "--name",
                &container.name,
                "--memory",
                "1500m",
                "--env",
                &user_env,
                "--env",
                &password_env,
                "--env",
                "CLICKHOUSE_DEFAULT_ACCESS_MANAGEMENT=1",
                "--publish",
                "127.0.0.1::8123",
                CLICKHOUSE_IMAGE,
            ],
        );
        let address = container.host_address("8123/tcp");
        let clickhouse = Self {
            url: format!("http://{address}"),
            admin_password,
            // The Job's own client.
            client: migrate::http::plain(),
            _container: container,
        };
        let started = Instant::now();
        loop {
            if clickhouse.query("SELECT 1").await.is_ok() {
                return Some(clickhouse);
            }
            assert!(
                started.elapsed() < CLICKHOUSE_START,
                "ClickHouse did not accept the admin account within {CLICKHOUSE_START:?}"
            );
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    /// Runs `query` as the admin account.
    pub async fn query(&self, query: &str) -> Result<String, String> {
        self.query_as(CLICKHOUSE_ADMIN, &self.admin_password, query)
            .await
    }

    pub async fn query_as(
        &self,
        user: &str,
        password: &str,
        query: &str,
    ) -> Result<String, String> {
        let auth = migrate::http::Auth::Basic {
            username: user,
            password,
        };
        let response = self
            .client
            .post(&format!("{}/", self.url), &[], auth, query.to_owned())
            .await
            .map_err(|error| error.to_string())?;
        if response.status.is_success() {
            Ok(response.body)
        } else {
            Err(response.body)
        }
    }

    /// Settings that point a run at this ClickHouse.
    pub fn settings(&self, collector: Option<(&str, &str)>) -> ClickHouseSettings {
        ClickHouseSettings {
            username: CLICKHOUSE_ADMIN.to_owned(),
            password: self.admin_password.clone(),
            collector_username: collector.map(|(user, _)| user.to_owned()),
            collector_password: collector.map(|(_, password)| password.to_owned()),
        }
    }
}
