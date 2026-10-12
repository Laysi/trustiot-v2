//! The product registry lists every migration file, in order, and the files follow the rules
//! the Job relies on. No database needed.

use std::fs;
use std::path::{Path, PathBuf};

use migrate::postgres::check_registry;
use migrate::registry::{Migration, Outbox, PRODUCT, Registry, Service, TenantRoleHook};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The `.sql` files of `dir`, in file name order.
fn files(dir: &Path) -> Vec<(String, String)> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap_or_else(|error| panic!("read {}: {error}", dir.display()))
        .map(|entry| {
            entry
                .expect("directory entry")
                .file_name()
                .into_string()
                .expect("UTF-8 name")
        })
        .filter(|name| name.ends_with(".sql"))
        .collect();
    names.sort();
    names
        .into_iter()
        .map(|name| {
            let text = fs::read_to_string(dir.join(&name)).expect("read a migration file");
            (name, text)
        })
        .collect()
}

fn assert_lists(what: &str, dir: &Path, listed: &[Migration]) {
    let on_disk = files(dir);
    let listed_names: Vec<&str> = listed.iter().map(|m| m.name).collect();
    let disk_names: Vec<&str> = on_disk.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(
        listed_names,
        disk_names,
        "{what}: tools/migrate/src/registry.rs must list every file of {} in file name order",
        dir.display()
    );
    for (migration, (_, text)) in listed.iter().zip(&on_disk) {
        assert_eq!(
            migration.sql, text,
            "{what}/{}: the compiled text differs",
            migration.name
        );
    }
}

/// Every service directory with `migrations/postgres` is in the registry with all its files;
/// the ClickHouse list is `services/ingest/migrations/clickhouse`.
#[test]
fn the_product_registry_lists_every_file_in_order() {
    let services = repo_root().join("services");
    let mut with_postgres = Vec::new();
    for entry in fs::read_dir(&services).expect("read services/") {
        let dir = entry.expect("directory entry").path();
        if dir.join("migrations/postgres").is_dir() {
            with_postgres.push(
                dir.file_name()
                    .expect("name")
                    .to_string_lossy()
                    .into_owned(),
            );
        }
    }
    with_postgres.sort();
    let mut registered: Vec<String> = PRODUCT.services.iter().map(|s| s.name.to_owned()).collect();
    registered.sort();
    assert_eq!(
        registered, with_postgres,
        "every service with migrations/postgres is registered"
    );
    for service in PRODUCT.services {
        let dir = services.join(service.name).join("migrations/postgres");
        assert_lists(service.name, &dir, service.migrations);
    }
    assert_lists(
        "clickhouse",
        &services.join("ingest/migrations/clickhouse"),
        PRODUCT.clickhouse,
    );
    check_registry(&PRODUCT).expect("the product registry is consistent");
}

/// Statements a service's Postgres migration must not contain: the Job runs each file in one
/// transaction as the owner role, inside the schema it created for the service. The Job also
/// stops a file that left its transaction or its role (tests/file_escapes.rs); this catches the
/// statements in review, before a file ships.
const FORBIDDEN: [&str; 23] = [
    "CREATE SCHEMA",
    "BEGIN;",
    "COMMIT;",
    "ROLLBACK;",
    "CONCURRENTLY",
    "CREATE ROLE",
    "ALTER ROLE",
    "SET ROLE",
    "SET LOCAL ROLE",
    "SET SESSION ROLE",
    "RESET ROLE",
    "CREATE USER",
    "ALTER USER",
    "CREATE GROUP",
    "ALTER GROUP",
    "COMMIT AND",
    "START TRANSACTION",
    "ABORT;",
    "RESET ALL",
    "SESSION AUTHORIZATION",
    "SET_CONFIG",
    "PG_SETTINGS",
    "SECURITY DEFINER",
];

/// The first forbidden statement in `sql`. Case does not matter, lines that start with `--` are
/// skipped, and whitespace runs (spaces, tabs, line breaks) count as one space, so "SET\n  ROLE"
/// or "SECURITY  DEFINER" cannot slip past; a space before ";" is dropped.
fn forbidden_in(sql: &str) -> Option<&'static str> {
    let upper = sql.to_ascii_uppercase();
    let code: String = upper
        .lines()
        .filter(|line| !line.trim_start().starts_with("--"))
        .collect::<Vec<_>>()
        .join("\n")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace(" ;", ";");
    FORBIDDEN
        .into_iter()
        .find(|forbidden| code.contains(forbidden))
}

#[test]
fn postgres_migrations_follow_the_jobs_rules() {
    for service in PRODUCT.services {
        for migration in service.migrations {
            if let Some(forbidden) = forbidden_in(migration.sql) {
                panic!(
                    "{}/{}: {forbidden} is not allowed in a service migration (tools/migrate/README.md)",
                    service.name, migration.name
                );
            }
        }
    }
}

/// The lint is not fooled by case, line breaks, tabs or a space before `;`.
#[test]
fn the_lint_sees_through_whitespace_and_case() {
    for (sql, expected) in [
        ("set\n\tlocal   role identity_owner;", "SET LOCAL ROLE"),
        ("Set Session\nRole x;", "SET SESSION ROLE"),
        ("SELECT 1;\ncommit ;", "COMMIT;"),
        ("commit\nand chain;", "COMMIT AND"),
        ("start\ttransaction;", "START TRANSACTION"),
        ("abort ;", "ABORT;"),
        (
            "CREATE FUNCTION f() RETURNS int LANGUAGE sql SECURITY\n  DEFINER AS 'SELECT 1';",
            "SECURITY DEFINER",
        ),
        ("create\nuser x;", "CREATE USER"),
        (
            "SELECT pg_catalog.set_config('role', 'x', true);",
            "SET_CONFIG",
        ),
    ] {
        assert_eq!(forbidden_in(sql), Some(expected), "{sql:?}");
    }
    for sql in [
        "-- SET ROLE in a comment line is fine\nCREATE TABLE t (i int);",
        "CREATE FUNCTION f() RETURNS int LANGUAGE plpgsql AS $$ BEGIN RETURN 1; END; $$;",
    ] {
        assert_eq!(forbidden_in(sql), None, "{sql:?}");
    }
}

const NONE: &[Migration] = &[];

fn service(name: &'static str, platform_role: bool, outbox: Outbox) -> Service {
    Service {
        name,
        platform_role,
        outbox,
        migrations: NONE,
    }
}

#[test]
fn inconsistent_registries_are_refused() {
    let cases: Vec<(&str, Vec<Service>, Vec<TenantRoleHook>)> = vec![
        (
            "a producer without a platform role",
            vec![
                service(
                    "p",
                    false,
                    Outbox {
                        producer: true,
                        consumes: &[],
                    },
                ),
                service(
                    "c",
                    true,
                    Outbox {
                        producer: false,
                        consumes: &["p"],
                    },
                ),
            ],
            vec![],
        ),
        (
            "a producer without consumers",
            vec![service(
                "p",
                true,
                Outbox {
                    producer: true,
                    consumes: &[],
                },
            )],
            vec![],
        ),
        (
            "a consumer before its producer",
            vec![
                service(
                    "c",
                    true,
                    Outbox {
                        producer: false,
                        consumes: &["p"],
                    },
                ),
                service(
                    "p",
                    true,
                    Outbox {
                        producer: true,
                        consumes: &[],
                    },
                ),
            ],
            vec![],
        ),
        (
            "a service listed twice",
            vec![
                service("a", false, Outbox::default()),
                service("a", false, Outbox::default()),
            ],
            vec![],
        ),
        (
            "a name that makes no plain role name",
            vec![service("Assets", false, Outbox::default())],
            vec![],
        ),
        (
            "a hook of an unknown service",
            vec![service("a", false, Outbox::default())],
            vec![TenantRoleHook {
                name: "h",
                service: "b",
                tenants: "SELECT NULL::text WHERE false",
                role_prefix: "t_",
                template: "SELECT 1",
            }],
        ),
    ];
    for (what, services, hooks) in cases {
        let services: &'static [Service] = Box::leak(services.into_boxed_slice());
        let hooks: &'static [TenantRoleHook] = Box::leak(hooks.into_boxed_slice());
        let registry = Registry {
            services,
            clickhouse: NONE,
            tenant_role_hooks: hooks,
        };
        assert!(check_registry(&registry).is_err(), "{what} must be refused");
    }
}
