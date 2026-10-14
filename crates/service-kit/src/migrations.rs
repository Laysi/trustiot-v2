//! What a service's image expects of its Postgres schema, and the readiness check that compares
//! it with what the migration Job applied (Laysi/trustiot-v2#402; docs/architecture/deployment.md,
//! "migration Job").
//!
//! The chart runs a service only after the migration Job of the same release has completed, but
//! that guarantee lives in the chart: a service patched by digest, a hand-edited Deployment, a
//! deployment outside the chart or a turned-off Job could start new code on an old schema. So a
//! service checks for itself, and stays unready until the database has every file it expects.
//!
//! - [`Schema`]: the files a service expects, compiled in. Each service keeps it in its own
//!   `src/schema.rs`, which the migration Job compiles too (`tools/migrate/src/registry.rs`
//!   includes the same file), so the Job applies exactly the list the service checks.
//! - The Job records every file it applies in a service's schema twice, in the same transaction:
//!   in its own `trustiot_migrate.applied`, which no service role can read, and in the schema's
//!   [`RECORDS_TABLE`] ([`RECORDS_TEMPLATE`]), which the service role may only `SELECT`.
//! - [`MigrationCheck`]: the readiness check. It fails while a file the image expects is missing
//!   or was applied with another SHA-256, and logs each such file and what to do (run the
//!   migration Job of this release). Files the database has and the image does not know (the
//!   image was rolled back) do not fail it: migrations are forward compatible only, so the older
//!   code runs on the newer schema. Once every file is there the result is kept for the life of
//!   the process, since the Job only ever adds records.

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use sha2::{Digest, Sha256};
use sqlx::PgPool;

use crate::health::Health;

/// One SQL file, compiled in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Migration {
    /// File name, for example `0001_identity.sql`; files apply in this order.
    pub name: &'static str,
    /// The file's text.
    pub sql: &'static str,
}

impl Migration {
    /// The SHA-256 of the file's text as 64 lower-case hex digits, as the Job records it.
    #[must_use]
    pub fn sha256(&self) -> String {
        sha256_hex(self.sql)
    }
}

/// The SHA-256 of `text` as 64 lower-case hex digits.
#[must_use]
pub fn sha256_hex(text: &str) -> String {
    use std::fmt::Write as _;
    let digest = Sha256::digest(text.as_bytes());
    let mut hex = String::with_capacity(64);
    for byte in digest {
        // Writing to a String cannot fail.
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// sql/database.sql: run once per database before any service connects.
pub const DATABASE: Migration = Migration {
    name: "database.sql",
    sql: include_str!("../sql/database.sql"),
};

/// The outbox policy template (sql/outbox-event.sql), applied once per producer with its roles.
pub const OUTBOX_EVENT: Migration = Migration {
    name: "outbox-event.sql",
    sql: include_str!("../sql/outbox-event.sql"),
};

/// The tenant column, row-level security and grants of the outbox's `outbox_aggregate`
/// (sql/outbox-aggregate.sql, Laysi/trustiot-v2#413), applied once per producer after
/// [`OUTBOX_EVENT`].
pub const OUTBOX_AGGREGATE: Migration = Migration {
    name: "outbox-aggregate.sql",
    sql: include_str!("../sql/outbox-aggregate.sql"),
};

/// The outbox templates an outbox producer's schema gets, in apply order.
pub const OUTBOX_PRODUCER_FILES: [Migration; 2] = [OUTBOX_EVENT, OUTBOX_AGGREGATE];

/// The table in each service's schema that holds its migration records.
pub const RECORDS_TABLE: &str = "trustiot_migrate_applied";

/// sql/migration-records.sql: creates [`RECORDS_TABLE`] in `example_schema` and lets
/// `example_app` read it. The Job runs it as the owner role on every run.
pub const RECORDS_TEMPLATE: &str = include_str!("../sql/migration-records.sql");

/// The records the service role can read. Unqualified: the Job sets every service role's
/// `search_path` to its own schema, so this is the service's own [`RECORDS_TABLE`].
const READ_RECORDS: &str = "SELECT component, name, sha256 FROM trustiot_migrate_applied";

/// SQLSTATE `undefined_table`: the records table is not there (no Job of this release has run).
const UNDEFINED_TABLE: &str = "42P01";
/// SQLSTATE `insufficient_privilege`: the role may not read it.
const INSUFFICIENT_PRIVILEGE: &str = "42501";

/// The component the Job records a service's outbox templates under, for example
/// `identity.outbox`. Its migrations/postgres files are recorded under the service's name.
#[must_use]
pub fn outbox_component(service: &str) -> String {
    format!("{service}.outbox")
}

/// What a service's image expects of its schema. Declare it once, as `pub const SCHEMA` in the
/// service's `src/schema.rs`, and include that file in `tools/migrate/src/registry.rs`: the Job
/// applies the same files from the same constant.
#[derive(Clone, Copy, Debug)]
pub struct Schema {
    /// The chart's service key, for example `identity` or `telemetry-query`; the Job's
    /// component for the service's own files.
    pub service: &'static str,
    /// The service writes outbox events, so its schema gets [`OUTBOX_PRODUCER_FILES`].
    pub outbox_producer: bool,
    /// Its `migrations/postgres/` files in apply order.
    pub migrations: &'static [Migration],
}

impl Schema {
    /// Every record the Job writes for this schema, in apply order: the outbox templates of a
    /// producer, then the service's files.
    #[must_use]
    pub fn expected(&self) -> Vec<Record> {
        let outbox = self.outbox_producer.then(|| outbox_component(self.service));
        let mut records = Vec::new();
        if let Some(component) = &outbox {
            records.extend(
                OUTBOX_PRODUCER_FILES
                    .iter()
                    .map(|file| Record::of(component, file)),
            );
        }
        records.extend(
            self.migrations
                .iter()
                .map(|file| Record::of(self.service, file)),
        );
        records
    }
}

/// One applied file: the component it belongs to, its name and its SHA-256.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Record {
    /// `<service>` or `<service>.outbox`.
    pub component: String,
    /// The file name.
    pub name: String,
    /// 64 lower-case hex digits.
    pub sha256: String,
}

impl Record {
    fn of(component: &str, file: &Migration) -> Self {
        Self {
            component: component.to_owned(),
            name: file.name.to_owned(),
            sha256: file.sha256(),
        }
    }
}

impl fmt::Display for Record {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.component, self.name)
    }
}

/// How the database's records compare with the files an image expects.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Comparison {
    /// Expected files the database has no record of.
    pub missing: Vec<Record>,
    /// Expected files the database applied with another SHA-256 (the record's hash is
    /// `found_sha256`).
    pub changed: Vec<Changed>,
    /// Records of files the image does not expect: the database is newer than the image.
    pub newer: Vec<Record>,
}

/// An expected file that the database applied with another SHA-256.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Changed {
    /// The file as the image has it.
    pub expected: Record,
    /// The SHA-256 the database recorded.
    pub found_sha256: String,
}

impl Comparison {
    /// Compares `found` (the database's records) with `expected`.
    #[must_use]
    pub fn of(expected: &[Record], found: &[Record]) -> Self {
        let mut comparison = Self::default();
        for file in expected {
            match found
                .iter()
                .find(|record| record.component == file.component && record.name == file.name)
            {
                None => comparison.missing.push(file.clone()),
                Some(record) if record.sha256 != file.sha256 => {
                    comparison.changed.push(Changed {
                        expected: file.clone(),
                        found_sha256: record.sha256.clone(),
                    });
                }
                Some(_) => {}
            }
        }
        comparison.newer = found
            .iter()
            .filter(|record| {
                !expected
                    .iter()
                    .any(|file| file.component == record.component && file.name == record.name)
            })
            .cloned()
            .collect();
        comparison.newer.sort();
        comparison
    }

    /// True when the database has every expected file, as the image has it.
    #[must_use]
    pub fn complete(&self) -> bool {
        self.missing.is_empty() && self.changed.is_empty()
    }
}

/// Why the migration check failed. The text names files, never data, and goes to the log only:
/// `/readyz` answers `unavailable` without it.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum MigrationCheckError {
    /// The database lacks files the image expects, or applied them with another SHA-256.
    #[error("{0}")]
    Behind(Behind),
    /// The records could not be read (the database is down, or no session passed the check).
    #[error("could not read the migration records: {0}")]
    Database(sqlx::Error),
}

/// The files the database lacks or applied differently, for [`MigrationCheckError::Behind`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Behind {
    /// The service.
    pub service: &'static str,
    /// No records were readable: the records table is missing or this role may not read it.
    pub no_records: bool,
    /// The comparison with what was readable.
    pub comparison: Comparison,
}

impl Behind {
    fn missing(&self) -> String {
        join(self.comparison.missing.iter().map(ToString::to_string))
    }

    fn changed(&self) -> String {
        join(self.comparison.changed.iter().map(|changed| {
            format!(
                "{} (image {}, database {})",
                changed.expected, changed.expected.sha256, changed.found_sha256
            )
        }))
    }
}

fn join(items: impl Iterator<Item = String>) -> String {
    let items: Vec<String> = items.collect();
    if items.is_empty() {
        "none".to_owned()
    } else {
        items.join(", ")
    }
}

impl fmt::Display for Behind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "the database lacks migrations the {} image expects",
            self.service
        )?;
        if self.no_records {
            write!(
                f,
                " (no migration records are readable: table {RECORDS_TABLE} is missing or not \
                 granted to this role)"
            )?;
        }
        write!(
            f,
            "; missing: {}; applied with another SHA-256: {}; run the migration Job of this \
             release (tools/migrate, docs/runbooks/migrations.md)",
            self.missing(),
            self.changed()
        )
    }
}

/// The readiness check of a service's migrations. Cheap to clone; clones share the result.
///
/// ```no_run
/// use service_kit::migrations::{Migration, MigrationCheck, Schema};
///
/// const SCHEMA: Schema = Schema {
///     service: "assets",
///     outbox_producer: false,
///     migrations: &[Migration { name: "0001_assets.sql", sql: "CREATE TABLE ..." }],
/// };
///
/// fn register(health: &service_kit::health::Health, pool: sqlx::PgPool) {
///     MigrationCheck::new(&SCHEMA, pool).register(health);
/// }
/// ```
#[derive(Clone, Debug)]
pub struct MigrationCheck {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    service: &'static str,
    pool: PgPool,
    expected: Vec<Record>,
    /// Set once every expected file was found; never cleared.
    passed: AtomicBool,
    /// The last failure logged, so a failure is logged once until it changes.
    logged: Mutex<Option<Behind>>,
}

impl MigrationCheck {
    /// A check of `schema` that reads the records on `pool`, a pool of the service role (for
    /// example `TenantDb::pool`). Nothing runs until [`Self::check`].
    #[must_use]
    pub fn new(schema: &Schema, pool: PgPool) -> Self {
        Self {
            inner: Arc::new(Inner {
                service: schema.service,
                pool,
                expected: schema.expected(),
                passed: AtomicBool::new(false),
                logged: Mutex::new(None),
            }),
        }
    }

    /// Adds the check to readiness as `migrations`.
    pub fn register(&self, health: &Health) {
        let check = self.clone();
        health.add_check("migrations", move || {
            let check = check.clone();
            async move { check.check().await }
        });
    }

    /// Compares the database's records with the files the image expects. Once they match, later
    /// calls return at once without a query: the Job only adds records, so a database that had
    /// every file keeps having it (restoring an older backup under running services needs a
    /// restart, as docs/runbooks/migrations.md says).
    ///
    /// # Errors
    ///
    /// [`MigrationCheckError::Behind`] while a file is missing or changed (logged at ERROR, each
    /// file named, once until the failure changes); [`MigrationCheckError::Database`] when the
    /// records could not be read.
    pub async fn check(&self) -> Result<(), MigrationCheckError> {
        if self.inner.passed.load(Ordering::Acquire) {
            return Ok(());
        }
        let read: Result<Vec<(String, String, String)>, sqlx::Error> = sqlx::query_as(READ_RECORDS)
            .fetch_all(&self.inner.pool)
            .await;
        let (found, no_records) = match read {
            Ok(rows) => (
                rows.into_iter()
                    .map(|(component, name, sha256)| Record {
                        component,
                        name,
                        sha256,
                    })
                    .collect(),
                false,
            ),
            Err(sqlx::Error::Database(error))
                if matches!(
                    error.code().as_deref(),
                    Some(UNDEFINED_TABLE | INSUFFICIENT_PRIVILEGE)
                ) =>
            {
                (Vec::new(), true)
            }
            Err(error) => return Err(MigrationCheckError::Database(error)),
        };
        self.judge(&found, no_records)
    }

    /// The outcome for `found`, logged.
    fn judge(&self, found: &[Record], no_records: bool) -> Result<(), MigrationCheckError> {
        let comparison = Comparison::of(&self.inner.expected, found);
        if comparison.complete() {
            if !self.inner.passed.swap(true, Ordering::AcqRel) {
                tracing::info!(
                    service = self.inner.service,
                    expected = self.inner.expected.len(),
                    newer = %join(comparison.newer.iter().map(ToString::to_string)),
                    "the database has every migration this image expects (newer ones are \
                     forward compatible)"
                );
            }
            return Ok(());
        }
        let behind = Behind {
            service: self.inner.service,
            no_records,
            comparison,
        };
        let changed = match self.inner.logged.lock() {
            Ok(mut logged) => {
                let changed = logged.as_ref() != Some(&behind);
                *logged = Some(behind.clone());
                changed
            }
            Err(_) => true,
        };
        if changed {
            tracing::error!(
                service = self.inner.service,
                missing = %behind.missing(),
                changed = %behind.changed(),
                no_records = behind.no_records,
                "the database lacks migrations this image expects, so the service stays unready; \
                 run the migration Job of this release (tools/migrate, docs/runbooks/migrations.md)"
            );
        }
        Err(MigrationCheckError::Behind(behind))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::test_support::{capture_subscriber, lines};

    const FIRST: Migration = Migration {
        name: "0001_first.sql",
        sql: "CREATE TABLE first (id int);",
    };
    const SECOND: Migration = Migration {
        name: "0002_second.sql",
        sql: "CREATE TABLE second (id int);",
    };
    const SCHEMA: Schema = Schema {
        service: "example",
        outbox_producer: false,
        migrations: &[FIRST, SECOND],
    };

    fn record(component: &str, file: &Migration) -> Record {
        Record::of(component, file)
    }

    fn check() -> MigrationCheck {
        let options = sqlx::postgres::PgConnectOptions::new().host("db.invalid");
        let pool = sqlx::postgres::PgPoolOptions::new().connect_lazy_with(options);
        MigrationCheck::new(&SCHEMA, pool)
    }

    #[test]
    fn hashes_are_lower_case_hex() {
        assert_eq!(
            sha256_hex("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(FIRST.sha256(), sha256_hex(FIRST.sql));
    }

    #[test]
    fn a_producer_expects_the_outbox_templates_first() {
        let producer = Schema {
            service: "telemetry-query",
            outbox_producer: true,
            migrations: &[FIRST],
        };
        let expected: Vec<String> = producer
            .expected()
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(
            expected,
            [
                "telemetry-query.outbox/outbox-event.sql",
                "telemetry-query.outbox/outbox-aggregate.sql",
                "telemetry-query/0001_first.sql",
            ]
        );
        let names: Vec<String> = SCHEMA.expected().iter().map(ToString::to_string).collect();
        assert_eq!(names, ["example/0001_first.sql", "example/0002_second.sql"]);
    }

    #[test]
    fn comparison_finds_missing_changed_and_newer_files() {
        let expected = SCHEMA.expected();
        assert!(Comparison::of(&expected, &expected).complete());

        let only_first = [record("example", &FIRST)];
        let comparison = Comparison::of(&expected, &only_first);
        assert_eq!(comparison.missing, [record("example", &SECOND)]);
        assert!(comparison.changed.is_empty());
        assert!(!comparison.complete());

        let mut edited = expected.clone();
        if let Some(second) = edited.get_mut(1) {
            second.sha256 = sha256_hex("something else");
        }
        let comparison = Comparison::of(&expected, &edited);
        assert!(comparison.missing.is_empty());
        assert_eq!(comparison.changed.len(), 1);
        assert!(!comparison.complete());

        // A database newer than the image (a rollback) is complete: forward compatible.
        let mut newer = expected.clone();
        newer.push(Record {
            component: "example".to_owned(),
            name: "0003_third.sql".to_owned(),
            sha256: sha256_hex("third"),
        });
        newer.push(Record {
            component: "example.outbox".to_owned(),
            name: "outbox-event.sql".to_owned(),
            sha256: OUTBOX_EVENT.sha256(),
        });
        let comparison = Comparison::of(&expected, &newer);
        assert!(comparison.complete());
        assert_eq!(comparison.newer.len(), 2);

        // The same file name under another component is another file.
        let elsewhere = [record("other", &FIRST), record("other", &SECOND)];
        assert_eq!(Comparison::of(&expected, &elsewhere).missing.len(), 2);
    }

    #[test]
    fn the_message_names_each_file_and_the_remedy() {
        let behind = Behind {
            service: "example",
            no_records: true,
            comparison: Comparison::of(&SCHEMA.expected(), &[]),
        };
        let text = behind.to_string();
        for part in [
            "the example image expects",
            "table trustiot_migrate_applied is missing",
            "missing: example/0001_first.sql, example/0002_second.sql",
            "run the migration Job of this release",
        ] {
            assert!(text.contains(part), "{part:?} in {text}");
        }
        let mut edited = SCHEMA.expected();
        if let Some(first) = edited.get_mut(0) {
            first.sha256 = "0".repeat(64);
        }
        let text = Behind {
            service: "example",
            no_records: false,
            comparison: Comparison::of(&SCHEMA.expected(), &edited),
        }
        .to_string();
        assert!(text.contains("missing: none"), "{text}");
        assert!(
            text.contains(&format!(
                "applied with another SHA-256: example/0001_first.sql (image {}, database {})",
                FIRST.sha256(),
                "0".repeat(64)
            )),
            "{text}"
        );
        assert!(!text.contains("not granted"), "{text}");
    }

    // A Tokio runtime only because the lazy pool needs one; `judge` runs no query.
    #[tokio::test]
    async fn a_failure_is_logged_once_with_each_file_and_a_success_is_kept() {
        let (subscriber, capture, _provider) = capture_subscriber("info");
        let check = check();
        tracing::subscriber::with_default(subscriber, || {
            let partial = [record("example", &FIRST)];
            for _ in 0..3 {
                let error = check
                    .judge(&partial, false)
                    .expect_err("one file is missing");
                assert!(
                    error
                        .to_string()
                        .contains("missing: example/0002_second.sql"),
                    "{error}"
                );
            }
            // Another failure is logged again.
            check.judge(&[], true).expect_err("no records");
            check
                .judge(&SCHEMA.expected(), false)
                .expect("every file is there");
            check
                .judge(&SCHEMA.expected(), false)
                .expect("still every file");
        });
        // Only this module's lines: other tests of the binary may log on this thread too.
        let lines: Vec<serde_json::Value> = lines(&capture)
            .into_iter()
            .filter(|line| line["target"] == "service_kit::migrations")
            .collect();
        let errors: Vec<&serde_json::Value> = lines
            .iter()
            .filter(|line| line["level"] == "ERROR")
            .collect();
        assert_eq!(errors.len(), 2, "{lines:#?}");
        assert_eq!(errors[0]["fields"]["missing"], "example/0002_second.sql");
        assert_eq!(errors[0]["fields"]["changed"], "none");
        assert_eq!(
            errors[1]["fields"]["missing"],
            "example/0001_first.sql, example/0002_second.sql"
        );
        assert_eq!(errors[1]["fields"]["no_records"], true);
        let message = errors[0]["message"].as_str().unwrap_or_default();
        assert!(
            message.contains("run the migration Job of this release"),
            "{message}"
        );
        // The success line (INFO) is not counted here: in full runs of this test binary it was
        // missing from the thread-local capture now and then (2026-10-15, 1 run in 4), so the
        // services' tests/migrations.rs check it in the real binary's log instead.
        assert!(check.inner.passed.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn a_passed_check_runs_no_query_again() {
        let check = check();
        check
            .judge(&SCHEMA.expected(), false)
            .expect("every file is there");
        // The pool points at a host that does not exist: a query would fail.
        check.check().await.expect("kept without a query");
    }
}
