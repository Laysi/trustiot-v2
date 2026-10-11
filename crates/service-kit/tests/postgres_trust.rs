//! The Postgres server certificate must chain to the configured CA and to nothing else
//! (Laysi/trustiot-v2#272): sqlx builds its trust store from `webpki_roots::TLS_SERVER_ROOTS`
//! plus the configured root certificate, so the workspace replaces `webpki-roots` with an empty
//! list (`crates/webpki-roots-empty`). These tests check that the replacement is the crate sqlx
//! is built with, that sqlx loads no other trust source, and, against a real Postgres with TLS,
//! that only the configured CA lets a connection through.
//!
//! The replacement is temporary: once sqlx accepts a custom rustls configuration or offers a
//! rustls feature without built-in roots, Laysi/trustiot-v2#304 removes the patch and the
//! lock-file guard, and keeps the real Postgres test.

mod support;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use service_kit::postgres::{PostgresConfig, SslMode};
use sqlx::postgres::{PgConnectOptions, PgSslMode};
use sqlx::{Connection, PgConnection};
use support::postgres::{SERVER_HOST, TEST_PASSWORD, TestCa, TlsPostgres, tls_postgres};

/// Appended to every guard failure: where the workaround is tracked and how it goes away.
const WORKAROUND: &str = "the empty webpki-roots patch is a temporary workaround until sqlx \
     accepts a custom rustls config; see Laysi/trustiot-v2#304 for how to replace and remove it";

/// Crates that give rustls or sqlx trust anchors besides the configured CA: the system store,
/// the platform verifier, Mozilla's roots as certificates, or another TLS stack.
const OTHER_TRUST_SOURCES: [&str; 6] = [
    "native-tls",
    "openssl",
    "rustls-native-certs",
    "rustls-platform-verifier",
    "webpki-root-certs",
    "security-framework",
];

#[test]
fn sqlx_is_built_with_no_public_roots() {
    // The real crate (1.0.9) lists 121 roots; sqlx adds every one of them to its trust store.
    assert!(
        webpki_roots::TLS_SERVER_ROOTS.is_empty(),
        "webpki-roots is not the empty stand-in: sqlx would trust {} public roots for Postgres \
         ({WORKAROUND})",
        webpki_roots::TLS_SERVER_ROOTS.len()
    );
}

/// One `[[package]]` of `Cargo.lock`.
struct LockedPackage {
    name: String,
    source: Option<String>,
    /// Dependency names, without the version or source the lock file adds when ambiguous.
    dependencies: BTreeSet<String>,
}

fn locked_packages() -> Vec<LockedPackage> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../Cargo.lock");
    let text = std::fs::read_to_string(&path).expect("read Cargo.lock");
    let lock: toml::Table = text.parse().expect("parse Cargo.lock");
    let packages = lock["package"].as_array().expect("package array");
    packages
        .iter()
        .map(|package| LockedPackage {
            name: package["name"].as_str().expect("name").to_owned(),
            source: package
                .get("source")
                .and_then(toml::Value::as_str)
                .map(str::to_owned),
            dependencies: package
                .get("dependencies")
                .and_then(toml::Value::as_array)
                .map(|dependencies| {
                    dependencies
                        .iter()
                        .map(|dependency| {
                            let text = dependency.as_str().expect("dependency");
                            text.split(' ').next().unwrap_or(text).to_owned()
                        })
                        .collect()
                })
                .unwrap_or_default(),
        })
        .collect()
}

/// `Cargo.lock` lists the optional dependencies that some member turns on, so it shows what
/// sqlx-core can be built with anywhere in the workspace. An sqlx upgrade that needs a newer
/// `webpki-roots` than the stand-in leaves the patch unused (Cargo only warns), and this fails.
#[test]
fn the_lock_file_gives_sqlx_no_trust_anchor_but_the_configured_ca() {
    let packages = locked_packages();

    let roots: Vec<&LockedPackage> = packages
        .iter()
        .filter(|package| package.name == "webpki-roots")
        .collect();
    assert_eq!(
        roots.len(),
        1,
        "exactly one webpki-roots in the graph; an sqlx upgrade may have left the patch \
         unused ({WORKAROUND})"
    );
    assert_eq!(
        roots[0].source, None,
        "webpki-roots must be the path crate crates/webpki-roots-empty, not crates.io \
         ({WORKAROUND})"
    );

    let sqlx_core = packages
        .iter()
        .find(|package| package.name == "sqlx-core")
        .expect("sqlx-core is locked");
    // sqlx-core uses webpki-roots only when `rustls-native-certs` is off; with it, sqlx would
    // load the system store instead.
    for source in OTHER_TRUST_SOURCES {
        assert!(
            !sqlx_core.dependencies.contains(source),
            "sqlx-core is built with {source}, which trusts more than the configured CA \
             ({WORKAROUND})"
        );
    }
    assert!(
        sqlx_core.dependencies.contains("rustls")
            && sqlx_core.dependencies.contains("webpki-roots"),
        "sqlx-core must be built with rustls and webpki-roots (feature tls-rustls-ring-webpki), \
         so its trust store starts from the empty stand-in ({WORKAROUND})"
    );

    let dependents: BTreeSet<&str> = packages
        .iter()
        .filter(|package| package.dependencies.contains("webpki-roots"))
        .map(|package| package.name.as_str())
        .collect();
    assert_eq!(
        dependents,
        BTreeSet::from(["service-kit", "sqlx-core"]),
        "only sqlx (and this test) may use webpki-roots: the empty stand-in would leave any \
         other client without public roots; such a client should use the system store or \
         webpki-root-certs ({WORKAROUND})"
    );
    println!("sqlx-core trust sources: webpki-roots (empty stand-in) plus the configured CA");
}

/// Writes `pem` to a fresh file under the test's temporary directory.
fn ca_file(name: &str, pem: &str) -> PathBuf {
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(support::unique_name(name));
    std::fs::write(&path, pem).expect("write the CA file");
    path
}

fn config(postgres: &TlsPostgres, host: &str, ca_file: &Path) -> PostgresConfig {
    PostgresConfig {
        host: host.to_owned(),
        port: postgres.address.port(),
        database: "postgres".to_owned(),
        ssl_mode: SslMode::VerifyFull,
        ca_file: ca_file.to_owned(),
        username: "postgres".to_owned(),
        password: TEST_PASSWORD.to_owned(),
    }
}

async fn connect(options: &PgConnectOptions) -> Result<(), sqlx::Error> {
    let mut connection = PgConnection::connect_with(options).await?;
    let encrypted: bool =
        sqlx::query_scalar("SELECT ssl FROM pg_stat_ssl WHERE pid = pg_backend_pid()")
            .fetch_one(&mut connection)
            .await?;
    assert!(encrypted, "the session uses TLS");
    connection.close().await
}

/// Against a real Postgres whose certificate names `SERVER_HOST` and is signed by the test CA:
/// the configured CA is the only way in.
#[tokio::test]
async fn only_the_configured_ca_is_trusted() {
    let Some(postgres) = tls_postgres("SELECT 1").await else {
        return;
    };
    let configured = ca_file("configured-ca.crt", &postgres.ca_pem);
    // Any CA but the one that signed the server certificate, standing in for a public CA that
    // issued a certificate for the database host name.
    let other = TestCa::new("another CA");
    let other_file = ca_file("other-ca.crt", &other.pem);
    let bundle = ca_file("bundle.crt", &format!("{}{}", other.pem, postgres.ca_pem));

    connect(&config(&postgres, SERVER_HOST, &configured).connect_options())
        .await
        .expect("the configured CA is trusted");
    println!("configured CA, matching host: connected");

    connect(&config(&postgres, SERVER_HOST, &bundle).connect_options())
        .await
        .expect("a CA file may hold several CAs, for example during a CA change");
    println!("bundle with the configured CA: connected");

    let error = connect(&config(&postgres, SERVER_HOST, &other_file).connect_options())
        .await
        .expect_err("a server certificate from a CA that is not configured is refused");
    println!("another CA configured, matching host: {error}");
    assert!(error.to_string().contains("UnknownIssuer"), "{error}");

    // The certificate names 127.0.0.1 only; "localhost" reaches the same server.
    let error = connect(&config(&postgres, "localhost", &configured).connect_options())
        .await
        .expect_err("a host name the certificate does not name is refused");
    println!("configured CA, wrong host name: {error}");
    assert!(error.to_string().contains("not valid for name"), "{error}");

    // No root certificate at all: sqlx's store is then empty, so nothing is trusted.
    let without_ca = PgConnectOptions::new_without_pgpass()
        .host(SERVER_HOST)
        .port(postgres.address.port())
        .username("postgres")
        .password(TEST_PASSWORD)
        .database("postgres")
        .ssl_mode(PgSslMode::VerifyFull);
    let error = connect(&without_ca)
        .await
        .expect_err("without a configured CA no server is trusted");
    println!("no CA configured: {error}");
    assert!(error.to_string().contains("UnknownIssuer"), "{error}");

    for file in [configured, other_file, bundle] {
        let _ = std::fs::remove_file(file);
    }
}
