//! 0006_answer_removed_content.sql (Laysi/trustiot-v2#646) as the migration Job applies it to a
//! database the previous release filled, with `assistant.runs` busy: the Job runs the whole file
//! in one transaction and holds every lock until the commit, so the file builds the index on
//! `assistant.runs` first and alters `assistant.messages` last (the security re-review's LOW-2).
//! While the index waits for a transaction that writes runs, the Job holds no lock on the
//! messages, so reading a conversation does not wait behind the index's scan.
//!
//! Starts a throwaway Postgres with TLS (podman or docker) of its own; skipped without either
//! unless `CI` is set.

use std::sync::OnceLock;

use migrate::config::{Platform, PostgresSettings, RunConfig, WaitSettings};
use migrate::registry::{PRODUCT, Registry, Service};
use service_kit::postgres::SslMode;
use sqlx::{Connection as _, PgConnection};
use testkit::postgres::{SERVER_HOST, TEST_PASSWORD, TlsPostgres, tls_postgres};
use testkit::product::{DATABASE, MIGRATOR, PRODUCT_INIT, ProductPostgres};
use tokio::time::{Duration, Instant};

/// The assistant's files the previous release applied: up to 0004_dashboard_pages_optional.sql.
const PREVIOUS_FILES: usize = 4;

/// How long the test waits for the Job to block on the busy table, or for it to end.
const WAIT: Duration = Duration::from_secs(120);

/// How often the test looks.
const POLL: Duration = Duration::from_millis(50);

/// The Job's attempts the test makes: one is enough unless the machine is so loaded that the
/// test does not see the Job waiting before the file's own lock timeout (5 s) ends the wait.
const ATTEMPTS: usize = 5;

const CREATE_DATABASE: &str = "CREATE DATABASE trustiot OWNER trustiot_owner";

/// A transaction that writes runs holds this lock on the table until it ends.
const LOCK_RUNS: &str = "LOCK TABLE assistant.runs IN ROW EXCLUSIVE MODE";

/// The session that waits for a lock the session `$1` holds: the Job's.
const WAITING_ON: &str = "SELECT pid FROM pg_stat_activity WHERE $1 = ANY (pg_blocking_pids(pid))";

/// The locks the session `$1` holds on `assistant.messages`, by mode.
const LOCKS_ON_MESSAGES: &str = "SELECT coalesce(jsonb_agg(mode ORDER BY mode), '[]')::text
    FROM pg_locks WHERE pid = $1 AND granted AND relation = 'assistant.messages'::regclass";

/// The files the Job applied in the assistant's schema, by name.
const APPLIED: &str = "SELECT coalesce(jsonb_agg(name ORDER BY name), '[]')::text
    FROM assistant.trustiot_migrate_applied";

/// Whether the index and the column of 0006 are there.
const MIGRATED: &str = "SELECT
    to_regclass('assistant.runs_by_conversation') IS NOT NULL
    AND EXISTS (SELECT 1 FROM information_schema.columns
        WHERE table_schema = 'assistant' AND table_name = 'messages' AND column_name = 'removed')";

/// The registry of the previous release: the product with the assistant's first files only.
fn previous_release() -> Registry {
    static SERVICES: OnceLock<Vec<Service>> = OnceLock::new();
    let services = SERVICES.get_or_init(|| {
        PRODUCT
            .services
            .iter()
            .map(|service| {
                let mut service = *service;
                if service.name == "assistant" {
                    service.migrations = service
                        .migrations
                        .get(..PREVIOUS_FILES)
                        .expect("the assistant has its first files");
                }
                service
            })
            .collect()
    });
    Registry {
        services: services.as_slice(),
        ..PRODUCT
    }
}

fn run_config(postgres: &TlsPostgres, ca_file: &std::path::Path) -> RunConfig {
    RunConfig {
        platform: Platform {
            postgres_host: SERVER_HOST.to_owned(),
            postgres_port: postgres.address.port(),
            postgres_database: DATABASE.to_owned(),
            postgres_sslmode: SslMode::VerifyFull,
            postgres_ca_file: ca_file.to_owned(),
            clickhouse_url: None,
            clickhouse_ca_file: None,
        },
        postgres: PostgresSettings {
            username: MIGRATOR.to_owned(),
            password: TEST_PASSWORD.to_owned(),
        },
        clickhouse: None,
        passwords: None,
        wait: WaitSettings::default(),
    }
}

/// The JSON text `query` returns for the session `pid`, read back.
async fn json_of(admin: &mut PgConnection, query: &'static str, pid: i32) -> serde_json::Value {
    let text: String = sqlx::query_scalar(query)
        .bind(pid)
        .fetch_one(&mut *admin)
        .await
        .unwrap_or_else(|error| panic!("{query}: {error}"));
    serde_json::from_str(&text).expect("JSON text")
}

/// The session waiting for `holder`'s lock, once there is one; `None` when `job` ended first.
async fn waiting_on(
    watcher: &mut PgConnection,
    holder: i32,
    job: &tokio::task::JoinHandle<Result<migrate::Report, migrate::Error>>,
) -> Option<i32> {
    let deadline = Instant::now() + WAIT;
    loop {
        let waiting: Option<i32> = sqlx::query_scalar(WAITING_ON)
            .bind(holder)
            .fetch_optional(&mut *watcher)
            .await
            .expect("look for the waiting session");
        if waiting.is_some() {
            return waiting;
        }
        if job.is_finished() {
            return None;
        }
        assert!(
            Instant::now() < deadline,
            "the Job waiting on the busy runs table: not within {WAIT:?}"
        );
        tokio::time::sleep(POLL).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_runs_index_is_built_before_the_messages_are_locked() {
    let Some(postgres) = tls_postgres(PRODUCT_INIT).await else {
        return;
    };
    let mut admin = PgConnection::connect_with(&postgres.superuser("postgres"))
        .await
        .expect("connect as the superuser");
    sqlx::raw_sql(CREATE_DATABASE)
        .execute(&mut admin)
        .await
        .expect("create the product database");
    admin.close().await.expect("close");
    let dir = std::env::temp_dir().join(format!(
        "assistant-questions-migration-{}-{}",
        std::process::id(),
        postgres.address.port()
    ));
    std::fs::create_dir_all(&dir).expect("a directory for the CA file");
    let ca_file = dir.join("postgres-ca.crt");
    std::fs::write(&ca_file, &postgres.ca_pem).expect("write the CA file");
    let product = ProductPostgres { postgres, ca_file };

    migrate::run(
        &run_config(&product.postgres, &product.ca_file),
        &previous_release(),
    )
    .await
    .unwrap_or_else(|error| panic!("the migration Job's run up to 0004: {error}"));

    let mut watcher = product.superuser().await;
    for attempt in 1..=ATTEMPTS {
        // A transaction that writes runs, open while the Job applies 0006.
        let mut holder = product.superuser().await;
        sqlx::raw_sql("BEGIN")
            .execute(&mut holder)
            .await
            .expect("begin the writer's transaction");
        sqlx::raw_sql(LOCK_RUNS)
            .execute(&mut holder)
            .await
            .expect("lock the runs as a writer does");
        let holder_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut holder)
            .await
            .expect("the writer's process ID");

        let config = run_config(&product.postgres, &product.ca_file);
        let job = tokio::spawn(async move { migrate::run(&config, &PRODUCT).await });
        let waiting = waiting_on(&mut watcher, holder_pid, &job).await;
        if let Some(job_pid) = waiting {
            assert_eq!(
                json_of(&mut watcher, LOCKS_ON_MESSAGES, job_pid).await,
                serde_json::json!([]),
                "the Job holds no lock on assistant.messages while it waits for assistant.runs"
            );
        }
        sqlx::raw_sql("ROLLBACK")
            .execute(&mut holder)
            .await
            .expect("end the writer's transaction");
        holder.close().await.expect("close");
        match job.await.expect("the Job's task") {
            Ok(_) => {
                assert!(
                    waiting.is_some(),
                    "the Job applied 0006 without waiting for the runs"
                );
                break;
            }
            // Ended before the test saw it waiting, or waited past the file's lock timeout:
            // the Job is run again, as a deployment retries it.
            Err(error) => println!("attempt {attempt}: the Job failed ({error}); again"),
        }
        assert!(
            attempt < ATTEMPTS,
            "the Job did not apply 0006 in {ATTEMPTS} attempts"
        );
    }

    let applied: String = sqlx::query_scalar(APPLIED)
        .fetch_one(&mut watcher)
        .await
        .expect("the applied files");
    assert!(
        applied.contains("0006_answer_removed_content.sql"),
        "{applied}"
    );
    let migrated: bool = sqlx::query_scalar(MIGRATED)
        .fetch_one(&mut watcher)
        .await
        .expect("read the catalog");
    assert!(migrated, "the index and the column are there");
    watcher.close().await.expect("close");
    std::fs::remove_dir_all(&dir).expect("remove the CA file's directory");
    println!("0006 waited for the runs table holding no lock on the messages");
}
