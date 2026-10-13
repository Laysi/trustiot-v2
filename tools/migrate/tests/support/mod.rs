//! Helpers of the migration Job's integration tests: a throwaway Postgres with TLS (shared with
//! crates/service-kit's tests), a throwaway ClickHouse, a fake Kubernetes API that holds the
//! Secrets the Job fills (`secrets`), run settings pointing at them, and a snapshot of what a run
//! can change, to prove that a second run changes nothing.
//!
//! Roles are cluster-wide, so each test binary that runs the product registry has one test and
//! its own Postgres container (the shared container lives as long as one binary).
#![allow(
    dead_code,
    reason = "each test binary includes this module and uses a different subset of it"
)]

#[path = "../../../../crates/service-kit/tests/support/postgres.rs"]
pub mod postgres;

/// The start of that Postgres container (`postgres` above uses it): one logic for every crate's
/// test containers.
#[path = "../../../../crates/service-kit/tests/support/start.rs"]
pub mod start;

pub mod secrets;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use migrate::config::{
    ClickHouseSettings, PasswordSettings, Platform, PostgresSettings, RunConfig, WaitSettings,
};
use service_kit::postgres::SslMode;
use sqlx::{Connection, PgConnection};

use postgres::{SERVER_HOST, TEST_PASSWORD, TlsPostgres};
use secrets::FakeSecrets;

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
            // `--volumes`: the image's anonymous volumes go with the container (Laysi/trustiot-v2#334).
            .args(["rm", "--force", "--volumes", &self.name])
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

/// A fresh database owned by the migration role, with the server's CA in a file, and the fake
/// Kubernetes API that holds the Secrets of the login roles the tests bind.
pub struct Database {
    pub postgres: std::sync::Arc<TlsPostgres>,
    pub name: String,
    pub ca_file: PathBuf,
    pub secrets: FakeSecrets,
    /// Login roles whose passwords the run manages (`passwords.postgres`).
    bound: Mutex<Vec<String>>,
    /// `passwords.generation`
    generation: Mutex<String>,
}

/// The Secret the tests bind to `role`: one per role, as the chart declares one per service.
pub fn secret_of(role: &str) -> String {
    format!("{}-postgres", role.replace('_', "-"))
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
        Self {
            postgres,
            name,
            ca_file,
            secrets: FakeSecrets::start().await,
            bound: Mutex::default(),
            generation: Mutex::new("1".to_owned()),
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

    /// Lets the run manage `role`'s password: declares its empty Secret, as the chart does, and
    /// binds it in every later `config`.
    pub fn bind(&self, role: &str) {
        self.secrets.declare(&secret_of(role));
        self.bound
            .lock()
            .expect("bound roles")
            .push(role.to_owned());
    }

    /// Stops binding `role`, as the chart does when its service is turned off.
    pub fn unbind(&self, role: &str) {
        self.bound
            .lock()
            .expect("bound roles")
            .retain(|bound| bound != role);
    }

    /// The password the run keeps for `role` in its Secret.
    pub fn password(&self, role: &str) -> String {
        self.secrets
            .value(&secret_of(role), role)
            .unwrap_or_else(|| panic!("the Secret of {role} holds no password"))
    }

    /// Changes `passwords.generation` of every later `config` (a rotation).
    pub fn set_generation(&self, generation: &str) {
        *self.generation.lock().expect("generation") = generation.to_owned();
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
            },
            clickhouse: None,
            passwords: self.passwords(None),
            wait: WaitSettings {
                databases: Duration::from_secs(30),
                lock: Duration::from_secs(120),
            },
        }
    }
}

impl Database {
    /// `passwords.*` for the bound roles and `collector` (`account=secret`); none when nothing
    /// is bound.
    pub fn passwords(&self, collector: Option<(&str, &str)>) -> Option<PasswordSettings> {
        let bound = self.bound.lock().expect("bound roles").clone();
        if bound.is_empty() && collector.is_none() {
            return None;
        }
        let secrets: Vec<String> = bound.iter().map(|role| secret_of(role)).collect();
        let pairs: Vec<(&str, &str)> = bound
            .iter()
            .map(String::as_str)
            .zip(secrets.iter().map(String::as_str))
            .collect();
        let generation = self.generation.lock().expect("generation").clone();
        Some(self.secrets.settings(&generation, &pairs, collector))
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
    pub fn settings(&self) -> ClickHouseSettings {
        ClickHouseSettings {
            username: CLICKHOUSE_ADMIN.to_owned(),
            password: self.admin_password.clone(),
        }
    }
}
