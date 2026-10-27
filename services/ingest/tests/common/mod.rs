//! Shared helpers for the ClickHouse schema tests: reading the migration files and running a
//! real ClickHouse over its HTTP interface.

#![allow(
    dead_code,
    reason = "each test binary uses a different subset of these helpers"
)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The ClickHouse image the tests run: clickhouse/clickhouse-server 26.8 LTS (decision 14), pinned
/// to the multi-arch index digest. The local dev environment (`scripts/lib/stack.ts`) and the
/// Helm chart (`deploy/chart/values.yaml`) pin the same tag and digest; `bun test deploy/chart`
/// checks that all three agree, and Renovate updates the three in one pull request.
pub const CLICKHOUSE_IMAGE: &str = "docker.io/clickhouse/clickhouse-server:26.8@sha256:d426d2f612e1e189c90f9a3f8dcd85a03c6851273833817a296bb327e63aa7a7";

/// The bootstrap admin account, as the chart and the dev environment name it.
pub const ADMIN_USER: &str = "trustiot_admin";

/// The roles a user holds as default roles. `CREATE USER ... DEFAULT ROLE <role>` grants the role
/// as well as making it a default role (ClickHouse 26.8; tools/migrate creates the collector's
/// account the same way), which [`ClickHouse::assert_default_role`] checks.
const DEFAULT_ROLES: &str = "SELECT granted_role_name FROM system.role_grants
WHERE user_name = {user:String} AND granted_role_is_default
ORDER BY granted_role_name FORMAT TSV";

/// One migration file.
pub struct Migration {
    /// File name, for example `0002_telemetry.sql`.
    pub name: String,
    /// Statements in file order, without comments or the closing semicolon.
    pub statements: Vec<String>,
}

pub fn migrations_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations/clickhouse")
}

/// Every migration file in apply order (file name order).
pub fn migrations() -> Vec<Migration> {
    let dir = migrations_dir();
    let mut names: Vec<String> = fs::read_dir(&dir)
        .unwrap_or_else(|error| panic!("read {}: {error}", dir.display()))
        .map(|entry| {
            entry
                .expect("read a directory entry")
                .file_name()
                .into_string()
                .expect("migration file names are UTF-8")
        })
        .collect();
    names.sort();
    names
        .into_iter()
        .map(|name| {
            let text = fs::read_to_string(dir.join(&name))
                .unwrap_or_else(|error| panic!("read {name}: {error}"));
            let statements =
                split_statements(&text).unwrap_or_else(|error| panic!("{name}: {error}"));
            Migration { name, statements }
        })
        .collect()
}

/// Splits a migration file into statements. The file format keeps this unambiguous, so the
/// migration Job can apply files one statement at a time over HTTP:
/// - a line whose first non-blank characters are `--` is a comment and is dropped;
/// - a statement ends with `;` at the end of a line, and `;` appears nowhere else.
pub fn split_statements(text: &str) -> Result<Vec<String>, String> {
    let mut statements = Vec::new();
    let mut current = String::new();
    for (index, line) in text.lines().enumerate() {
        let trimmed = line.trim_end();
        if trimmed.trim_start().starts_with("--") {
            continue;
        }
        let (body, ends) = match trimmed.strip_suffix(';') {
            Some(body) => (body, true),
            None => (trimmed, false),
        };
        if body.contains(';') {
            return Err(format!(
                "line {}: `;` may only end a statement, at the end of a line",
                index + 1
            ));
        }
        if !body.trim().is_empty() {
            current.push_str(body);
            current.push('\n');
        }
        if ends {
            let statement = current.trim().to_owned();
            if statement.is_empty() {
                return Err(format!("line {}: empty statement", index + 1));
            }
            statements.push(statement);
            current.clear();
        }
    }
    if !current.trim().is_empty() {
        return Err("the last statement does not end with `;`".to_owned());
    }
    Ok(statements)
}

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
fn container_runtime() -> Option<&'static str> {
    ["podman", "docker"].into_iter().find(|runtime| {
        Command::new(runtime)
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    })
}

fn runtime_output(runtime: &str, args: &[&str]) -> String {
    let output = Command::new(runtime)
        .args(args)
        .output()
        .unwrap_or_else(|error| panic!("{runtime} {}: {error}", args.join(" ")));
    assert!(
        output.status.success(),
        "{runtime} {} failed: {}",
        args.first().copied().unwrap_or_default(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// A container that is removed when dropped.
struct Container {
    runtime: &'static str,
    name: String,
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

/// An error answer from ClickHouse.
#[derive(Debug)]
pub struct QueryError {
    pub status: u16,
    pub body: String,
}

impl QueryError {
    /// ClickHouse's error name, for example `ACCESS_DENIED`.
    /// It is the last parenthesised upper-case word of the message, such as `(ACCESS_DENIED)`.
    pub fn code(&self) -> &str {
        self.body
            .split('(')
            .filter_map(|part| part.split_once(')').map(|(inside, _)| inside))
            .rfind(|inside| {
                inside.len() > 2
                    && inside
                        .chars()
                        .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
            })
            .unwrap_or("UNKNOWN")
    }
}

/// ClickHouse's answer to one statement, with the rows it read and wrote.
#[derive(Debug)]
pub struct Answer {
    pub result: Result<String, QueryError>,
    /// `read_rows` of the `X-ClickHouse-Summary` header, when ClickHouse sent one.
    pub read_rows: Option<u64>,
    /// `written_rows` of the same header: the rows an INSERT wrote, its materialized views'
    /// rows included.
    pub written_rows: Option<u64>,
}

/// An account to run queries as.
#[derive(Clone, Debug)]
pub struct Account {
    pub user: String,
    pub password: String,
}

/// A ClickHouse server for one test binary: `TEST_CLICKHOUSE_URL` with `TEST_CLICKHOUSE_USER`
/// and `TEST_CLICKHOUSE_PASSWORD` (an account with access management) when set, otherwise a
/// throwaway container started with podman or docker. The container is capped at 1500 MB of
/// memory and removed when this value is dropped.
pub struct ClickHouse {
    pub url: String,
    pub admin: Account,
    client: reqwest::Client,
    container: Option<Container>,
}

impl ClickHouse {
    pub async fn start() -> Option<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .expect("build the HTTP client");
        if let Some(url) = std::env::var("TEST_CLICKHOUSE_URL")
            .ok()
            .filter(|url| !url.is_empty())
        {
            let admin = Account {
                user: std::env::var("TEST_CLICKHOUSE_USER")
                    .expect("TEST_CLICKHOUSE_USER is required with TEST_CLICKHOUSE_URL"),
                password: std::env::var("TEST_CLICKHOUSE_PASSWORD")
                    .expect("TEST_CLICKHOUSE_PASSWORD is required with TEST_CLICKHOUSE_URL"),
            };
            let server = Self {
                url,
                admin,
                client,
                container: None,
            };
            server.wait_ready().await;
            return Some(server);
        }
        let Some(runtime) = container_runtime() else {
            unavailable("TEST_CLICKHOUSE_URL, podman, or docker");
            return None;
        };
        Some(Self::start_container(runtime, client, &[]).await)
    }

    /// Always a throwaway container, whatever `TEST_CLICKHOUSE_URL` says: for a test that changes
    /// server-wide state a shared server must not keep, such as stopped merges or an account that
    /// reads every row (tools/lab-dataset/tests/clickhouse_load.rs).
    pub async fn start_throwaway() -> Option<Self> {
        let Some(runtime) = container_runtime() else {
            unavailable("podman or docker");
            return None;
        };
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .expect("build the HTTP client");
        Some(Self::start_container(runtime, client, &[]).await)
    }

    /// A throwaway container whose data directory is a tmpfs of `size` (for example `256m`), so a
    /// test can fill the server's disk with [`ClickHouse::exec`]. Always a container, whatever
    /// `TEST_CLICKHOUSE_URL` says: a shared server's disk is not the test's to fill.
    pub async fn start_with_small_disk(size: &str) -> Option<Self> {
        let Some(runtime) = container_runtime() else {
            unavailable("podman or docker");
            return None;
        };
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .expect("build the HTTP client");
        // A tmpfs replaces the image's anonymous volume for the data directory.
        let tmpfs = format!("/var/lib/clickhouse:rw,size={size}");
        Some(Self::start_container(runtime, client, &["--tmpfs", &tmpfs]).await)
    }

    /// Runs a command inside the container, for example `fallocate`; panics when it fails or
    /// when this server is not a container the test started.
    pub fn exec(&self, args: &[&str]) -> String {
        let container = self
            .container
            .as_ref()
            .expect("exec needs a ClickHouse container that the test started");
        let mut full = vec!["exec", container.name.as_str()];
        full.extend_from_slice(args);
        runtime_output(container.runtime, &full)
    }

    async fn start_container(
        runtime: &'static str,
        client: reqwest::Client,
        extra_args: &[&str],
    ) -> Self {
        let password = uuid::Uuid::new_v4().simple().to_string();
        let container = Container {
            runtime,
            name: format!(
                "ingest-test-clickhouse-{}-{}",
                std::process::id(),
                uuid::Uuid::new_v4().simple()
            ),
        };
        let user_env = format!("CLICKHOUSE_USER={ADMIN_USER}");
        let password_env = format!("CLICKHOUSE_PASSWORD={password}");
        let mut args = vec![
            "run",
            "--detach",
            "--name",
            &container.name,
            // The test machine is shared; ClickHouse sizes its caches from this limit.
            "--memory",
            "1500m",
            "--env",
            &user_env,
            "--env",
            &password_env,
            // The bootstrap admin may manage access entities with SQL, as in the chart.
            "--env",
            "CLICKHOUSE_DEFAULT_ACCESS_MANAGEMENT=1",
            "--publish",
            "127.0.0.1::8123",
        ];
        args.extend_from_slice(extra_args);
        args.push(CLICKHOUSE_IMAGE);
        runtime_output(runtime, &args);
        let mapping = runtime_output(runtime, &["port", &container.name, "8123/tcp"]);
        let address = mapping
            .lines()
            .find(|line| !line.trim().is_empty())
            .unwrap_or_else(|| panic!("no host address for 8123/tcp: {mapping}"))
            .trim()
            .to_owned();
        let server = Self {
            url: format!("http://{address}"),
            admin: Account {
                user: ADMIN_USER.to_owned(),
                password,
            },
            client,
            container: Some(container),
        };
        server.wait_ready().await;
        server
    }

    async fn wait_ready(&self) {
        let started = Instant::now();
        loop {
            if let Ok(answer) = self.admin("SELECT 1").await
                && answer.trim() == "1"
            {
                return;
            }
            assert!(
                started.elapsed() < Duration::from_secs(90),
                "ClickHouse at {} did not accept the admin login in time",
                self.url
            );
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    /// Runs one statement as `account`; `params` go into the URL query (settings, query
    /// parameters, session_id).
    pub async fn query_with(
        &self,
        account: &Account,
        sql: &str,
        params: &[(&str, &str)],
    ) -> Result<String, QueryError> {
        self.send(account, sql, params).await.result
    }

    /// Runs one statement as `account` and also returns how many rows it read.
    pub async fn query_with_stats(&self, account: &Account, sql: &str) -> Answer {
        self.send(account, sql, &[]).await
    }

    /// Runs one statement as `account` with `params` and also returns how many rows it read and
    /// wrote.
    pub async fn query_with_summary(
        &self,
        account: &Account,
        sql: &str,
        params: &[(&str, &str)],
    ) -> Answer {
        self.send(account, sql, params).await
    }

    /// Runs an INSERT whose rows are the request body, as a client sends data: `insert` is static
    /// SQL that ends in `FORMAT <format>` and goes in the URL's `query` parameter, `data` the rows
    /// in that format, and `params` (settings, query parameters) go in the URL as well.
    pub async fn insert_data(
        &self,
        account: &Account,
        insert: &str,
        data: String,
        params: &[(&str, &str)],
    ) -> Answer {
        let mut pairs = vec![("query", insert)];
        pairs.extend_from_slice(params);
        self.post(account, &pairs, data).await
    }

    async fn send(&self, account: &Account, sql: &str, params: &[(&str, &str)]) -> Answer {
        self.post(account, params, sql.to_owned()).await
    }

    async fn post(&self, account: &Account, params: &[(&str, &str)], body: String) -> Answer {
        let mut url = reqwest::Url::parse(&self.url).expect("valid ClickHouse URL");
        if !params.is_empty() {
            url.query_pairs_mut().extend_pairs(params);
        }
        let response = match self
            .client
            .post(url)
            .header("X-ClickHouse-User", &account.user)
            .header("X-ClickHouse-Key", &account.password)
            .body(body)
            .send()
            .await
        {
            Ok(response) => response,
            Err(error) => {
                return Answer {
                    result: Err(QueryError {
                        status: 0,
                        body: error.to_string(),
                    }),
                    read_rows: None,
                    written_rows: None,
                };
            }
        };
        let status = response.status().as_u16();
        // X-ClickHouse-Summary is JSON with every counter as a string, for example
        // {"read_rows":"7",...}.
        let summary = response
            .headers()
            .get("X-ClickHouse-Summary")
            .and_then(|value| value.to_str().ok())
            .and_then(|summary| serde_json::from_str::<serde_json::Value>(summary).ok());
        let counter = |name: &str| {
            summary
                .as_ref()
                .and_then(|summary| summary[name].as_str()?.parse().ok())
        };
        let (read_rows, written_rows) = (counter("read_rows"), counter("written_rows"));
        let result = match response.text().await {
            Ok(body) if (200..300).contains(&status) => Ok(body),
            Ok(body) => Err(QueryError { status, body }),
            Err(error) => Err(QueryError {
                status,
                body: error.to_string(),
            }),
        };
        Answer {
            result,
            read_rows,
            written_rows,
        }
    }

    pub async fn query(&self, account: &Account, sql: &str) -> Result<String, QueryError> {
        self.query_with(account, sql, &[]).await
    }

    /// Runs one statement as the admin and panics on an error.
    pub async fn admin(&self, sql: &str) -> Result<String, QueryError> {
        self.query(&self.admin, sql).await
    }

    pub async fn admin_ok(&self, sql: &str) -> String {
        self.admin(sql)
            .await
            .unwrap_or_else(|error| panic!("admin query failed: {sql}\n{}", error.body))
    }

    /// Asserts that `user` holds `role`, granted, as its only default role.
    pub async fn assert_default_role(&self, user: &str, role: &str) {
        let roles = self
            .query_with(&self.admin, DEFAULT_ROLES, &[("param_user", user)])
            .await
            .unwrap_or_else(|error| panic!("read the roles of {user}: {}", error.body));
        assert_eq!(
            roles.trim(),
            role,
            "{user} must hold {role} as its only default role"
        );
    }

    /// Applies every migration file, statement by statement, as the admin.
    pub async fn apply_migrations(&self) {
        for migration in migrations() {
            for statement in &migration.statements {
                if let Err(error) = self.admin(statement).await {
                    panic!(
                        "{}: statement failed:\n{statement}\n{}",
                        migration.name, error.body
                    );
                }
            }
        }
    }
}
