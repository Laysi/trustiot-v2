//! The bundled ClickHouse's bootstrap admin password (Laysi/trustiot-v2#403): the init container
//! (`migrate clickhouse-admin-password`) against the fake Kubernetes API of support/secrets.rs,
//! and the migration Job reading the password from the Secret against a throwaway ClickHouse.
//!
//! - The init container fills the empty Secret the chart declares and writes the same password
//!   to the file the server reads, readable by its owner only; a second start changes nothing; a
//!   higher generation rotates it once, a lower one (a rollback) keeps it; a Secret someone else
//!   created is never trusted; a missing Secret or refused access stops it with the way out.
//! - The Job waits until the Secret is filled, uses only a password the init container wrote,
//!   reads the Secret again while ClickHouse refuses the password (a rotation in progress), and
//!   gives up at its bound with a message that names the Secret.
//!
//! Nothing a report, an error or `Debug` shows contains a password. The ClickHouse tests need
//! podman or docker; skipped without one unless `CI` is set.

mod support;

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use migrate::clickhouse::ClickHouse as Client;
use migrate::clickhouse_admin::generate;
use migrate::config::{
    AdminSettings, ClickHouseSettings, Platform, PostgresSettings, RunConfig, WaitSettings,
};
use migrate::passwords::{GENERATION_ANNOTATION, UID_ANNOTATION};
use migrate::{Error, Report};
use service_kit::postgres::SslMode;
use support::secrets::FakeSecrets;
use support::{CLICKHOUSE_ADMIN, ClickHouse, random_hex, scratch_dir};

const ADMIN_SECRET: &str = "trustiot-clickhouse-admin-password";

/// Longer than several retries (every 2 s) on a loaded machine.
const WAIT: Duration = Duration::from_secs(120);
/// How long a test lets a wait run that must give up; the run's own bound, not a timing guess.
const SHORT_WAIT: Duration = Duration::from_secs(6);

async fn fake() -> FakeSecrets {
    let fake = FakeSecrets::start().await;
    fake.declare(ADMIN_SECRET);
    fake
}

fn password_file() -> PathBuf {
    scratch_dir("clickhouse-admin").join("password")
}

fn admin(fake: &FakeSecrets, generation: &str, file: &Path) -> AdminSettings {
    fake.admin_settings(ADMIN_SECRET, CLICKHOUSE_ADMIN, generation, file)
}

fn stored(fake: &FakeSecrets) -> Option<String> {
    fake.value(ADMIN_SECRET, CLICKHOUSE_ADMIN)
}

fn generation_of(fake: &FakeSecrets) -> Option<String> {
    fake.get(ADMIN_SECRET)
        .expect("declared")
        .annotations
        .get(GENERATION_ANNOTATION)
        .cloned()
}

/// Fails when any text contains `value` or its base64.
fn assert_not_in(value: &str, texts: &[String]) {
    for text in texts {
        assert!(
            !text.contains(value) && !text.contains(&STANDARD.encode(value)),
            "a password or its base64 reached a report, an error or Debug output"
        );
    }
}

fn texts(report: &Report) -> Vec<String> {
    let mut texts = report.changes.clone();
    texts.push(format!("{report:?}"));
    texts
}

#[tokio::test]
async fn the_init_container_fills_the_secret_and_the_file_once() {
    let fake = fake().await;
    let file = password_file();
    let mut report = Report::default();
    generate(&admin(&fake, "1", &file), &mut report)
        .await
        .expect("the first start");
    assert_eq!(
        report.changes,
        [format!(
            "generated the password of {CLICKHOUSE_ADMIN} in Secret {ADMIN_SECRET} (it had none)"
        )]
    );
    let value = stored(&fake).expect("filled");
    assert_eq!(value.len(), 64, "256 bits as hex digits");
    assert!(value.bytes().all(|byte| byte.is_ascii_hexdigit()));
    assert_eq!(generation_of(&fake).as_deref(), Some("1"));
    let secret = fake.get(ADMIN_SECRET).expect("declared");
    assert_eq!(secret.annotations.get(UID_ANNOTATION), Some(&secret.uid));
    // The server reads the same password, from a file only its owner may read.
    assert_eq!(
        std::fs::read_to_string(&file).expect("the password file"),
        value
    );
    let mode = std::fs::metadata(&file)
        .expect("the password file")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o400, "mode {mode:o}");
    assert_not_in(&value, &texts(&report));

    // Only GET and PATCH of this one Secret, with the token.
    let seen = fake.seen();
    let path = format!("/api/v1/namespaces/release-ns/secrets/{ADMIN_SECRET}");
    assert_eq!(
        seen.iter()
            .map(|seen| seen.line.clone())
            .collect::<Vec<_>>(),
        [
            format!("GET {path} HTTP/1.1"),
            format!("PATCH {path}?fieldManager=trustiot-migrate HTTP/1.1"),
        ]
    );
    assert!(seen.iter().all(|seen| seen.authorized));

    // A restart of the pod: the same password, read and written again, nothing patched. A file
    // left half-written by an interrupted start is replaced too.
    let leftover = file.with_file_name(".password.new");
    std::fs::write(&leftover, "half").expect("a leftover file");
    std::fs::set_permissions(&leftover, std::fs::Permissions::from_mode(0o400))
        .expect("read-only, as the init container leaves it");
    let requests = fake.seen().len();
    let mut report = Report::default();
    generate(&admin(&fake, "1", &file), &mut report)
        .await
        .expect("the second start");
    assert!(report.changes.is_empty(), "{:?}", report.changes);
    assert_eq!(fake.seen().len(), requests + 1, "one GET and no PATCH");
    assert_eq!(stored(&fake).as_deref(), Some(value.as_str()));
    assert_eq!(
        std::fs::read_to_string(&file).expect("the password file"),
        value
    );
    assert!(!leftover.exists());
}

#[tokio::test]
async fn a_new_generation_rotates_once_and_a_rollback_keeps_it() {
    let fake = fake().await;
    let file = password_file();
    generate(&admin(&fake, "1", &file), &mut Report::default())
        .await
        .expect("the first start");
    let first = stored(&fake).expect("filled");
    let mut report = Report::default();
    generate(&admin(&fake, "2", &file), &mut report)
        .await
        .expect("the start after adminPasswordGeneration went up");
    assert_eq!(
        report.changes,
        [format!(
            "generated the password of {CLICKHOUSE_ADMIN} in Secret {ADMIN_SECRET} (password generation 2)"
        )]
    );
    let second = stored(&fake).expect("filled");
    assert_ne!(first, second);
    assert_eq!(
        std::fs::read_to_string(&file).expect("the password file"),
        second
    );
    // The same generation again, a rollback to the older one, and the re-upgrade: no change.
    for requested in ["2", "1", "2"] {
        let mut report = Report::default();
        generate(&admin(&fake, requested, &file), &mut report)
            .await
            .expect("a restart, a rollback or a re-upgrade");
        assert!(
            report.changes.is_empty(),
            "generation {requested}: {:?}",
            report.changes
        );
        assert_eq!(stored(&fake).as_deref(), Some(second.as_str()));
        assert_eq!(generation_of(&fake).as_deref(), Some("2"));
        assert_eq!(
            std::fs::read_to_string(&file).expect("the password file"),
            second
        );
    }
}

#[tokio::test]
async fn a_secret_someone_else_created_is_never_trusted() {
    // Someone creates the Secret before the install, with a password of their choosing and the
    // annotations the init container writes; Helm adopts it. Its uid is not the one recorded.
    let fake = FakeSecrets::start().await;
    fake.declare(ADMIN_SECRET);
    let chosen = "c".repeat(64);
    fake.edit(ADMIN_SECRET, |secret| {
        secret
            .data
            .insert(CLICKHOUSE_ADMIN.to_owned(), chosen.as_bytes().to_vec());
        secret
            .annotations
            .insert(GENERATION_ANNOTATION.to_owned(), "1".to_owned());
        secret
            .annotations
            .insert(UID_ANNOTATION.to_owned(), "someone-elses-uid".to_owned());
    });
    let file = password_file();
    let mut report = Report::default();
    generate(&admin(&fake, "1", &file), &mut report)
        .await
        .expect("the first start");
    assert_eq!(
        report.changes,
        [format!(
            "generated the password of {CLICKHOUSE_ADMIN} in Secret {ADMIN_SECRET} (the Secret is not the one the Job filled)"
        )]
    );
    let value = stored(&fake).expect("filled");
    assert_ne!(value, chosen);
    assert_eq!(
        std::fs::read_to_string(&file).expect("the password file"),
        value
    );
    assert_not_in(&chosen, &texts(&report));
}

#[tokio::test]
async fn a_missing_secret_or_refused_access_stops_it_with_the_way_out() {
    let fake = fake().await;
    let file = password_file();
    fake.delete(ADMIN_SECRET);
    let error = generate(&admin(&fake, "1", &file), &mut Report::default())
        .await
        .expect_err("a missing Secret");
    let text = error.to_string();
    assert!(matches!(error, Error::Secret { .. }), "{text}");
    for part in [
        ADMIN_SECRET,
        "does not exist",
        "helm upgrade",
        "init container",
    ] {
        assert!(text.contains(part), "{part:?} missing from: {text}");
    }
    assert!(!file.exists(), "no password file without a password");

    fake.declare(ADMIN_SECRET);
    fake.refuse_with(Some(403));
    let error = generate(&admin(&fake, "1", &file), &mut Report::default())
        .await
        .expect_err("refused");
    let text = error.to_string();
    assert!(
        text.contains("refused to get it (HTTP 403)")
            && text
                .contains("bundled ClickHouse's ServiceAccount get and patch exactly this Secret"),
        "{text}"
    );
    assert!(!file.exists());
}

/// Run settings that reach ClickHouse at `url` with the admin password from `ADMIN_SECRET` in
/// `fake`. Postgres is never used by `ClickHouse::connect`.
fn job_config(fake: &FakeSecrets, url: &str) -> (RunConfig, ClickHouseSettings) {
    let mut passwords = fake.settings("1", &[], None);
    passwords.postgres = None;
    let settings = ClickHouseSettings {
        username: CLICKHOUSE_ADMIN.to_owned(),
        password: None,
        password_secret: Some(ADMIN_SECRET.to_owned()),
        raw_retention_days: None,
        aggregate_1m_retention_months: None,
        aggregate_1h_retention_months: None,
    };
    let config = RunConfig {
        platform: Platform {
            postgres_host: "postgres.invalid".to_owned(),
            postgres_port: 5432,
            postgres_database: "unused".to_owned(),
            postgres_sslmode: SslMode::VerifyFull,
            postgres_ca_file: PathBuf::from("/unused/ca.crt"),
            clickhouse_url: Some(url.to_owned()),
            clickhouse_ca_file: None,
        },
        postgres: PostgresSettings {
            username: "unused".to_owned(),
            password: "unused".to_owned(),
        },
        clickhouse: None,
        passwords: Some(passwords),
        wait: WaitSettings::default(),
    };
    (config, settings)
}

/// GET requests of the admin Secret so far.
fn reads(fake: &FakeSecrets) -> usize {
    fake.seen()
        .iter()
        .filter(|seen| {
            seen.line.starts_with(&format!(
                "GET /api/v1/namespaces/release-ns/secrets/{ADMIN_SECRET} "
            ))
        })
        .count()
}

/// Waits until the fake has answered `count` reads of the admin Secret.
async fn until_reads(fake: &FakeSecrets, count: usize) {
    let started = Instant::now();
    while reads(fake) < count {
        assert!(
            started.elapsed() < WAIT,
            "the Job did not read the admin Secret {count} times within {WAIT:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_job_uses_only_the_password_the_init_container_wrote_and_rereads_it_when_refused() {
    let fake = fake().await;
    let file = password_file();
    // The init container runs first; the server starts with the password it wrote.
    generate(&admin(&fake, "1", &file), &mut Report::default())
        .await
        .expect("the init container");
    let first = std::fs::read_to_string(&file).expect("the password file");
    let Some(clickhouse) = ClickHouse::start_with_password(first.clone()).await else {
        return;
    };
    let (config, settings) = job_config(&fake, &clickhouse.url);
    Client::connect(&config, &settings, WAIT)
        .await
        .expect("the Job logs in with the password from the Secret");

    // A rotation in progress: the Secret already holds the next password, the server still
    // runs with the old one. The Job keeps reading the Secret and does not give up...
    generate(&admin(&fake, "2", &file), &mut Report::default())
        .await
        .expect("the init container of the restarting pod");
    let second = stored(&fake).expect("rotated");
    assert_ne!(second, first);
    let before = reads(&fake);
    let job = {
        let (config, settings) = (job_config(&fake, &clickhouse.url).0, settings);
        tokio::spawn(async move { Client::connect(&config, &settings, WAIT).await.map(|_| ()) })
    };
    until_reads(&fake, before + 2).await;
    assert!(
        !job.is_finished(),
        "the Job stopped while ClickHouse refused the password"
    );
    // ...until the Secret and the server agree (here the server keeps its password, which the
    // test puts back into the bound Secret; on a cluster the server restarts with the new one).
    fake.write(ADMIN_SECRET, CLICKHOUSE_ADMIN, Some(first.as_bytes()));
    job.await
        .expect("the Job's task")
        .expect("the Job logs in once the Secret holds the server's password");

    // A password the server never accepts: the Job gives up at its bound, naming the Secret.
    fake.write(ADMIN_SECRET, CLICKHOUSE_ADMIN, Some(second.as_bytes()));
    let (config, settings) = job_config(&fake, &clickhouse.url);
    let error = Client::connect(&config, &settings, SHORT_WAIT)
        .await
        .err()
        .expect("never accepted");
    let text = error.to_string();
    for part in [ADMIN_SECRET, "did not accept", "restart"] {
        assert!(text.contains(part), "{part:?} missing from: {text}");
    }
    assert_not_in(&first, &[text.clone(), format!("{error:?}")]);
    assert_not_in(&second, &[text, format!("{error:?}")]);

    // The server's password in an object someone else created under the Secret's name: the Job
    // never uses it, and waits for the init container instead.
    fake.write(ADMIN_SECRET, CLICKHOUSE_ADMIN, Some(first.as_bytes()));
    fake.recreate(ADMIN_SECRET);
    let error = Client::connect(&config, &settings, SHORT_WAIT)
        .await
        .err()
        .expect("an unbound Secret is not used");
    let text = error.to_string();
    assert!(
        matches!(&error, Error::Secret { secret, problem } if secret == ADMIN_SECRET && problem.contains("that the bundled ClickHouse generated")),
        "{text}"
    );
    assert_not_in(&first, &[text]);
}

#[tokio::test(flavor = "multi_thread")]
async fn on_a_fresh_install_the_job_waits_for_the_init_container() {
    let fake = fake().await;
    let Some(clickhouse) = ClickHouse::start_with_password(random_hex()).await else {
        return;
    };
    // The Job gets there first: the Secret is still as the chart declared it, empty.
    let (config, settings) = job_config(&fake, &clickhouse.url);
    let job =
        tokio::spawn(async move { Client::connect(&config, &settings, WAIT).await.map(|_| ()) });
    until_reads(&fake, 2).await;
    assert!(
        !job.is_finished(),
        "the Job stopped before the Secret was filled"
    );
    // The init container's write: the server's password, bound to the Secret's uid in the same
    // patch (as `generate` writes it; the test puts in the password the server started with).
    fake.edit(ADMIN_SECRET, |secret| {
        secret.data.insert(
            CLICKHOUSE_ADMIN.to_owned(),
            clickhouse.admin_password.as_bytes().to_vec(),
        );
        secret
            .annotations
            .insert(UID_ANNOTATION.to_owned(), secret.uid.clone());
        secret
            .annotations
            .insert(GENERATION_ANNOTATION.to_owned(), "1".to_owned());
    });
    job.await
        .expect("the Job's task")
        .expect("the Job logs in once the Secret is filled");
}
