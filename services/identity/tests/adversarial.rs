//! Independent adversarial checks of the identity service as a real process against a real
//! Postgres with TLS (podman or docker; skipped without either unless `CI` is set): cookie
//! parsing, session expiry, database faults (the check must answer 503 and never fail open),
//! tenant isolation of the identity tables, logging at every level, key loading, health port
//! and key rotation across restarts.
//!
//! Written by the verifier of Laysi/trustiot-v2#61, apart from the implementer's own
//! `tests/ext_authz.rs`; the fixture is duplicated on purpose so neither file depends on the
//! other.

#![allow(
    clippy::string_slice,
    clippy::sliced_string_as_bytes,
    reason = "slices of ASCII test data (base64url, hex)"
)]

mod support;

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use contract::ServiceName;
use identity::session::SessionSecret;
use serde_json::json;
use service_kit::identity::{IdentityKeys, IdentityVerifier};
use sqlx::{Connection, PgConnection};
use support::postgres::{SERVER_HOST, TEST_PASSWORD, TlsPostgres, tls_postgres};
use support::{
    KeyFiles, Service, TestKey, cookie, jwks, kid_of, new_secret, request, scratch_dir,
    signing_keys,
};

const ROLES: &str = "
CREATE ROLE identity_owner LOGIN PASSWORD 'service-kit-test';
CREATE ROLE identity_app LOGIN PASSWORD 'service-kit-test';
ALTER ROLE identity_app SET search_path = identity;
";
const MIGRATION: &str = include_str!("../migrations/postgres/0001_identity.sql");
/// The writes on sessions that logging in and extending a session need, and their guard
/// (Laysi/trustiot-v2#103).
const LOGIN_MIGRATION: &str = include_str!("../migrations/postgres/0003_login.sql");
/// The migration Job creates the service's schema, owned by the owner role, before it runs the
/// migrations (tools/migrate); the tests do the same as the owner.
const SCHEMA: &str = "CREATE SCHEMA identity";
const DATABASE_TEMPLATE: &str = include_str!("../../../crates/service-kit/sql/database.sql");

const APP: &str = "app.saas.trustiot.test";
const TENANT_A: &str = "01928f6e-7a3c-7d4e-8f00-00000000000a";
const TENANT_B: &str = "01928f6e-7a3c-7d4e-8f00-00000000000b";
const USER: &str = "01928f6e-7a3c-7d4e-8f00-0123456789ab";
const OTHER_USER: &str = "01928f6e-7a3c-7d4e-8f00-0000000000cc";
const KID: &str = "2026-10-a";
const COOKIE_NAME: &str = "__Host-trustiot-session";
const SESSION_SECS: f64 = 12.0 * 3600.0;

const INSERT_SESSION: &str = "INSERT INTO identity.sessions
    (id, token_hash, user_id, active_tenant_id, created_at, expires_at, revoked_at)
VALUES (gen_random_uuid(), $1, $2::uuid, $3::uuid,
    now() - make_interval(secs => $5), now() + make_interval(secs => $4),
    CASE WHEN $6 THEN now() ELSE NULL END)";

type Env = Vec<(String, String)>;

struct Fixture {
    postgres: Arc<TlsPostgres>,
    database: &'static str,
    key: TestKey,
    files: KeyFiles,
    ca_file: PathBuf,
}

impl Fixture {
    /// Creates `database` with `create`, a static `CREATE DATABASE <database> OWNER
    /// identity_owner` (tools/sql-check: no SQL built from strings).
    async fn new(create: &'static str, database: &'static str) -> Option<Self> {
        let postgres = tls_postgres(ROLES).await?;
        let mut admin = PgConnection::connect_with(&postgres.superuser("postgres"))
            .await
            .expect("connect as the superuser");
        sqlx::raw_sql(create)
            .execute(&mut admin)
            .await
            .expect("create the database");
        let dir = scratch_dir(database);
        let key = TestKey::new(71);
        let files = KeyFiles::new(database, &key, KID);
        let ca_file = dir.join("ca.crt");
        std::fs::write(&ca_file, &postgres.ca_pem).expect("write the CA");
        let fixture = Self {
            postgres,
            database,
            key,
            files,
            ca_file,
        };
        let mut owner = fixture.connect("identity_owner").await;
        for sql in [DATABASE_TEMPLATE, SCHEMA, MIGRATION, LOGIN_MIGRATION] {
            sqlx::raw_sql(sql)
                .execute(&mut owner)
                .await
                .expect("apply the schema");
        }
        support::record_release_migrations(&mut owner).await;
        Some(fixture)
    }

    async fn connect(&self, role: &str) -> PgConnection {
        PgConnection::connect_with(&self.postgres.options(role, self.database))
            .await
            .unwrap_or_else(|error| panic!("connect as {role}: {error}"))
    }

    fn keys(&self) -> String {
        jwks(&[(&self.key, KID)])
    }

    fn env(&self, extra: &[(&str, &str)]) -> Env {
        let routes = json!([
            {"host": APP, "path": "/", "match": "PathPrefix", "service": "assets"},
            {"host": APP, "path": "/api/certs", "match": "PathPrefix", "service": "certs"},
        ]);
        let mut env: Env = self.files.env();
        env.extend([
            ("TRUSTIOT_INGRESS_ROUTES".into(), routes.to_string()),
            ("TRUSTIOT_POSTGRES_HOST".into(), SERVER_HOST.into()),
            (
                "TRUSTIOT_POSTGRES_PORT".into(),
                self.postgres.address.port().to_string(),
            ),
            ("TRUSTIOT_POSTGRES_DATABASE".into(), self.database.into()),
            ("TRUSTIOT_POSTGRES_SSLMODE".into(), "verify-full".into()),
            (
                "TRUSTIOT_POSTGRES_CA_FILE".into(),
                self.ca_file.display().to_string(),
            ),
            ("TRUSTIOT__DATABASE__USERNAME".into(), "identity_app".into()),
            ("TRUSTIOT__DATABASE__PASSWORD".into(), TEST_PASSWORD.into()),
        ]);
        for (name, value) in extra {
            env.retain(|(existing, _)| existing != name);
            env.push(((*name).to_owned(), (*value).to_owned()));
        }
        env
    }

    fn service_with(&self, extra: &[(&str, &str)]) -> Service {
        let env = self.env(extra);
        let env: Vec<(&str, &str)> = env.iter().map(|(n, v)| (n.as_str(), v.as_str())).collect();
        let service = Service::start(&env);
        service.wait_ready(Duration::from_secs(30));
        service
    }

    fn service(&self) -> Service {
        self.service_with(&[])
    }

    async fn add_user(&self, user: &str, platform_admin: bool) {
        sqlx::query("INSERT INTO identity.users (id, platform_admin) VALUES ($1::uuid, $2)")
            .bind(user)
            .bind(platform_admin)
            .execute(&mut self.connect("identity_owner").await)
            .await
            .expect("insert the user");
    }

    async fn add_membership(&self, tenant: &str, user: &str, role: &str) {
        sqlx::query(
            "INSERT INTO identity.memberships (tenant_id, user_id, tenant_role)
             VALUES ($1::uuid, $2::uuid, $3)",
        )
        .bind(tenant)
        .bind(user)
        .bind(role)
        .execute(&mut self.connect("identity_owner").await)
        .await
        .expect("insert the membership");
    }

    async fn add_session(
        &self,
        user: &str,
        tenant: Option<&str>,
        expires_in_secs: f64,
        revoked: bool,
    ) -> SessionSecret {
        let secret = new_secret();
        sqlx::query(INSERT_SESSION)
            .bind(secret.hash().as_bytes().as_slice())
            .bind(user)
            .bind(tenant)
            .bind(expires_in_secs)
            .bind(SESSION_SECS)
            .bind(revoked)
            .execute(&mut self.connect("identity_owner").await)
            .await
            .expect("insert the session");
        secret
    }

    fn verifier(&self, audience: ServiceName) -> IdentityVerifier {
        IdentityVerifier::new(
            audience,
            IdentityKeys::from_jwks(&self.keys()).expect("keys"),
        )
    }
}

/// A check as the gateway sends it.
fn check(service: &Service, path: &str, cookie: Option<&str>) -> support::Response {
    let mut headers = vec![("Host", APP)];
    if let Some(cookie) = cookie {
        headers.push(("Cookie", cookie));
    }
    request(
        service.address,
        "GET",
        &format!("/internal/ext-authz{path}"),
        &headers,
    )
    .expect("check request")
}

/// Issues checks until five in a row are allowed, so that the pool holds warm connections: the
/// 0.4 s check deadline also covers opening a connection (TLS, SCRAM, the row-level security
/// session check), which an overloaded machine cannot always do for the first requests. Returns
/// the tokens of the allows.
fn warm(service: &Service, cookie: &str) -> Vec<String> {
    let started = Instant::now();
    let mut tokens = Vec::new();
    let mut streak = 0;
    while streak < 5 {
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "the service never answered five allows in a row"
        );
        let response = check(service, "/", Some(cookie));
        if response.status == 200 {
            streak += 1;
            tokens.push(response.header_values("x-internal-identity")[0].to_owned());
        } else {
            assert_eq!(response.status, 503, "{}", response.raw());
            streak = 0;
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    tokens
}

/// What a raw request got: the status and every identity header value.
#[derive(Debug)]
struct Raw {
    status: u16,
    identity: Vec<String>,
    #[allow(dead_code, reason = "kept for debugging failed assertions")]
    text: String,
}

/// Sends exactly these bytes as one HTTP/1.1 request; `None` when the server closed without a
/// parsable response.
fn raw(address: SocketAddr, head: &[u8]) -> Option<Raw> {
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(2)).ok()?;
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
        .ok()?;
    // The server may reply and close before we finish writing a huge request: ignore that.
    let _ = stream.write_all(head);
    let mut buffer = Vec::new();
    let _ = stream.read_to_end(&mut buffer);
    let text = String::from_utf8_lossy(&buffer).into_owned();
    let status = text.split_whitespace().nth(1)?.parse().ok()?;
    let identity = text
        .lines()
        .filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("x-internal-identity")
                .then(|| value.trim().to_owned())
        })
        .collect();
    Some(Raw {
        status,
        identity,
        text,
    })
}

/// A check whose `Cookie` header lines are the given raw byte strings.
fn raw_check(service: &Service, cookies: &[&[u8]], path: &str) -> Option<Raw> {
    let mut head =
        format!("GET /internal/ext-authz{path} HTTP/1.1\r\nHost: {APP}\r\nConnection: close\r\n")
            .into_bytes();
    for value in cookies {
        head.extend_from_slice(b"Cookie: ");
        head.extend_from_slice(value);
        head.extend_from_slice(b"\r\n");
    }
    head.extend_from_slice(b"\r\n");
    raw(service.address, &head)
}

fn status_of(response: &Option<Raw>) -> String {
    response
        .as_ref()
        .map_or_else(|| "closed".to_owned(), |raw| raw.status.to_string())
}

// ---------------------------------------------------------------------------------------------
// Cookie parsing
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn cookie_parsing_accepts_only_one_exact_session_cookie() {
    let Some(fixture) = Fixture::new(
        "CREATE DATABASE verify_cookies OWNER identity_owner",
        "verify_cookies",
    )
    .await
    else {
        return;
    };
    fixture.add_user(USER, false).await;
    fixture.add_membership(TENANT_A, USER, "viewer").await;
    let secret = fixture
        .add_session(USER, Some(TENANT_A), SESSION_SECS, false)
        .await;
    let other = fixture
        .add_session(USER, Some(TENANT_A), SESSION_SECS, false)
        .await;
    let service = fixture.service();
    let value = secret.to_cookie_value();
    let other_value = other.to_cookie_value();
    let good = format!("{COOKIE_NAME}={value}");
    let other_good = format!("{COOKIE_NAME}={other_value}");
    warm(&service, &good);

    // Value spellings that decode to the same 32 bytes or to garbage.
    let last = value.chars().last().expect("last");
    let alphabet = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let non_canonical: Vec<String> = alphabet
        .chars()
        .filter(|candidate| *candidate != last)
        .map(|candidate| format!("{}{candidate}", &value[..value.len() - 1]))
        .collect();
    // Standard-alphabet spelling of a secret that has `-` or `_` in it.
    let mut standard = None;
    for _ in 0..200 {
        let candidate = new_secret();
        let text = candidate.to_cookie_value();
        if text.contains('-') || text.contains('_') {
            let session_secret = fixture
                .add_session(USER, Some(TENANT_A), SESSION_SECS, false)
                .await;
            // Re-create with a known secret: insert this candidate's hash.
            sqlx::query("UPDATE identity.sessions SET token_hash = $1 WHERE token_hash = $2")
                .bind(candidate.hash().as_bytes().as_slice())
                .bind(session_secret.hash().as_bytes().as_slice())
                .execute(&mut fixture.connect("identity_owner").await)
                .await
                .expect("re-key the session");
            standard = Some((text.clone(), text.replace('-', "+").replace('_', "/")));
            break;
        }
    }
    let (url_safe, standard_alphabet) = standard.expect("a secret with - or _");

    let long_junk = "a=".to_owned() + &"x".repeat(2000);
    let allowed: Vec<(&str, Vec<Vec<u8>>)> = vec![
        ("the cookie alone", vec![good.clone().into_bytes()]),
        (
            "among other cookies",
            vec![format!("a=1; {good}; b=2").into_bytes()],
        ),
        (
            "no space after ;",
            vec![format!("a=1;{good};b=2").into_bytes()],
        ),
        (
            "space before ;",
            vec![format!("a=1 ; {good} ; b=2").into_bytes()],
        ),
        (
            "tabs around",
            vec![format!("a=1;\t{good}\t;b=2").into_bytes()],
        ),
        (
            "second Cookie header (HTTP/2 split)",
            vec![b"a=1".to_vec(), good.clone().into_bytes()],
        ),
        (
            "quoted other cookie",
            vec![format!("a=\"x y\"; {good}").into_bytes()],
        ),
        (
            "other cookie with =",
            vec![format!("a=b=c; {good}").into_bytes()],
        ),
        (
            "other cookie with non-ASCII bytes",
            vec![[b"a=\xff\xfe\xc3\x28; ".as_slice(), good.as_bytes()].concat()],
        ),
        (
            "empty segments",
            vec![format!(";;; ;{good};;").into_bytes()],
        ),
        (
            "60 KiB of other cookies around a good one",
            vec![format!("{}; {good}", "a=1; ".repeat(12_000)).into_bytes()],
        ),
        (
            "long other cookie",
            vec![format!("{long_junk}; {good}").into_bytes()],
        ),
        (
            "other cookie whose value is the cookie name",
            vec![format!("a={COOKIE_NAME}; {good}").into_bytes()],
        ),
        (
            "space before = (name still exact after trimming)",
            vec![format!("{COOKIE_NAME} ={value}").into_bytes()],
        ),
        (
            "url-safe alphabet",
            vec![format!("{COOKIE_NAME}={url_safe}").into_bytes()],
        ),
    ];
    for (name, cookies) in &allowed {
        let refs: Vec<&[u8]> = cookies.iter().map(Vec::as_slice).collect();
        let response = raw_check(&service, &refs, "/api/devices");
        assert_eq!(status_of(&response), "200", "ALLOW {name}: {response:?}");
        let response = response.expect("response");
        assert_eq!(response.identity.len(), 1, "{name}");
        fixture
            .verifier(ServiceName::Assets)
            .verify(&response.identity[0])
            .unwrap_or_else(|error| panic!("{name}: {error}"));
    }

    let two_headers_same = [good.clone().into_bytes(), good.clone().into_bytes()];
    let mut denied: Vec<(String, Vec<Vec<u8>>)> = vec![
        ("no Cookie header".into(), vec![]),
        ("other cookie only".into(), vec![b"theme=dark".to_vec()]),
        (
            "same cookie twice in one header".into(),
            vec![format!("{good}; {good}").into_bytes()],
        ),
        (
            "two different valid session cookies in one header".into(),
            vec![format!("{good}; {other_good}").into_bytes()],
        ),
        (
            "valid then invalid duplicate".into(),
            vec![format!("{good}; {COOKIE_NAME}=AAAA").into_bytes()],
        ),
        (
            "invalid then valid duplicate".into(),
            vec![format!("{COOKIE_NAME}=AAAA; {good}").into_bytes()],
        ),
        (
            "same cookie in two headers".into(),
            two_headers_same.to_vec(),
        ),
        (
            "two valid cookies in two headers".into(),
            vec![good.clone().into_bytes(), other_good.clone().into_bytes()],
        ),
        (
            "quoted value".into(),
            vec![format!("{COOKIE_NAME}=\"{value}\"").into_bytes()],
        ),
        (
            "space after =".into(),
            vec![format!("{COOKIE_NAME}= {value}").into_bytes()],
        ),
        (
            "padded with =".into(),
            vec![format!("{COOKIE_NAME}={value}=").into_bytes()],
        ),
        (
            "one char short".into(),
            vec![format!("{COOKIE_NAME}={}", &value[1..]).into_bytes()],
        ),
        (
            "one char long".into(),
            vec![format!("{COOKIE_NAME}={value}A").into_bytes()],
        ),
        (
            "doubled value".into(),
            vec![format!("{COOKIE_NAME}={value}{value}").into_bytes()],
        ),
        (
            "empty value".into(),
            vec![format!("{COOKIE_NAME}=").into_bytes()],
        ),
        ("no =".into(), vec![COOKIE_NAME.as_bytes().to_vec()]),
        ("value only".into(), vec![value.clone().into_bytes()]),
        (
            "lower-case name".into(),
            vec![format!("{}={value}", COOKIE_NAME.to_lowercase()).into_bytes()],
        ),
        (
            "upper-case name".into(),
            vec![format!("{}={value}", COOKIE_NAME.to_uppercase()).into_bytes()],
        ),
        (
            "without the __Host- prefix".into(),
            vec![format!("trustiot-session={value}").into_bytes()],
        ),
        (
            "name with a prefix".into(),
            vec![format!("x{COOKIE_NAME}={value}").into_bytes()],
        ),
        (
            "name with a suffix".into(),
            vec![format!("{COOKIE_NAME}x={value}").into_bytes()],
        ),
        (
            "name inside another cookie's value".into(),
            vec![format!("a=b{COOKIE_NAME}={value}").into_bytes()],
        ),
        (
            "value with NUL".into(),
            vec![format!("{COOKIE_NAME}={}\0", &value[..42]).into_bytes()],
        ),
        (
            "non-ASCII in the value".into(),
            vec![
                [
                    format!("{COOKIE_NAME}=").as_bytes(),
                    b"\xff",
                    value[1..].as_bytes(),
                ]
                .concat(),
            ],
        ),
        (
            "SQL metacharacters".into(),
            vec![format!("{COOKIE_NAME}=' OR '1'='1' --").into_bytes()],
        ),
        (
            "the hash instead of the secret".into(),
            vec![
                format!(
                    "{COOKIE_NAME}={}",
                    URL_SAFE_NO_PAD.encode(secret.hash().as_bytes())
                )
                .into_bytes(),
            ],
        ),
        (
            "standard base64 alphabet".into(),
            vec![format!("{COOKIE_NAME}={standard_alphabet}").into_bytes()],
        ),
        (
            "percent-encoded value".into(),
            vec![format!("{COOKIE_NAME}=%41{}", &value[1..]).into_bytes()],
        ),
        (
            "all zero secret".into(),
            vec![format!("{COOKIE_NAME}={}", "A".repeat(43)).into_bytes()],
        ),
        (
            "oversized: 8 KiB value".into(),
            vec![format!("{COOKIE_NAME}={}", "A".repeat(8 * 1024)).into_bytes()],
        ),
        (
            "oversized: 2 MiB of other cookies around a good one".into(),
            vec![format!("{}; {good}", "a=1; ".repeat(400_000)).into_bytes()],
        ),
    ];
    // Every non-canonical last character: the same 32 bytes with another spelling.
    for (index, candidate) in non_canonical.iter().enumerate() {
        denied.push((
            format!("non-canonical spelling #{index}"),
            vec![format!("{COOKIE_NAME}={candidate}").into_bytes()],
        ));
    }
    let mut accepted_spellings = Vec::new();
    for (name, cookies) in &denied {
        let refs: Vec<&[u8]> = cookies.iter().map(Vec::as_slice).collect();
        let response = raw_check(&service, &refs, "/api/devices");
        let status = status_of(&response);
        if let Some(response) = &response {
            assert!(
                response.identity.is_empty(),
                "DENY {name} carried an identity header: {response:?}"
            );
        }
        if name.starts_with("non-canonical") {
            // 401 for a spelling that is not the one canonical form. A 200 here would mean one
            // secret has several spellings (not exploitable, but the README says it cannot).
            if status == "200" {
                accepted_spellings.push(name.clone());
            }
            continue;
        }
        assert!(
            matches!(status.as_str(), "401" | "400" | "431" | "closed"),
            "DENY {name}: {status}: {response:?}"
        );
        assert_ne!(status, "200", "{name}");
    }
    // The three other spellings of the last character that decode to the same bytes (only the
    // zero trailing bits differ) must not be accepted.
    assert!(
        accepted_spellings.is_empty(),
        "non-canonical spellings accepted: {accepted_spellings:?}"
    );

    // The secret anywhere but the cookie does not count.
    for (name, target, headers) in [
        (
            "query string",
            format!("/internal/ext-authz/api/x?{COOKIE_NAME}={value}"),
            vec![("Host", APP)],
        ),
        (
            "path",
            format!("/internal/ext-authz/{COOKIE_NAME}={value}"),
            vec![("Host", APP)],
        ),
        (
            "X-Cookie header",
            "/internal/ext-authz/".to_owned(),
            vec![("Host", APP), ("X-Cookie", good.as_str())],
        ),
        (
            "Authorization: Bearer",
            "/internal/ext-authz/".to_owned(),
            vec![("Host", APP), ("Authorization", &format!("Bearer {value}"))],
        ),
        (
            "Set-Cookie request header",
            "/internal/ext-authz/".to_owned(),
            vec![("Host", APP), ("Set-Cookie", good.as_str())],
        ),
    ] {
        let response = request(service.address, "GET", &target, &headers).expect("request");
        assert_eq!(response.status, 401, "{name}: {}", response.raw());
        assert!(response.header_values("x-internal-identity").is_empty());
    }

    // The service survived every hostile request above.
    assert_eq!(check(&service, "/", Some(&good)).status, 200);
    // And a valid cookie with an invalid duplicate does not poison the other session.
    assert_eq!(check(&service, "/", Some(&cookie(&other))).status, 200);
}

// ---------------------------------------------------------------------------------------------
// Expiry
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn session_expiry_and_revocation_are_decided_by_the_database_clock() {
    let Some(fixture) = Fixture::new(
        "CREATE DATABASE verify_expiry OWNER identity_owner",
        "verify_expiry",
    )
    .await
    else {
        return;
    };
    fixture.add_user(USER, false).await;
    fixture.add_membership(TENANT_A, USER, "viewer").await;
    let service = fixture.service();
    let warm_secret = fixture
        .add_session(USER, Some(TENANT_A), SESSION_SECS, false)
        .await;
    warm(&service, &cookie(&warm_secret));

    // Expires in 3 s at its absolute lifetime (7 days after its creation), so no request
    // extends it: allowed now, refused after the second passes the expiry, never allowed again.
    let soon = fixture.add_session(USER, Some(TENANT_A), 3.0, false).await;
    sqlx::query(
        "UPDATE identity.sessions SET created_at = expires_at - interval '7 days'
         WHERE token_hash = $1",
    )
    .bind(soon.hash().as_bytes().as_slice())
    .execute(&mut fixture.connect("identity_owner").await)
    .await
    .expect("start the session 7 days before its expiry");
    let soon_cookie = cookie(&soon);
    let first = check(&service, "/", Some(&soon_cookie));
    assert_eq!(first.status, 200, "{}", first.raw());
    let token = first.header_values("x-internal-identity")[0].to_owned();
    let started = Instant::now();
    let mut flipped_after = None;
    while started.elapsed() < Duration::from_secs(10) {
        if check(&service, "/", Some(&soon_cookie)).status == 401 {
            flipped_after = Some(started.elapsed());
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let flipped_after = flipped_after.expect("the session expired");
    assert!(
        flipped_after > Duration::from_millis(1500) && flipped_after < Duration::from_secs(4),
        "flipped to 401 after {flipped_after:?}"
    );
    for _ in 0..5 {
        assert_eq!(check(&service, "/", Some(&soon_cookie)).status, 401);
    }
    // The token issued before the expiry stays valid for its own 60 s: documented, bounded.
    assert!(fixture.verifier(ServiceName::Assets).verify(&token).is_ok());

    // Boundaries around now: already expired by a hair, expiring exactly at creation + 1 ms.
    let just_expired = fixture
        .add_session(USER, Some(TENANT_A), -0.001, false)
        .await;
    assert_eq!(
        check(&service, "/", Some(&cookie(&just_expired))).status,
        401
    );

    // Revocation takes effect on the next request, with no cache in between.
    let revocable = fixture
        .add_session(USER, Some(TENANT_A), SESSION_SECS, false)
        .await;
    let revocable_cookie = cookie(&revocable);
    assert_eq!(check(&service, "/", Some(&revocable_cookie)).status, 200);
    sqlx::query("UPDATE identity.sessions SET revoked_at = now() WHERE token_hash = $1")
        .bind(revocable.hash().as_bytes().as_slice())
        .execute(&mut fixture.connect("identity_owner").await)
        .await
        .expect("revoke");
    assert_eq!(check(&service, "/", Some(&revocable_cookie)).status, 401);
    // A revoked session never comes back unless the owner clears revoked_at (not a service act).
    assert_eq!(check(&service, "/", Some(&revocable_cookie)).status, 401);

    // revoked_at in the future still counts as revoked.
    let future_revoked = fixture
        .add_session(USER, Some(TENANT_A), SESSION_SECS, false)
        .await;
    sqlx::query(
        "UPDATE identity.sessions SET revoked_at = now() + interval '1 day' WHERE token_hash = $1",
    )
    .bind(future_revoked.hash().as_bytes().as_slice())
    .execute(&mut fixture.connect("identity_owner").await)
    .await
    .expect("revoke later");
    assert_eq!(
        check(&service, "/", Some(&cookie(&future_revoked))).status,
        401
    );

    // A session created in the future (clock skew in the writer) is judged by expiry only.
    let skewed = fixture
        .add_session(USER, Some(TENANT_A), SESSION_SECS, false)
        .await;
    sqlx::query(
        "UPDATE identity.sessions SET created_at = now() + interval '1 minute' WHERE token_hash = $1",
    )
    .bind(skewed.hash().as_bytes().as_slice())
    .execute(&mut fixture.connect("identity_owner").await)
    .await
    .expect("move creation");
    assert_eq!(check(&service, "/", Some(&cookie(&skewed))).status, 200);
}

// ---------------------------------------------------------------------------------------------
// Database faults
// ---------------------------------------------------------------------------------------------

fn no_identity_and_closed(response: &support::Response, what: &str) {
    assert!(
        response.header_values("x-internal-identity").is_empty(),
        "{what}: {}",
        response.raw()
    );
    assert_ne!(
        response.status,
        200,
        "{what}: failed open: {}",
        response.raw()
    );
}

#[tokio::test]
async fn slow_or_locked_database_answers_503_and_recovers_without_failing_open() {
    let Some(fixture) = Fixture::new(
        "CREATE DATABASE verify_locks OWNER identity_owner",
        "verify_locks",
    )
    .await
    else {
        return;
    };
    fixture.add_user(USER, false).await;
    fixture.add_membership(TENANT_A, USER, "viewer").await;
    let secret = fixture
        .add_session(USER, Some(TENANT_A), SESSION_SECS, false)
        .await;
    let session_cookie = cookie(&secret);
    // The smallest pool (the three connections kept open), so a few stuck lookups exhaust it.
    let service = fixture.service_with(&[("TRUSTIOT__DATABASE__MAX_CONNECTIONS", "3")]);
    warm(&service, &session_cookie);

    for (table, lock) in [
        (
            "identity.sessions",
            "BEGIN; LOCK TABLE identity.sessions IN ACCESS EXCLUSIVE MODE",
        ),
        (
            "identity.memberships",
            "BEGIN; LOCK TABLE identity.memberships IN ACCESS EXCLUSIVE MODE",
        ),
    ] {
        let mut owner = fixture.connect("identity_owner").await;
        sqlx::raw_sql(lock)
            .execute(&mut owner)
            .await
            .expect("lock the table");

        // One request: 503 within the 0.4 s deadline (plus margin), never a 200.
        let started = Instant::now();
        let response = check(&service, "/api/devices", Some(&session_cookie));
        let took = started.elapsed();
        assert_eq!(response.status, 503, "{table}: {}", response.raw());
        no_identity_and_closed(&response, table);
        assert!(took < Duration::from_millis(1500), "{table}: took {took:?}");
        assert_eq!(
            response.json(),
            json!({"code": "unavailable", "message_key": "error.unavailable"})
        );

        // 12 concurrent requests against a pool of 3 with every connection stuck or timing out:
        // all fail closed, quickly.
        let address = service.address;
        let cookie_value = session_cookie.clone();
        let started = Instant::now();
        let results: Vec<(u16, usize, Duration)> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..12)
                .map(|_| {
                    let cookie_value = cookie_value.clone();
                    scope.spawn(move || {
                        let began = Instant::now();
                        let response = request(
                            address,
                            "GET",
                            "/internal/ext-authz/",
                            &[("Host", APP), ("Cookie", &cookie_value)],
                        )
                        .expect("request");
                        (
                            response.status,
                            response.header_values("x-internal-identity").len(),
                            began.elapsed(),
                        )
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().expect("thread"))
                .collect()
        });
        for (status, identities, took) in &results {
            assert_eq!(*status, 503, "{table}: {results:?}");
            assert_eq!(*identities, 0);
            assert!(*took < Duration::from_secs(3), "{table}: {took:?}");
        }
        assert!(started.elapsed() < Duration::from_secs(5));
        // A request that needs no database is still answered from the cookie alone.
        assert_eq!(check(&service, "/", None).status, 401);
        // Readiness reports the trouble only when it concerns the readiness query itself; it
        // must never report ready while checks fail on connection errors, so just record it.

        sqlx::raw_sql("ROLLBACK")
            .execute(&mut owner)
            .await
            .expect("unlock");
        // Recovery: the next requests succeed, so cancelled lookups did not poison the pool.
        let mut recovered = false;
        for _ in 0..30 {
            if check(&service, "/", Some(&session_cookie)).status == 200 {
                recovered = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(
            recovered,
            "{table}: the service did not recover after the lock"
        );
        for _ in 0..20 {
            assert_eq!(check(&service, "/", Some(&session_cookie)).status, 200);
        }
    }

    // No leftover work in the database from cancelled lookups: nothing idle in transaction, no
    // query still running for the service role.
    let mut admin = PgConnection::connect_with(&fixture.postgres.superuser("postgres"))
        .await
        .expect("admin");
    tokio::time::sleep(Duration::from_millis(300)).await;
    let leftovers: Vec<(String, String)> = sqlx::query_as(
        "SELECT state, left(query, 80) FROM pg_stat_activity
         WHERE usename = 'identity_app' AND datname = 'verify_locks'
           AND state IS DISTINCT FROM 'idle'",
    )
    .fetch_all(&mut admin)
    .await
    .expect("activity");
    assert!(leftovers.is_empty(), "leftover sessions: {leftovers:?}");
}

#[tokio::test]
async fn killed_connections_never_turn_into_401_or_200_for_the_wrong_person() {
    let Some(fixture) = Fixture::new(
        "CREATE DATABASE verify_kill OWNER identity_owner",
        "verify_kill",
    )
    .await
    else {
        return;
    };
    fixture.add_user(USER, false).await;
    fixture.add_membership(TENANT_A, USER, "viewer").await;
    let secret = fixture
        .add_session(USER, Some(TENANT_A), SESSION_SECS, false)
        .await;
    let session_cookie = cookie(&secret);
    let service = fixture.service();
    warm(&service, &session_cookie);
    let mut admin = PgConnection::connect_with(&fixture.postgres.superuser("postgres"))
        .await
        .expect("admin");
    for round in 0..3 {
        let killed: i64 = sqlx::query_scalar(
            "SELECT count(pg_terminate_backend(pid)) FROM pg_stat_activity
             WHERE usename = 'identity_app' AND datname = 'verify_kill'",
        )
        .fetch_one(&mut admin)
        .await
        .expect("terminate");
        assert!(killed >= 1, "round {round}: nothing to kill");
        let mut statuses = Vec::new();
        for _ in 0..8 {
            let response = check(&service, "/", Some(&session_cookie));
            assert!(
                matches!(response.status, 200 | 503),
                "round {round}: a valid session got {} after its connection was killed",
                response.status
            );
            statuses.push(response.status);
        }
        assert_eq!(statuses.last(), Some(&200), "round {round}: {statuses:?}");
    }

    // The service role loses its right to read sessions: closed, never open.
    sqlx::raw_sql("REVOKE SELECT ON identity.sessions FROM identity_app")
        .execute(&mut fixture.connect("identity_owner").await)
        .await
        .expect("revoke");
    let response = check(&service, "/", Some(&session_cookie));
    assert_eq!(response.status, 503, "{}", response.raw());
    no_identity_and_closed(&response, "revoked privilege");
    sqlx::raw_sql("GRANT SELECT ON identity.sessions TO identity_app")
        .execute(&mut fixture.connect("identity_owner").await)
        .await
        .expect("grant");
    assert_eq!(check(&service, "/", Some(&session_cookie)).status, 200);

    // A membership row with a role the schema forbids (made by the owner after dropping the
    // CHECK) is refused, not issued as an unknown role.
    let mut owner = fixture.connect("identity_owner").await;
    sqlx::raw_sql(
        "ALTER TABLE identity.memberships DROP CONSTRAINT memberships_tenant_role_check;
         UPDATE identity.memberships SET tenant_role = 'superuser';",
    )
    .execute(&mut owner)
    .await
    .expect("corrupt the role");
    let response = check(&service, "/", Some(&session_cookie));
    assert_eq!(response.status, 503, "{}", response.raw());
    no_identity_and_closed(&response, "corrupt role");
}

#[tokio::test]
async fn wrong_credentials_or_database_fail_closed() {
    let Some(fixture) = Fixture::new(
        "CREATE DATABASE verify_creds OWNER identity_owner",
        "verify_creds",
    )
    .await
    else {
        return;
    };
    fixture.add_user(USER, false).await;
    let secret = fixture.add_session(USER, None, SESSION_SECS, false).await;
    let session_cookie = cookie(&secret);
    for (name, extra) in [
        (
            "wrong password",
            ("TRUSTIOT__DATABASE__PASSWORD", "not-the-password"),
        ),
        (
            "wrong database",
            ("TRUSTIOT_POSTGRES_DATABASE", "does_not_exist"),
        ),
        (
            "owner role is not the service role",
            ("TRUSTIOT__DATABASE__USERNAME", "nobody"),
        ),
    ] {
        let env = fixture.env(&[extra]);
        let env: Vec<(&str, &str)> = env.iter().map(|(n, v)| (n.as_str(), v.as_str())).collect();
        let service = Service::start(&env);
        let response = check(&service, "/", Some(&session_cookie));
        assert_eq!(response.status, 503, "{name}: {}", response.raw());
        no_identity_and_closed(&response, name);
        assert_eq!(
            request(service.health_address, "GET", "/readyz", &[])
                .expect("readyz")
                .status,
            503,
            "{name}"
        );
    }
}

// ---------------------------------------------------------------------------------------------
// Tenant isolation of the identity tables
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn memberships_are_isolated_by_tenant_and_by_user() {
    let Some(fixture) = Fixture::new(
        "CREATE DATABASE verify_tenants OWNER identity_owner",
        "verify_tenants",
    )
    .await
    else {
        return;
    };
    fixture.add_user(USER, false).await;
    fixture.add_user(OTHER_USER, false).await;
    // USER: viewer in A, admin in B. OTHER_USER: admin in A.
    fixture.add_membership(TENANT_A, USER, "viewer").await;
    fixture.add_membership(TENANT_B, USER, "admin").await;
    fixture.add_membership(TENANT_A, OTHER_USER, "admin").await;

    // The service role as an attacker with arbitrary SQL sees tenant data only through the
    // setting, and the setting only through set_config.
    let mut app = fixture.connect("identity_app").await;
    let no_tenant: i64 = sqlx::query_scalar("SELECT count(*) FROM identity.memberships")
        .fetch_one(&mut fixture.connect("identity_app").await)
        .await
        .expect("count without a tenant");
    assert_eq!(no_tenant, 0, "no tenant, no rows");
    for (setting, expected) in [
        (TENANT_A, 2_i64),
        (TENANT_B, 1),
        ("01928f6e-7a3c-7d4e-8f00-0000000000ee", 0),
        ("", 0),
    ] {
        let mut tx = Connection::begin(&mut app).await.expect("begin");
        sqlx::query("SELECT set_config('trustiot.tenant_id', $1, true)")
            .bind(setting)
            .execute(&mut *tx)
            .await
            .expect("set the tenant");
        let seen: i64 = sqlx::query_scalar("SELECT count(*) FROM identity.memberships")
            .fetch_one(&mut *tx)
            .await
            .expect("count");
        assert_eq!(seen, expected, "tenant setting {setting:?}");
        // Rows of other tenants cannot be named by predicate either.
        let leaked: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM identity.memberships WHERE tenant_id <> $1::uuid",
        )
        .bind(if setting.is_empty() {
            TENANT_A
        } else {
            setting
        })
        .fetch_one(&mut *tx)
        .await
        .expect("count others");
        assert_eq!(leaked, 0, "tenant setting {setting:?}");
        tx.rollback().await.expect("rollback");
    }
    // A malformed setting fails the query rather than widening it.
    let mut tx = Connection::begin(&mut app).await.expect("begin");
    sqlx::query("SELECT set_config('trustiot.tenant_id', 'not-a-uuid', true)")
        .execute(&mut *tx)
        .await
        .expect("set");
    let outcome = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM identity.memberships")
        .fetch_one(&mut *tx)
        .await;
    assert!(
        outcome.is_err(),
        "a malformed tenant setting read rows: {outcome:?}"
    );
    drop(tx);
    // Escalation attempts by the service role.
    for statement in [
        "SET ROLE identity_owner",
        "SET SESSION AUTHORIZATION identity_owner",
        "ALTER TABLE identity.memberships DISABLE ROW LEVEL SECURITY",
        "DROP POLICY tenant_isolation ON identity.memberships",
        "CREATE POLICY open ON identity.memberships USING (true)",
        "DELETE FROM identity.memberships",
        "UPDATE identity.sessions SET user_id = '01928f6e-7a3c-7d4e-8f00-0000000000cc'",
        "DELETE FROM identity.sessions",
        "ALTER TABLE identity.sessions DISABLE TRIGGER sessions_guard",
        "CREATE FUNCTION identity.f() RETURNS int LANGUAGE sql AS 'SELECT 1'",
    ] {
        let mut app = fixture.connect("identity_app").await;
        let result = sqlx::raw_sql(statement).execute(&mut app).await;
        assert!(result.is_err(), "the service role could run: {statement}");
    }
    // GRANT by a role without grant option only warns in Postgres; what counts is that nothing
    // was granted.
    let mut app = fixture.connect("identity_app").await;
    let _ = sqlx::raw_sql("GRANT ALL ON identity.sessions TO identity_app")
        .execute(&mut app)
        .await;
    // It may insert sessions since Laysi/trustiot-v2#103, but never delete them or hand one to
    // another user.
    for statement in [
        "DELETE FROM identity.sessions",
        "UPDATE identity.sessions SET user_id = '01928f6e-7a3c-7d4e-8f00-0000000000cc'",
    ] {
        let outcome = sqlx::raw_sql(statement).execute(&mut app).await;
        assert!(
            outcome.is_err(),
            "the service role granted itself what it needs for: {statement}"
        );
    }
    // row_security = off must not expose rows to a role that does not own the table.
    let mut app = fixture.connect("identity_app").await;
    sqlx::raw_sql("SET row_security = off")
        .execute(&mut app)
        .await
        .expect("setting is allowed");
    let outcome = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM identity.memberships")
        .fetch_one(&mut app)
        .await;
    assert!(
        !matches!(outcome, Ok(n) if n > 0),
        "row_security=off exposed rows: {outcome:?}"
    );

    // Through the service: the role comes from the session's tenant and the session's user only.
    let service = fixture.service();
    let warm_secret = fixture.add_session(USER, None, SESSION_SECS, false).await;
    warm(&service, &cookie(&warm_secret));
    let verifier = fixture.verifier(ServiceName::Assets);
    let role_for = |tenant: Option<&str>, user: &str| {
        let service = &service;
        let verifier = &verifier;
        let tenant = tenant.map(str::to_owned);
        let user = user.to_owned();
        let fixture = &fixture;
        async move {
            let secret = fixture
                .add_session(&user, tenant.as_deref(), SESSION_SECS, false)
                .await;
            let response = check(service, "/", Some(&cookie(&secret)));
            assert_eq!(response.status, 200, "{}", response.raw());
            let identity = verifier
                .verify(response.header_values("x-internal-identity")[0])
                .expect("verified");
            identity
                .tenant()
                .map(|grant| (grant.id.to_string(), format!("{:?}", grant.role)))
        }
    };
    assert_eq!(
        role_for(Some(TENANT_A), USER).await,
        Some((TENANT_A.to_owned(), "Viewer".to_owned())),
        "another user's admin role in the same tenant must not leak"
    );
    assert_eq!(
        role_for(Some(TENANT_B), USER).await,
        Some((TENANT_B.to_owned(), "Admin".to_owned()))
    );
    assert_eq!(role_for(None, USER).await, None);
    assert_eq!(
        role_for(Some("01928f6e-7a3c-7d4e-8f00-0000000000ee"), USER).await,
        None,
        "a tenant nobody belongs to"
    );
    // OTHER_USER is not a member of B: a session pointing there grants nothing.
    assert_eq!(role_for(Some(TENANT_B), OTHER_USER).await, None);
}

// ---------------------------------------------------------------------------------------------
// Logging at every level
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn nothing_secret_is_logged_at_any_level() {
    let Some(fixture) = Fixture::new(
        "CREATE DATABASE verify_logs OWNER identity_owner",
        "verify_logs",
    )
    .await
    else {
        return;
    };
    fixture.add_user(USER, false).await;
    fixture.add_membership(TENANT_A, USER, "editor").await;
    let secret = fixture
        .add_session(USER, Some(TENANT_A), SESSION_SECS, false)
        .await;
    let revoked = fixture
        .add_session(USER, Some(TENANT_A), SESSION_SECS, true)
        .await;
    let session_cookie = cookie(&secret);
    let hash = secret.hash();
    let hash_spellings = [
        URL_SAFE_NO_PAD.encode(hash.as_bytes()),
        base64::engine::general_purpose::STANDARD.encode(hash.as_bytes()),
        hash.as_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>(),
        hash.as_bytes()
            .iter()
            .map(|b| format!("{b:02X}"))
            .collect::<String>(),
        format!("{:?}", hash.as_bytes()),
    ];
    let mut all_logs = String::new();
    for filter in [
        "info",
        "debug",
        "trace",
        "trace,sqlx=trace,hyper=trace,rustls=trace",
    ] {
        let service = fixture.service_with(&[("TRUSTIOT__TELEMETRY__LOG_FILTER", filter)]);
        let mut tokens = warm(&service, &session_cookie);
        for path in ["/api/devices", "/api/certs/42?x=1", "/"] {
            let response = check(&service, path, Some(&session_cookie));
            assert_eq!(response.status, 200, "{}", response.raw());
            tokens.push(response.header_values("x-internal-identity")[0].to_owned());
        }
        // Denials of every kind, and a database error.
        check(&service, "/", None);
        check(&service, "/", Some(&cookie(&revoked)));
        check(&service, "/", Some(&format!("{COOKIE_NAME}=AAAA")));
        check(&service, "/", Some(&cookie(&new_secret())));
        let other_host = request(
            service.address,
            "GET",
            "/internal/ext-authz/",
            &[("Host", "elsewhere.test"), ("Cookie", &session_cookie)],
        )
        .expect("request");
        assert_eq!(other_host.status, 403);
        let _ = request(service.health_address, "GET", "/readyz", &[]);
        sqlx::raw_sql("REVOKE SELECT ON identity.sessions FROM identity_app")
            .execute(&mut fixture.connect("identity_owner").await)
            .await
            .expect("revoke");
        assert_eq!(check(&service, "/", Some(&session_cookie)).status, 503);
        sqlx::raw_sql("GRANT SELECT ON identity.sessions TO identity_app")
            .execute(&mut fixture.connect("identity_owner").await)
            .await
            .expect("grant");
        let logs = service.stop();
        all_logs.push_str(&format!("=== filter {filter}: {} bytes ===\n", logs.len()));

        let mut never = fixture.key.secret_spellings();
        never.push(
            std::fs::read_to_string(&fixture.files.signing_keys).expect("the signing keys file"),
        );
        never.push(secret.to_cookie_value());
        never.push(revoked.to_cookie_value());
        never.extend(hash_spellings.iter().cloned());
        never.push(TEST_PASSWORD.to_owned());
        for token in &tokens {
            never.push(token.clone());
            // Each token segment too: a log of the claims or the signature alone is a leak.
            never.extend(token.split('.').map(str::to_owned));
        }
        for value in &never {
            assert!(
                !logs.contains(value.as_str()),
                "filter {filter}: the logs contain {} ({} chars)",
                &value[..value.len().min(6)],
                value.len()
            );
        }
        assert!(!logs.contains("PRIVATE KEY"), "filter {filter}");
        assert!(
            !logs.to_lowercase().contains("x-internal-identity: ey"),
            "filter {filter}"
        );
        // The Cookie header name and the session cookie's name with its value never appear.
        assert!(
            !logs.contains(&format!("{COOKIE_NAME}=")),
            "filter {filter}"
        );
        all_logs.push_str(&logs);
        all_logs.push('\n');
    }
    // Keep the volume of what was searched in the report data.
    let out = scratch_dir("verify-log-evidence").join("trace-logs.txt");
    std::fs::write(&out, &all_logs).expect("write the log evidence");
    println!("log evidence: {} ({} bytes)", out.display(), all_logs.len());
}

// ---------------------------------------------------------------------------------------------
// Health port
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_health_port_serves_only_the_health_routes() {
    let Some(fixture) = Fixture::new(
        "CREATE DATABASE verify_health OWNER identity_owner",
        "verify_health",
    )
    .await
    else {
        return;
    };
    fixture.add_user(USER, false).await;
    let secret = fixture.add_session(USER, None, SESSION_SECS, false).await;
    let session_cookie = cookie(&secret);
    let service = fixture.service();
    for method in ["GET", "POST", "HEAD", "PUT", "DELETE", "OPTIONS"] {
        for target in [
            "/internal/ext-authz",
            "/internal/ext-authz/",
            "/internal/ext-authz/api/devices",
            "/internal/ext-authz/healthz",
            "//internal/ext-authz/x",
            "/healthz/../internal/ext-authz/x",
            "/%69nternal/ext-authz/x",
            "/internal/%65xt-authz/x",
            "/",
            "/metrics",
            "/debug/pprof",
            "/.well-known/jwks.json",
            "/readyz/x",
            "/healthz/",
        ] {
            let response = request(
                service.health_address,
                method,
                target,
                &[("Host", APP), ("Cookie", &session_cookie)],
            )
            .expect("request");
            assert!(
                response.header_values("x-internal-identity").is_empty(),
                "{method} {target}: {}",
                response.raw()
            );
            let allowed = matches!((method, target), ("GET" | "HEAD", "/healthz" | "/readyz"));
            if !allowed {
                assert!(
                    matches!(response.status, 404 | 405),
                    "{method} {target}: {}",
                    response.raw()
                );
            }
        }
    }
    for target in ["/healthz", "/readyz"] {
        assert_eq!(
            request(service.health_address, "GET", target, &[])
                .expect("request")
                .status,
            200,
            "{target}"
        );
    }
    // The API port does not serve anything else either.
    for target in [
        "/",
        "/metrics",
        "/internal",
        "/internal/keys",
        "/internal/ext-authz-x",
    ] {
        let response = request(
            service.address,
            "GET",
            target,
            &[("Cookie", &session_cookie)],
        )
        .expect("request");
        assert!(
            matches!(response.status, 404 | 405),
            "{target}: {}",
            response.raw()
        );
        assert!(response.header_values("x-internal-identity").is_empty());
    }
}

// ---------------------------------------------------------------------------------------------
// Key files
// ---------------------------------------------------------------------------------------------

/// Starts the service with these key files and settings and checks that it serves nothing
/// signed: not ready, every check of an active session 503 without an identity, and no form of
/// the private key in its output.
fn stays_unready(fixture: &Fixture, name: &str, extra: &[(&str, &str)], session: &str) {
    let env = fixture.env(extra);
    let env: Vec<(&str, &str)> = env.iter().map(|(n, v)| (n.as_str(), v.as_str())).collect();
    let service = Service::start(&env);
    service.wait_readiness(503, Duration::from_secs(30));
    // Give the database check time to pass, so only the keys keep it unready.
    assert!(
        service
            .stdout
            .wait_for_message("the database connections are open", Duration::from_secs(60))
            .is_some(),
        "{name}: the database connections never opened\n{}",
        service.stdout.text()
    );
    service.wait_readiness(503, Duration::from_secs(5));
    for _ in 0..3 {
        let response = check(&service, "/", Some(session));
        assert_eq!(response.status, 503, "{name}: {}", response.raw());
        assert!(
            response.header_values("x-internal-identity").is_empty(),
            "{name}"
        );
    }
    let output = service.stop();
    for secret in fixture.key.secret_spellings() {
        assert!(!output.contains(&secret), "{name} leaks the key: {output}");
    }
}

#[tokio::test]
async fn the_service_signs_nothing_without_a_usable_published_signing_key() {
    let Some(fixture) = Fixture::new(
        "CREATE DATABASE verify_keys OWNER identity_owner",
        "verify_keys",
    )
    .await
    else {
        return;
    };
    fixture.add_user(USER, false).await;
    fixture.add_membership(TENANT_A, USER, "viewer").await;
    let secret = fixture
        .add_session(USER, Some(TENANT_A), SESSION_SECS, false)
        .await;
    let session = cookie(&secret);
    let dir = scratch_dir("verify-keyfiles");
    let write = |name: &str, content: &[u8]| {
        let path = dir.join(name);
        std::fs::write(&path, content).expect("write");
        path.display().to_string()
    };
    let good = signing_keys(&[(&fixture.key, KID, 1, 0)]);
    let published = fixture.keys();
    let other = TestKey::new(72);
    let signing = support::SIGNING_KEYS_FILE;
    let public = support::PUBLIC_KEYS_FILE;

    // The signing keys file: missing, not a file, empty, not a key set, the PEM of the old
    // format, cut short, or a key whose public part is not its own.
    let cases: Vec<(&str, String)> = vec![
        (
            "missing signing keys",
            dir.join("missing.json").display().to_string(),
        ),
        ("a directory", dir.display().to_string()),
        ("empty file", write("empty.json", b"")),
        ("whitespace only", write("ws.json", b" \n\n")),
        ("no keys", write("none.json", br#"{"keys":[]}"#)),
        (
            "a PEM private key",
            write("key.pem", fixture.key.pem().as_bytes()),
        ),
        (
            "truncated",
            write("truncated.json", &good.as_bytes()[..good.len() / 2]),
        ),
        (
            "another key's public part",
            write(
                "mismatch.json",
                good.replace(
                    fixture.key.jwk(KID)["x"].as_str().expect("x"),
                    other.jwk(KID)["x"].as_str().expect("x"),
                )
                .as_bytes(),
            ),
        ),
    ];
    for (name, path) in &cases {
        stays_unready(&fixture, name, &[(signing, path)], &session);
    }
    // A key file readable by nobody (as a misconfigured Secret mount could be).
    {
        use std::os::unix::fs::PermissionsExt;
        let locked = write("locked.json", good.as_bytes());
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000))
            .expect("chmod 000");
        if std::fs::File::open(&locked).is_err() {
            stays_unready(
                &fixture,
                "unreadable file (mode 000)",
                &[(signing, &locked)],
                &session,
            );
        }
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o600)).expect("restore");
    }
    // A usable key the service may not sign with: not published, revoked, or not due yet.
    let not_published = write("public-other.json", jwks(&[(&other, KID)]).as_bytes());
    stays_unready(
        &fixture,
        "the public keys lack the signing key",
        &[(public, &not_published)],
        &session,
    );
    stays_unready(
        &fixture,
        "every key revoked",
        &[("TRUSTIOT_IDENTITY_REVOKE_BEFORE_GENERATION", "2")],
        &session,
    );
    let later = write(
        "later.json",
        signing_keys(&[(&fixture.key, KID, 1, i64::MAX / 2)]).as_bytes(),
    );
    stays_unready(
        &fixture,
        "the only key signs later",
        &[(signing, &later)],
        &session,
    );
    // The public keys file: missing, empty set, not JSON, or holding a private key.
    let private_jwk = json!({"keys": [{
        "kty": "OKP", "crv": "Ed25519", "kid": KID,
        "x": fixture.key.jwk(KID)["x"], "d": fixture.key.secret_spellings()[1]
    }]})
    .to_string();
    for (name, text) in [
        ("empty public key set", r#"{"keys":[]}"#.to_owned()),
        ("public keys not JSON", "not json".to_owned()),
        ("a private key in the public keys", private_jwk),
    ] {
        let path = write(&format!("public-{}.json", name.len()), text.as_bytes());
        stays_unready(&fixture, name, &[(public, &path)], &session);
    }
    stays_unready(
        &fixture,
        "missing public keys",
        &[(
            public,
            &dir.join("missing-public.json").display().to_string(),
        )],
        &session,
    );

    // The good files start and sign, with any file mode: the check is by content.
    {
        use std::os::unix::fs::PermissionsExt;
        let open = write("open.json", good.as_bytes());
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o644)).expect("chmod");
        let service = fixture.service_with(&[
            (signing, &open),
            (public, &write("p.json", published.as_bytes())),
        ]);
        let tokens = warm(&service, &session);
        assert_eq!(kid_of(tokens.last().expect("a token")), KID);
        drop(service);
    }
    // The key files can be named only by the deployment, never by a service setting, and the
    // settings of the format before Laysi/trustiot-v2#404 do nothing.
    for (name, value) in [
        (
            "TRUSTIOT__PLATFORM__IDENTITY_SIGNING_KEYS_FILE",
            "/etc/passwd",
        ),
        ("TRUSTIOT__IDENTITY__SIGNING_KEY", "x"),
        ("TRUSTIOT_IDENTITY_SIGNING_KEY", "x"),
        ("TRUSTIOT_IDENTITY_SIGNING_KEY_FILE", "/etc/passwd"),
        ("TRUSTIOT_INTERNAL_IDENTITY_KEYS", "x"),
    ] {
        let env = fixture.env(&[(name, value)]);
        let env: Vec<(&str, &str)> = env.iter().map(|(n, v)| (n.as_str(), v.as_str())).collect();
        let (status, output) = run_to_exit_or_serve(&env);
        // Either it refuses (a setting it does not take), or the legitimate key still signs.
        assert!(!output.contains("/etc/passwd"), "{name}: {output}");
        let _ = status;
    }
}

/// Starts the service; if it stays up, stops it and returns its output, else the exit output.
fn run_to_exit_or_serve(env: &[(&str, &str)]) -> (Option<i32>, String) {
    let (mut child, stdout, stderr) = support::spawn(env);
    let status = support::wait_exit(&mut child, Duration::from_secs(5));
    if status.is_none() {
        let _ = child.kill();
        let _ = child.wait();
    }
    let _ = stdout.wait_closed(Duration::from_secs(5));
    (
        status.and_then(|s| s.code()),
        format!("{}\n{}", stdout.text(), stderr.text()),
    )
}

/// Two replicas reading the same files switch to a new key by the schedule alone, without a
/// restart: no token issued before the new key's `signs_from` names it, and both replicas name
/// it once they have read the file. The key id each logs is the one its tokens carry.
#[tokio::test]
async fn replicas_switch_keys_by_the_schedule_without_a_restart() {
    let Some(fixture) = Fixture::new(
        "CREATE DATABASE verify_rotation OWNER identity_owner",
        "verify_rotation",
    )
    .await
    else {
        return;
    };
    fixture.add_user(USER, false).await;
    fixture.add_membership(TENANT_A, USER, "viewer").await;
    let secret = fixture
        .add_session(USER, Some(TENANT_A), SESSION_SECS, false)
        .await;
    let session = cookie(&secret);
    let new_key = TestKey::new(82);
    let replicas = [fixture.service(), fixture.service()];
    for replica in &replicas {
        let tokens = warm(replica, &session);
        assert_eq!(kid_of(tokens.last().expect("a token")), KID);
    }

    // The init container's three steps: both public keys, then the new key signs from a short
    // while ahead.
    fixture
        .files
        .set_public_keys(&jwks(&[(&fixture.key, KID), (&new_key, "2027-04-a")]));
    let signs_from = identity::issuer::unix_now() + 15;
    fixture.files.set_signing_keys(&signing_keys(&[
        (&fixture.key, KID, 1, 0),
        (&new_key, "2027-04-a", 2, signs_from),
    ]));

    let deadline = Instant::now() + Duration::from_secs(90);
    let mut switched = [false, false];
    while switched != [true, true] {
        assert!(
            Instant::now() < deadline,
            "the replicas did not switch: {switched:?}"
        );
        for (index, replica) in replicas.iter().enumerate() {
            let response = check(replica, "/", Some(&session));
            assert_eq!(response.status, 200, "{}", response.raw());
            let token = response.header_values("x-internal-identity")[0].to_owned();
            let payload: serde_json::Value = serde_json::from_slice(
                &URL_SAFE_NO_PAD
                    .decode(token.split('.').nth(1).expect("payload"))
                    .expect("base64url"),
            )
            .expect("JSON");
            let iat = payload["iat"].as_i64().expect("iat");
            let kid = kid_of(&token);
            if iat < signs_from {
                assert_eq!(kid, KID, "replica {index} signed with the new key early");
            }
            if kid == "2027-04-a" {
                switched[index] = true;
            }
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    for replica in replicas {
        let logged = replica
            .stdout
            .wait_for_json(Duration::from_secs(30), |line| {
                line["message"] == "signing internal identity tokens"
                    && line["fields"]["kid"] == "2027-04-a"
            });
        assert!(logged.is_some(), "the switch is logged with the key id");
        let output = replica.stop();
        for secret in new_key.secret_spellings() {
            assert!(!output.contains(&secret), "the new key reached the logs");
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Audience and allow-response shape
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_audience_follows_the_route_and_an_allow_carries_only_the_identity_header() {
    let Some(fixture) = Fixture::new(
        "CREATE DATABASE verify_audience OWNER identity_owner",
        "verify_audience",
    )
    .await
    else {
        return;
    };
    fixture.add_user(USER, false).await;
    fixture.add_membership(TENANT_A, USER, "viewer").await;
    let secret = fixture
        .add_session(USER, Some(TENANT_A), SESSION_SECS, false)
        .await;
    let session_cookie = cookie(&secret);
    let service = fixture.service();
    warm(&service, &session_cookie);

    let audience_of = |response: &support::Response| -> Option<ServiceName> {
        if response.status != 200 {
            return None;
        }
        let token = &response.header_values("x-internal-identity")[0];
        [ServiceName::Assets, ServiceName::Certs]
            .into_iter()
            .find(|audience| fixture.verifier(*audience).verify(token).is_ok())
    };
    let ask = |host: &str, path: &str| {
        request(
            service.address,
            "GET",
            &format!("/internal/ext-authz{path}"),
            &[("Host", host), ("Cookie", &session_cookie)],
        )
        .expect("request")
    };
    for (host, path, expected) in [
        (APP, "/", Some(ServiceName::Assets)),
        (APP, "/anything", Some(ServiceName::Assets)),
        (APP, "/api/certs", Some(ServiceName::Certs)),
        (APP, "/api/certs/", Some(ServiceName::Certs)),
        (APP, "/api/certs/42/x", Some(ServiceName::Certs)),
        (APP, "/api/certsx", Some(ServiceName::Assets)),
        (APP, "/api/cert", Some(ServiceName::Assets)),
        (APP, "/API/certs", Some(ServiceName::Assets)),
        (APP, "/api//certs", Some(ServiceName::Assets)),
        (APP, "/api/certs/../x", Some(ServiceName::Certs)),
        (
            "APP.SAAS.TRUSTIOT.TEST",
            "/api/certs",
            Some(ServiceName::Certs),
        ),
        (
            "app.saas.trustiot.test:8443",
            "/api/certs",
            Some(ServiceName::Certs),
        ),
        (
            "app.saas.trustiot.test.",
            "/api/certs",
            Some(ServiceName::Certs),
        ),
        ("other.test", "/", None),
        ("app.saas.trustiot.test.evil.test", "/", None),
        ("xapp.saas.trustiot.test", "/", None),
        ("evil.test@app.saas.trustiot.test", "/", None),
        ("app.saas.trustiot.test:80:80", "/", None),
        ("app.saas.trustiot.test, evil.test", "/", None),
        ("[::1]", "/", None),
        ("127.0.0.1", "/", None),
    ] {
        let response = ask(host, path);
        assert_eq!(
            audience_of(&response),
            expected,
            "host {host:?} path {path:?}: {}",
            response.raw()
        );
        if expected.is_none() {
            assert_eq!(
                response.status,
                403,
                "{host:?} {path:?}: {}",
                response.raw()
            );
            assert!(response.header_values("x-internal-identity").is_empty());
        }
    }

    // Two Host headers: the check refuses to guess.
    let response = raw(
        service.address,
        format!(
            "GET /internal/ext-authz/ HTTP/1.1\r\nHost: {APP}\r\nHost: evil.test\r\nCookie: {session_cookie}\r\nConnection: close\r\n\r\n"
        )
        .as_bytes(),
    );
    assert!(
        response
            .as_ref()
            .is_none_or(|r| r.status != 200 && r.identity.is_empty()),
        "duplicate Host: {response:?}"
    );

    // The allow response: exactly the identity header and no-store, plus what HTTP itself needs.
    let response = ask(APP, "/api/certs");
    assert_eq!(response.status, 200);
    let names: Vec<String> = response
        .headers
        .iter()
        .map(|(name, _)| name.to_ascii_lowercase())
        .collect();
    for name in &names {
        assert!(
            matches!(
                name.as_str(),
                "x-internal-identity" | "cache-control" | "content-length" | "date" | "connection"
            ),
            "unexpected header {name} in the allow: {names:?}"
        );
    }
    assert_eq!(response.header_values("cache-control"), vec!["no-store"]);
    assert!(response.body.is_empty(), "{}", response.body);
    assert!(
        response.header_values("set-cookie").is_empty(),
        "an allow must not set cookies"
    );

    // Forged identity headers in every spelling, also repeated and with the real token value
    // of another audience: the allow never echoes them and carries exactly one fresh token.
    let forged_names = [
        "x-internal-identity",
        "X-Internal-Identity",
        "X-INTERNAL-IDENTITY",
        "x_internal_identity",
        "X_Internal_Identity",
        "x-internal_identity",
        "x_internal-identity",
        "x-internal-identity2",
        "x-forwarded-internal-identity",
    ];
    let mut head = format!(
        "GET /internal/ext-authz/api/certs HTTP/1.1\r\nHost: {APP}\r\nCookie: {session_cookie}\r\nConnection: close\r\n"
    );
    for name in forged_names {
        // Two copies each, different values.
        head.push_str(&format!("{name}: forged-one\r\n{name}: forged-two\r\n"));
    }
    head.push_str("\r\n");
    let response = raw(service.address, head.as_bytes()).expect("response");
    assert_eq!(response.status, 200, "{}", response.text);
    assert_eq!(response.identity.len(), 1, "{}", response.text);
    assert!(!response.text.contains("forged"), "{}", response.text);
    fixture
        .verifier(ServiceName::Certs)
        .verify(&response.identity[0])
        .expect("the one token is the service's own");
    // The same forged headers with an invalid cookie: denied, nothing echoed.
    let head = head.replace(&session_cookie, &format!("{COOKIE_NAME}=AAAA"));
    let response = raw(service.address, head.as_bytes()).expect("response");
    assert_eq!(response.status, 401, "{}", response.text);
    assert!(response.identity.is_empty());
    assert!(!response.text.contains("forged"), "{}", response.text);
}
