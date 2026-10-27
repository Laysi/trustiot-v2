//! `migrate bootstrap-platform-admin` (Laysi/trustiot-v2#582), run as the binary the chart
//! runs, with the address on stdin: it creates the first platform administrator once ever, the
//! loser of two concurrent runs and every later run exit with code 3 and change nothing, an
//! existing user is upgraded instead of duplicated, and no address ever reaches stdout, stderr
//! or the event. The recovery step of docs/runbooks/platform-admin.md (deleting the marker)
//! makes it work once more.
//!
//! Needs podman or docker; skipped without one unless `CI` is set. One test: roles are
//! cluster-wide, and this binary has its own Postgres.

mod support;

use std::io::Write as _;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use migrate::registry::PRODUCT;
use serde_json::Value;
use sqlx::{Connection, PgConnection};
use support::postgres::{SERVER_HOST, TEST_PASSWORD, tls_postgres};
use support::{Database, MIGRATOR, MIGRATOR_INIT};

const CREATE: &str = "CREATE DATABASE migrate_platform_admin OWNER trustiot_owner";
const DATABASE: &str = "migrate_platform_admin";
const BINARY: &str = env!("CARGO_BIN_EXE_migrate");

/// The addresses the test uses: example.com only (the repository is public).
const FIRST: &str = "first.admin@example.com";
const SECOND: &str = "second.admin@example.com";
const LATER: &str = "later.admin@example.com";
const EXISTING: &str = "existing.admin@example.com";

/// How long the test waits for both concurrent runs to queue on the marker table, and for a run
/// to end; neither depends on the machine being fast.
const QUEUE_BOUND: Duration = Duration::from_secs(120);
const RUN_BOUND: Duration = Duration::from_secs(180);

const MARKERS: &str = "SELECT count(*) FROM identity.platform_admin_bootstrap";
const MARKER_USER: &str = "SELECT user_id::text FROM identity.platform_admin_bootstrap";
const USERS: &str = "SELECT id::text || ' ' || coalesce(email, '-') || ' ' || platform_admin::text \
     FROM identity.users ORDER BY 1";
const ADMIN_OF: &str = "SELECT id::text, platform_admin FROM identity.users WHERE email = $1";
const GRANTED_EVENTS: &str = "SELECT envelope::text FROM identity.outbox_event \
     WHERE event_type = 'identity.platform_admin_granted' ORDER BY tx_order, aggregate_seq";
const ALL_EVENTS: &str = "SELECT count(*) FROM identity.outbox_event";
const LOCK_MARKER: &str = "LOCK TABLE identity.platform_admin_bootstrap IN ACCESS EXCLUSIVE MODE";
const WAITING_RUNS: &str = "SELECT count(*) FROM pg_stat_activity \
     WHERE application_name = 'trustiot-migrate' AND wait_event_type = 'Lock'";
const ADD_EXISTING_USER: &str =
    "INSERT INTO identity.users (id, email) VALUES (gen_random_uuid(), $1)";
/// The recovery step of docs/runbooks/platform-admin.md, as written there.
const RECOVERY: &str = "DELETE FROM identity.platform_admin_bootstrap";

/// One finished run of the binary.
struct Run {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

impl Run {
    /// The `user_id` field of the log line with `message`.
    fn user_id(&self, message: &str) -> String {
        self.stdout
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .find(|line| find(line, "message").and_then(Value::as_str) == Some(message))
            .and_then(|line| {
                find(&line, "user_id")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| panic!("no log line {message:?} with a user_id:\n{}", self.stdout))
    }

    /// No address of the test, in any spelling, appears in what the run printed.
    fn assert_no_address(&self) {
        let printed = format!("{}\n{}", self.stdout, self.stderr).to_ascii_lowercase();
        assert!(
            !printed.contains("@example.com"),
            "the run printed an address:\n{printed}"
        );
    }
}

/// The first value of `key` anywhere in a JSON log line (the tracing layout nests fields).
fn find<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    match value {
        Value::Object(map) => map
            .get(key)
            .or_else(|| map.values().find_map(|nested| find(nested, key))),
        _ => None,
    }
}

/// The binary with the settings the chart gives it, as the migration role; `args` after the
/// command.
fn command(db: &Database, args: &[&str]) -> Command {
    let mut command = Command::new(BINARY);
    command
        .env_clear()
        .arg("bootstrap-platform-admin")
        .args(args)
        .env("TRUSTIOT_POSTGRES_HOST", SERVER_HOST)
        .env(
            "TRUSTIOT_POSTGRES_PORT",
            db.postgres.address.port().to_string(),
        )
        .env("TRUSTIOT_POSTGRES_DATABASE", DATABASE)
        .env("TRUSTIOT_POSTGRES_SSLMODE", "verify-full")
        .env("TRUSTIOT_POSTGRES_CA_FILE", &db.ca_file)
        .env("TRUSTIOT__POSTGRES__USERNAME", MIGRATOR)
        .env("TRUSTIOT__POSTGRES__PASSWORD", TEST_PASSWORD)
        .env("TRUSTIOT__WAIT__DATABASES", "30s")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

/// Starts the binary and writes `input` to its stdin, keeping stdin open as `kubectl attach`
/// does until the command ends; [`finish`] collects it.
fn start(db: &Database, args: &[&str], input: &str) -> std::process::Child {
    let mut child = command(db, args).spawn().expect("start migrate");
    let mut stdin = child.stdin.take().expect("piped stdin");
    stdin.write_all(input.as_bytes()).expect("write stdin");
    stdin.flush().expect("flush stdin");
    // Kept open in a thread that outlives the read: the command must stop reading at the line's
    // end, not at the end of the input.
    std::thread::spawn(move || {
        std::thread::sleep(RUN_BOUND);
        drop(stdin);
    });
    child
}

fn finish(child: std::process::Child) -> Run {
    let started = Instant::now();
    let output = child.wait_with_output().expect("wait for migrate");
    assert!(
        started.elapsed() < RUN_BOUND,
        "migrate bootstrap-platform-admin did not end within {RUN_BOUND:?} (it must not wait for \
         stdin to close)"
    );
    Run {
        code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

async fn run(db: &Database, args: &[&str], input: &str) -> Run {
    let child = start(db, args, input);
    tokio::task::spawn_blocking(move || finish(child))
        .await
        .expect("the run's thread")
}

async fn superuser(db: &Database) -> PgConnection {
    PgConnection::connect_with(&db.postgres.superuser(&db.name))
        .await
        .expect("connect as the superuser")
}

async fn count(conn: &mut PgConnection, query: &'static str) -> i64 {
    sqlx::query_scalar(query)
        .fetch_one(conn)
        .await
        .unwrap_or_else(|error| panic!("{query}: {error}"))
}

/// What the command may change: the users, the marker and the identity outbox.
async fn state(conn: &mut PgConnection) -> (Vec<String>, i64, Vec<String>, i64) {
    let users: Vec<String> = sqlx::query_scalar(USERS)
        .fetch_all(&mut *conn)
        .await
        .expect("read the users");
    let markers = count(conn, MARKERS).await;
    let events: Vec<String> = sqlx::query_scalar(GRANTED_EVENTS)
        .fetch_all(&mut *conn)
        .await
        .expect("read the events");
    let all = count(conn, ALL_EVENTS).await;
    (users, markers, events, all)
}

#[tokio::test(flavor = "multi_thread")]
async fn the_first_platform_admin_is_created_once() {
    let Some(postgres) = tls_postgres(MIGRATOR_INIT).await else {
        return;
    };
    let db = Database::create(postgres, CREATE, DATABASE).await;

    // 1. Before the release's migration run, the command fails and changes nothing.
    let early = run(&db, &[], &format!("{FIRST}\n")).await;
    assert_eq!(early.code, Some(1), "{}{}", early.stdout, early.stderr);
    early.assert_no_address();

    migrate::run(&db.config(MIGRATOR), &PRODUCT)
        .await
        .expect("the migration run");
    let mut admin = superuser(&db).await;
    let empty = state(&mut admin).await;
    assert_eq!(empty.1, 0, "no marker before the first run");

    // 2. Unusable input or an argument: exit code 2, nothing changes. The address is never an
    //    argument, and an error never quotes the input.
    for input in ["not-an-address\n", "\n", "first admin@example.com\n"] {
        let refused = run(&db, &[], input).await;
        assert_eq!(refused.code, Some(2), "{input:?}: {}", refused.stdout);
        refused.assert_no_address();
    }
    let with_argument = run(&db, &[FIRST], "").await;
    assert_eq!(with_argument.code, Some(2), "{}", with_argument.stdout);
    with_argument.assert_no_address();
    assert_eq!(
        state(&mut admin).await,
        empty,
        "refused runs change nothing"
    );

    // 3. Two runs at the same time: both queue on the marker table (held here so neither can
    //    finish first), then exactly one creates its user and the other exits with code 3.
    let mut holder = superuser(&db).await;
    let mut held = holder.begin().await.expect("begin");
    sqlx::query(LOCK_MARKER)
        .execute(&mut *held)
        .await
        .expect("lock the marker table");
    let first = start(&db, &[], &format!("{FIRST}\n"));
    let second = start(&db, &[], "  Second.Admin@EXAMPLE.com \n");
    let started = Instant::now();
    loop {
        let waiting = count(&mut admin, WAITING_RUNS).await;
        if waiting == 2 {
            break;
        }
        assert!(
            started.elapsed() < QUEUE_BOUND,
            "both runs did not queue on the marker table within {QUEUE_BOUND:?} ({waiting} did)"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    held.rollback().await.expect("release the marker table");
    let (first, second) = tokio::task::spawn_blocking(move || (finish(first), finish(second)))
        .await
        .expect("the runs' thread");
    first.assert_no_address();
    second.assert_no_address();
    let mut codes = [first.code, second.code];
    codes.sort_unstable();
    assert_eq!(
        codes,
        [Some(0), Some(3)],
        "one run wins, the other finds its marker:\n{}\n{}",
        first.stdout,
        second.stdout
    );
    let (winner, winner_email, loser_email, loser) = if first.code == Some(0) {
        (&first, FIRST, SECOND, &second)
    } else {
        (&second, SECOND, FIRST, &first)
    };
    let user_id = winner.user_id("created the first platform administrator");
    assert_eq!(
        loser.user_id(
            "the first platform administrator was created before; nothing changed \
             (docs/runbooks/platform-admin.md)"
        ),
        user_id,
        "the loser names the winner's user"
    );

    // 4. The winner's user is a platform administrator with its normalized address; the loser
    //    left nothing; the marker names the user; one event with the user ID and `via` only.
    let (id, platform_admin): (String, bool) = sqlx::query_as(ADMIN_OF)
        .bind(winner_email)
        .fetch_one(&mut admin)
        .await
        .expect("the winner's user");
    assert_eq!((id.as_str(), platform_admin), (user_id.as_str(), true));
    let loser_user: Option<(String, bool)> = sqlx::query_as(ADMIN_OF)
        .bind(loser_email)
        .fetch_optional(&mut admin)
        .await
        .expect("look for the loser's user");
    assert_eq!(loser_user, None, "the loser created no user");
    let marker: String = sqlx::query_scalar(MARKER_USER)
        .fetch_one(&mut admin)
        .await
        .expect("the marker");
    assert_eq!(marker, user_id);
    let after_first = state(&mut admin).await;
    assert_eq!(after_first.1, 1);
    assert_eq!(after_first.0.len(), empty.0.len() + 1, "one new user");
    assert_eq!(after_first.2.len(), 1, "one grant event");
    assert_eq!(after_first.3, empty.3 + 1, "nothing else in the outbox");
    let envelope: Value = serde_json::from_str(&after_first.2[0]).expect("the envelope");
    assert_eq!(
        envelope["payload"],
        serde_json::json!({"user_id": user_id, "via": "bootstrap"})
    );
    assert_eq!(envelope["producer"], "identity");
    assert_eq!(envelope["tenant_id"], Value::Null);
    assert!(
        !after_first.2[0].contains('@'),
        "the event carries no address: {}",
        after_first.2[0]
    );

    // 5. Every later run, with a new address or the winner's, exits with code 3 and changes
    //    nothing.
    for input in [format!("{LATER}\n"), format!("{winner_email}\n")] {
        let later = run(&db, &[], &input).await;
        assert_eq!(later.code, Some(3), "{}", later.stdout);
        later.assert_no_address();
        assert_eq!(
            state(&mut admin).await,
            after_first,
            "a later run changes nothing"
        );
    }

    // 6. Once every platform administrator is gone, the command still does not run again: the
    //    marker stays.
    sqlx::query("UPDATE identity.users SET platform_admin = false")
        .execute(&mut admin)
        .await
        .expect("remove every platform administrator");
    let without_admins = state(&mut admin).await;
    let again = run(&db, &[], &format!("{LATER}\n")).await;
    assert_eq!(again.code, Some(3), "{}", again.stdout);
    assert_eq!(state(&mut admin).await, without_admins);

    // 7. Recovery (docs/runbooks/platform-admin.md): a database administrator deletes the
    //    marker; the next run, for the address of an existing user, upgrades that user instead
    //    of creating another one, whatever the spelling of the address.
    sqlx::query(ADD_EXISTING_USER)
        .bind(EXISTING)
        .execute(&mut admin)
        .await
        .expect("add an existing user");
    let (existing_id, existing_admin): (String, bool) = sqlx::query_as(ADMIN_OF)
        .bind(EXISTING)
        .fetch_one(&mut admin)
        .await
        .expect("the existing user");
    assert!(!existing_admin);
    sqlx::query(RECOVERY)
        .execute(&mut admin)
        .await
        .expect("delete the marker");
    let users_before = state(&mut admin).await.0.len();
    let recovered = run(&db, &[], "  Existing.Admin@Example.COM\r\n").await;
    assert_eq!(recovered.code, Some(0), "{}", recovered.stdout);
    recovered.assert_no_address();
    assert_eq!(
        recovered.user_id("created the first platform administrator"),
        existing_id
    );
    assert!(
        recovered.stdout.contains("\"existing_user\":true"),
        "{}",
        recovered.stdout
    );
    let (id, platform_admin): (String, bool) = sqlx::query_as(ADMIN_OF)
        .bind(EXISTING)
        .fetch_one(&mut admin)
        .await
        .expect("the existing user");
    assert_eq!((id.as_str(), platform_admin), (existing_id.as_str(), true));
    let after_recovery = state(&mut admin).await;
    assert_eq!(after_recovery.0.len(), users_before, "no new user");
    assert_eq!(after_recovery.1, 1, "a new marker");
    assert_eq!(
        after_recovery.2.len(),
        2,
        "the operation log keeps both grants"
    );
    let marker: String = sqlx::query_scalar(MARKER_USER)
        .fetch_one(&mut admin)
        .await
        .expect("the marker");
    assert_eq!(marker, existing_id);
    let again = run(&db, &[], &format!("{EXISTING}\n")).await;
    assert_eq!(again.code, Some(3), "{}", again.stdout);
}
