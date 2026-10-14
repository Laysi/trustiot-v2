//! The identity service end to end: the real binary, the migration in
//! `migrations/postgres`, and a real Postgres with TLS (podman or docker; skipped without either
//! unless `CI` is set). Each test works in its own database and starts its own service, which
//! connects as the service role `identity_app` with service-kit's session check, so a schema that
//! row-level security would not protect stops the service from becoming ready.
//!
//! Sessions are created here, by the owner role, as the MS0 test tooling: the service itself has
//! no way to create one (Laysi/trustiot-v2#61).

mod support;

use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use contract::{ServiceName, TenantRole};
use identity::session::{SessionSecret, WARM_CONNECTIONS};
use serde_json::json;
use service_kit::identity::{IdentityError, IdentityKeys, IdentityVerifier, UserIdentity};
use sqlx::{Connection, PgConnection};
use support::postgres::{SERVER_HOST, TEST_PASSWORD, TlsPostgres, tls_postgres};
use support::{
    Service, SlowProxy, TestKey, cookie, jwks, new_secret, request, run_to_exit, scratch_dir,
};

/// The roles of decision 29 for this service, created once per Postgres. The migration Job
/// (Laysi/trustiot-v2#60) creates them in a deployment.
const ROLES: &str = "
CREATE ROLE identity_owner LOGIN PASSWORD 'service-kit-test';
CREATE ROLE identity_app LOGIN PASSWORD 'service-kit-test';
";

const MIGRATION: &str = include_str!("../migrations/postgres/0001_identity.sql");
/// The migration Job creates the service's schema, owned by the owner role, before it runs the
/// migrations (tools/migrate); the tests do the same as the owner.
const SCHEMA: &str = "CREATE SCHEMA identity";
const DATABASE_TEMPLATE: &str = include_str!("../../../crates/service-kit/sql/database.sql");

const APP: &str = "app.saas.trustiot.test";
const TENANT_A: &str = "01928f6e-7a3c-7d4e-8f00-00000000000a";
const TENANT_B: &str = "01928f6e-7a3c-7d4e-8f00-00000000000b";
const USER: &str = "01928f6e-7a3c-7d4e-8f00-0123456789ab";
const ADMIN: &str = "01928f6e-7a3c-7d4e-8f00-0000000000ad";
const KID: &str = "2026-10-a";

const INSERT_USER: &str = "INSERT INTO identity.users (id, platform_admin) VALUES ($1::uuid, $2)";
const INSERT_MEMBERSHIP: &str = "INSERT INTO identity.memberships (tenant_id, user_id, tenant_role)
VALUES ($1::uuid, $2::uuid, $3)";
/// A session that started `$5` seconds ago and expires in `$4` seconds (negative: expired).
const INSERT_SESSION: &str = "INSERT INTO identity.sessions
    (id, token_hash, user_id, active_tenant_id, created_at, expires_at, revoked_at)
VALUES (gen_random_uuid(), $1, $2::uuid, $3::uuid,
    now() - make_interval(secs => $5), now() + make_interval(secs => $4),
    CASE WHEN $6 THEN now() ELSE NULL END)";
const UPDATE_ROLE: &str = "UPDATE identity.memberships SET tenant_role = $3
WHERE tenant_id = $1::uuid AND user_id = $2::uuid";
const DELETE_MEMBERSHIP: &str =
    "DELETE FROM identity.memberships WHERE tenant_id = $1::uuid AND user_id = $2::uuid";
const DELETE_USER: &str = "DELETE FROM identity.users WHERE id = $1::uuid";

/// Environment variables for the service.
type Env = Vec<(String, String)>;

/// SQLSTATE `insufficient_privilege`.
const INSUFFICIENT_PRIVILEGE: &str = "42501";

/// Twelve hours, the session lifetime of checklist A1.
const SESSION_SECS: f64 = 12.0 * 3600.0;

/// One test's database with the migration applied, the signing key and CA files, and a
/// running service.
struct Fixture {
    postgres: Arc<TlsPostgres>,
    database: &'static str,
    key: TestKey,
    key_file: PathBuf,
    ca_file: PathBuf,
}

impl Fixture {
    /// Creates `database` with `create` (a static `CREATE DATABASE ... OWNER identity_owner`) and
    /// applies the database template and the migration as the owner.
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
        let key = TestKey::new(61);
        let key_file = dir.join("signing-key.pem");
        std::fs::write(&key_file, key.pem()).expect("write the signing key");
        let ca_file = dir.join("ca.crt");
        std::fs::write(&ca_file, &postgres.ca_pem).expect("write the CA");
        let fixture = Self {
            postgres,
            database,
            key,
            key_file,
            ca_file,
        };
        let mut owner = fixture.connect("identity_owner").await;
        sqlx::raw_sql(DATABASE_TEMPLATE)
            .execute(&mut owner)
            .await
            .expect("run the database template");
        sqlx::raw_sql(SCHEMA)
            .execute(&mut owner)
            .await
            .expect("create the schema");
        sqlx::raw_sql(MIGRATION)
            .execute(&mut owner)
            .await
            .expect("run the migration");
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

    /// Every setting the chart gives the service, pointing at this database.
    fn env(&self) -> Env {
        let routes = json!([
            {"host": APP, "path": "/", "match": "PathPrefix", "service": "assets"},
            {"host": APP, "path": "/api/certs", "match": "PathPrefix", "service": "certs"},
        ]);
        vec![
            ("TRUSTIOT_INTERNAL_IDENTITY_KEYS".into(), self.keys()),
            (
                "TRUSTIOT_IDENTITY_SIGNING_KEY_FILE".into(),
                self.key_file.display().to_string(),
            ),
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
        ]
    }

    /// Starts the service and waits until it is ready, which needs a session that passed
    /// service-kit's row-level security check.
    fn service(&self) -> Service {
        let env = self.env();
        let env: Vec<(&str, &str)> = env.iter().map(|(n, v)| (n.as_str(), v.as_str())).collect();
        let service = Service::start(&env);
        service.wait_ready(Duration::from_secs(30));
        service
    }

    async fn add_user(&self, user: &str, platform_admin: bool) {
        sqlx::query(INSERT_USER)
            .bind(user)
            .bind(platform_admin)
            .execute(&mut self.connect("identity_owner").await)
            .await
            .expect("insert the user");
    }

    async fn add_membership(&self, tenant: &str, user: &str, role: &str) {
        sqlx::query(INSERT_MEMBERSHIP)
            .bind(tenant)
            .bind(user)
            .bind(role)
            .execute(&mut self.connect("identity_owner").await)
            .await
            .expect("insert the membership");
    }

    /// A session as the test tooling creates it: a fresh secret, of which only the hash is
    /// stored.
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

    async fn owner_execute(&self, sql: &'static str, binds: &[&str]) {
        let mut query = sqlx::query(sql);
        for value in binds {
            query = query.bind(*value);
        }
        query
            .execute(&mut self.connect("identity_owner").await)
            .await
            .expect("owner statement");
    }

    fn verifier(&self, audience: ServiceName) -> IdentityVerifier {
        IdentityVerifier::new(
            audience,
            IdentityKeys::from_jwks(&self.keys()).expect("keys"),
        )
    }
}

/// A check request as the ingress gateway sends it.
fn check(service: &Service, path: &str, cookie: Option<&str>) -> support::Response {
    check_with(service, path, APP, cookie, &[])
}

fn check_with(
    service: &Service,
    path: &str,
    host: &str,
    cookie: Option<&str>,
    extra: &[(&str, &str)],
) -> support::Response {
    let mut headers = vec![("Host", host)];
    if let Some(cookie) = cookie {
        headers.push(("Cookie", cookie));
    }
    headers.extend_from_slice(extra);
    request(
        service.address,
        "GET",
        &format!("/internal/ext-authz{path}"),
        &headers,
    )
    .expect("check request")
}

/// The one identity header of an allow, verified for `audience`.
fn verified(
    fixture: &Fixture,
    response: &support::Response,
    audience: ServiceName,
) -> UserIdentity {
    assert_eq!(response.status, 200, "{}", response.raw());
    let tokens = response.header_values("x-internal-identity");
    let [token] = tokens.as_slice() else {
        panic!("expected exactly one identity header: {}", response.raw());
    };
    fixture
        .verifier(audience)
        .verify(token)
        .unwrap_or_else(|error| panic!("{audience} refuses the token: {error}"))
}

fn assert_unauthenticated(response: &support::Response) {
    assert_eq!(response.status, 401, "{}", response.raw());
    assert!(response.header_values("x-internal-identity").is_empty());
    assert_eq!(
        response.json(),
        json!({"code": "unauthenticated", "message_key": "error.unauthenticated"})
    );
}

/// Acceptance: a valid session's check carries one identity header that passes
/// Laysi/trustiot-v2#52's verification, with the session's user, tenant, role and admin flag,
/// addressed to the routed service; a forged header in the request changes nothing.
#[tokio::test]
async fn an_active_session_gets_one_token_that_the_routed_service_accepts() {
    let Some(fixture) = Fixture::new(
        "CREATE DATABASE identity_allow OWNER identity_owner",
        "identity_allow",
    )
    .await
    else {
        return;
    };
    fixture.add_user(USER, true).await;
    fixture.add_membership(TENANT_A, USER, "editor").await;
    // The same user is an admin elsewhere: only the session's tenant counts.
    fixture.add_membership(TENANT_B, USER, "admin").await;
    let secret = fixture
        .add_session(USER, Some(TENANT_A), SESSION_SECS, false)
        .await;
    let service = fixture.service();
    let cookie = cookie(&secret);

    let user = verified(
        &fixture,
        &check(&service, "/api/devices", Some(&cookie)),
        ServiceName::Assets,
    );
    assert_eq!(user.user_id().to_string(), USER);
    assert_eq!(
        user.tenant_id().map(|t| t.to_string()).as_deref(),
        Some(TENANT_A)
    );
    assert_eq!(user.tenant().map(|t| t.role), Some(TenantRole::Editor));
    assert!(user.is_platform_admin());
    assert_eq!(user.claims().exp - user.claims().iat, 60);

    let response = check(&service, "/api/certs/42?x=1", Some(&cookie));
    verified(&fixture, &response, ServiceName::Certs);
    let token = response.header_values("x-internal-identity")[0].to_owned();
    assert_eq!(
        fixture.verifier(ServiceName::Assets).verify(&token).err(),
        Some(IdentityError::WrongAudience),
        "a token for certs is refused by assets"
    );

    // Forged identity headers, in every spelling, do not change the allow.
    let forged = [
        ("x-internal-identity", token.as_str()),
        ("x-internal-identity", "forged"),
        ("x_internal_identity", "forged"),
        ("x-internal_identity", "forged"),
        ("x_internal-identity", "forged"),
    ];
    let response = check_with(&service, "/api/devices", APP, Some(&cookie), &forged);
    let user = verified(&fixture, &response, ServiceName::Assets);
    assert_eq!(user.user_id().to_string(), USER);
    assert_ne!(response.header_values("x-internal-identity")[0], token);

    // A valid session for a request no cookie route serves.
    let response = check_with(&service, "/", "other.trustiot.test", Some(&cookie), &[]);
    assert_eq!(response.status, 403, "{}", response.raw());
    assert!(response.header_values("x-internal-identity").is_empty());
}

/// Acceptance: missing, malformed, unknown, expired and revoked sessions get 401.
#[tokio::test]
async fn invalid_or_expired_sessions_get_401() {
    let Some(fixture) = Fixture::new(
        "CREATE DATABASE identity_deny OWNER identity_owner",
        "identity_deny",
    )
    .await
    else {
        return;
    };
    fixture.add_user(USER, false).await;
    fixture.add_membership(TENANT_A, USER, "viewer").await;
    let expired = fixture.add_session(USER, Some(TENANT_A), -1.0, false).await;
    let revoked = fixture
        .add_session(USER, Some(TENANT_A), SESSION_SECS, true)
        .await;
    let deleted_user = fixture
        .add_session(USER, Some(TENANT_A), SESSION_SECS, false)
        .await;
    let service = fixture.service();

    for cookie in [
        None,
        Some("theme=dark".to_owned()),
        Some("__Host-trustiot-session=not-a-session".to_owned()),
        Some(cookie(&new_secret())),
        Some(cookie(&expired)),
        Some(cookie(&revoked)),
    ] {
        assert_unauthenticated(&check(&service, "/api/devices", cookie.as_deref()));
    }

    // A session ends with its user.
    let still = cookie(&deleted_user);
    verified(
        &fixture,
        &check(&service, "/", Some(&still)),
        ServiceName::Assets,
    );
    fixture.owner_execute(DELETE_USER, &[USER]).await;
    assert_unauthenticated(&check(&service, "/", Some(&still)));
}

/// Decision 13: role changes and removed memberships apply to the next request of the same
/// session; a session without a tenant gets a token without one.
#[tokio::test]
async fn role_changes_apply_to_the_next_request() {
    let Some(fixture) = Fixture::new(
        "CREATE DATABASE identity_roles OWNER identity_owner",
        "identity_roles",
    )
    .await
    else {
        return;
    };
    fixture.add_user(USER, false).await;
    fixture.add_user(ADMIN, true).await;
    fixture.add_membership(TENANT_A, USER, "viewer").await;
    let session = cookie(
        &fixture
            .add_session(USER, Some(TENANT_A), SESSION_SECS, false)
            .await,
    );
    let no_tenant = cookie(&fixture.add_session(ADMIN, None, SESSION_SECS, false).await);
    let not_a_member = cookie(
        &fixture
            .add_session(ADMIN, Some(TENANT_B), SESSION_SECS, false)
            .await,
    );
    let service = fixture.service();
    let role = |response: &support::Response| {
        verified(&fixture, response, ServiceName::Assets)
            .tenant()
            .map(|grant| grant.role)
    };

    assert_eq!(
        role(&check(&service, "/", Some(&session))),
        Some(TenantRole::Viewer)
    );
    fixture
        .owner_execute(UPDATE_ROLE, &[TENANT_A, USER, "admin"])
        .await;
    assert_eq!(
        role(&check(&service, "/", Some(&session))),
        Some(TenantRole::Admin)
    );
    fixture
        .owner_execute(DELETE_MEMBERSHIP, &[TENANT_A, USER])
        .await;
    assert_eq!(role(&check(&service, "/", Some(&session))), None);

    let admin = verified(
        &fixture,
        &check(&service, "/", Some(&no_tenant)),
        ServiceName::Assets,
    );
    assert_eq!(admin.tenant(), None);
    assert!(admin.is_platform_admin());
    let admin = verified(
        &fixture,
        &check(&service, "/", Some(&not_a_member)),
        ServiceName::Assets,
    );
    assert_eq!(
        admin.tenant(),
        None,
        "a tenant the user is not a member of is not granted"
    );
}

/// MS0 has no login: the service role can read sessions but not create, extend or revoke them,
/// and memberships are invisible without a tenant.
#[tokio::test]
async fn the_service_role_cannot_create_or_change_sessions() {
    let Some(fixture) = Fixture::new(
        "CREATE DATABASE identity_grants OWNER identity_owner",
        "identity_grants",
    )
    .await
    else {
        return;
    };
    fixture.add_user(USER, false).await;
    fixture.add_membership(TENANT_A, USER, "viewer").await;
    let mut app = fixture.connect("identity_app").await;
    for statement in [
        "INSERT INTO identity.sessions (id, token_hash, user_id, expires_at) VALUES (gen_random_uuid(), sha256('x'), '01928f6e-7a3c-7d4e-8f00-0123456789ab', now() + interval '1 hour')",
        "UPDATE identity.sessions SET expires_at = now() + interval '1 year'",
        "UPDATE identity.sessions SET revoked_at = NULL",
        "INSERT INTO identity.users (id) VALUES (gen_random_uuid())",
        "UPDATE identity.users SET platform_admin = true",
        "INSERT INTO identity.memberships (tenant_id, user_id, tenant_role) VALUES ('01928f6e-7a3c-7d4e-8f00-00000000000a', '01928f6e-7a3c-7d4e-8f00-0123456789ab', 'admin')",
        "TRUNCATE identity.sessions",
        "CREATE TABLE identity.extra (id int)",
    ] {
        let error = sqlx::raw_sql(statement)
            .execute(&mut app)
            .await
            .expect_err(statement);
        let code = error
            .as_database_error()
            .and_then(|error| error.code())
            .map(|code| code.into_owned());
        assert_eq!(
            code.as_deref(),
            Some(INSUFFICIENT_PRIVILEGE),
            "{statement}: {error}"
        );
    }
    let visible: i64 = sqlx::query_scalar("SELECT count(*) FROM identity.memberships")
        .fetch_one(&mut app)
        .await
        .expect("count");
    assert_eq!(
        visible, 0,
        "row-level security hides memberships without a tenant"
    );
}

/// Acceptance: the private key never appears in logs or in any response; neither do session
/// cookies, issued tokens or the database password.
#[tokio::test]
async fn secrets_never_reach_logs_or_responses() {
    let Some(fixture) = Fixture::new(
        "CREATE DATABASE identity_leaks OWNER identity_owner",
        "identity_leaks",
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
    let session_cookie = cookie(&secret);
    let service = fixture.service();

    let mut responses = Vec::new();
    for (path, host, cookie) in [
        ("/api/devices", APP, Some(session_cookie.as_str())),
        ("/api/certs", APP, Some(session_cookie.as_str())),
        ("/", "other.trustiot.test", Some(session_cookie.as_str())),
        ("/", APP, None),
        ("/", APP, Some("__Host-trustiot-session=AAAA")),
    ] {
        responses.push(check_with(&service, path, host, cookie, &[]));
    }
    for target in [
        "/healthz",
        "/readyz",
        "/",
        "/internal/keys",
        "/.well-known/jwks.json",
    ] {
        responses.push(request(service.address, "GET", target, &[]).expect("request"));
    }
    // The health port, which the node can reach, serves the health routes and nothing else:
    // not the check, so no token is ever issued there.
    for target in ["/healthz", "/readyz", "/internal/ext-authz/api/devices"] {
        let headers = [("host", APP), ("cookie", session_cookie.as_str())];
        let response = request(service.health_address, "GET", target, &headers).expect("request");
        if target.starts_with("/internal") {
            assert_eq!(response.status, 404, "{}", response.raw());
        }
        responses.push(response);
    }
    let tokens: Vec<String> = responses
        .iter()
        .flat_map(|response| response.header_values("x-internal-identity"))
        .map(str::to_owned)
        .collect();
    assert_eq!(tokens.len(), 2, "two allows");
    let logs = service.stop();

    let mut secrets = fixture.key.secret_spellings();
    secrets.push(fixture.key.pem());
    for response in &responses {
        for secret in &secrets {
            assert!(
                !response.raw().contains(secret.as_str()),
                "{}",
                response.raw()
            );
        }
        assert!(
            !response.raw().contains("PRIVATE KEY"),
            "{}",
            response.raw()
        );
    }
    let mut never_logged = secrets;
    never_logged.push(secret.to_cookie_value());
    never_logged.extend(tokens);
    never_logged.push(TEST_PASSWORD.to_owned());
    for value in &never_logged {
        assert!(
            !logs.contains(value.as_str()),
            "the logs contain a secret:\n{logs}"
        );
    }
    assert!(!logs.contains("PRIVATE KEY"), "{logs}");
    // The logs do say which key signs, by key id, and why requests were denied.
    assert!(logs.contains(KID), "{logs}");
    assert!(logs.contains("no_session_cookie"), "{logs}");
    assert!(logs.contains("no_route"), "{logs}");
}

/// When the database cannot answer, checks fail closed with 503 (the gateway's statusOnError)
/// and readiness fails.
#[tokio::test]
async fn an_unreachable_database_fails_closed() {
    let key = TestKey::new(62);
    let dir = scratch_dir("unreachable");
    let key_file = dir.join("signing-key.pem");
    std::fs::write(&key_file, key.pem()).expect("write the key");
    // A port that refuses connections.
    let port = TcpListener::bind("127.0.0.1:0")
        .and_then(|listener| listener.local_addr())
        .expect("free port")
        .port();
    let env = base_env(&jwks(&[(&key, KID)]), &key_file, port);
    let env: Vec<(&str, &str)> = env.iter().map(|(n, v)| (n.as_str(), v.as_str())).collect();
    let service = Service::start(&env);
    let started = Instant::now();
    let response = check(&service, "/", Some(&cookie(&new_secret())));
    assert_eq!(response.status, 503, "{}", response.raw());
    // Bounded by the check deadline (0.4 s; src/ext_authz/tests.rs proves it in-process), with
    // room for a loaded test machine.
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    assert!(response.header_values("x-internal-identity").is_empty());
    assert_eq!(
        response.json(),
        json!({"code": "unavailable", "message_key": "error.unavailable"})
    );
    for address in [service.health_address, service.address] {
        let ready = request(address, "GET", "/readyz", &[]).expect("readyz");
        assert_eq!(ready.status, 503);
    }
    // Requests without a usable cookie never need the database.
    assert_unauthenticated(&check(&service, "/", None));
}

/// How long a test waits for the service to become ready, or for readiness or a log line to
/// change: several keep-warm rounds (2 s each) plus 1 s connections, on a loaded machine.
const READINESS_BOUND: Duration = Duration::from_secs(30);

/// The keep-warm task's log lines when readiness changes (src/session.rs).
const READY_LOG: &str = "the database connections are open";
const NOT_READY_LOG: &str = "cannot keep the database connections open; not ready";

/// A fixture with one viewer session, and the service connecting through a proxy that holds
/// every new connection for 1 s (longer than a check may take), started and ready.
async fn slow_connect_service(
    create: &'static str,
    database: &'static str,
) -> Option<(Fixture, SlowProxy, Service, String)> {
    let fixture = Fixture::new(create, database).await?;
    fixture.add_user(USER, false).await;
    fixture.add_membership(TENANT_A, USER, "viewer").await;
    let session = cookie(
        &fixture
            .add_session(USER, Some(TENANT_A), SESSION_SECS, false)
            .await,
    );
    let proxy = SlowProxy::start(fixture.postgres.address, Duration::from_secs(1));
    let env = with(
        fixture.env(),
        "TRUSTIOT_POSTGRES_PORT",
        &proxy.port.to_string(),
    );
    let env: Vec<(&str, &str)> = env.iter().map(|(n, v)| (n.as_str(), v.as_str())).collect();
    let service = Service::start(&env);
    service.wait_ready(READINESS_BOUND);
    Some((fixture, proxy, service, session))
}

/// The keep-warm task's readiness log lines so far, for the test output.
fn readiness_log(service: &Service) -> String {
    service
        .stdout
        .lines()
        .iter()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|value| value["message"] == READY_LOG || value["message"] == NOT_READY_LOG)
        .map(|value| {
            format!(
                "  {} {} {}",
                value["timestamp"], value["message"], value["fields"]["error"]
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Sends `count` checks for `session`, 50 ms apart: every one must be allowed with a token the
/// routed service accepts, and none may make the service open a database connection.
fn assert_checks_use_open_connections(
    fixture: &Fixture,
    proxy: &SlowProxy,
    service: &Service,
    session: &str,
    count: usize,
    when: &str,
) {
    let opened = proxy.accepted();
    for round in 0..count {
        let response = check(service, "/", Some(session));
        assert_eq!(
            response.status,
            200,
            "check {round} {when}: {}",
            response.raw()
        );
        verified(fixture, &response, ServiceName::Assets);
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(
        proxy.accepted(),
        opened,
        "a check {when} opened a database connection"
    );
}

/// Finding F1 of docs/test-reports/2026-10-11-61-verify.md and Laysi/trustiot-v2#341: opening a
/// database connection may take longer than a check may, as on a loaded node or a slow network;
/// here a proxy holds every new connection for 1 s. The service becomes ready only with its
/// connections open, and checks after that never wait for a new one. When the database drops
/// every connection (a failover), checks fail closed, the keep-warm task finds the dropped
/// connections on its own and readiness fails; once readiness passes again, every check is
/// answered from open connections.
#[tokio::test]
async fn a_slow_database_connect_never_fails_a_check_after_ready() {
    let Some((fixture, proxy, service, session)) = slow_connect_service(
        "CREATE DATABASE identity_slow_connect OWNER identity_owner",
        "identity_slow_connect",
    )
    .await
    else {
        return;
    };
    let opened = proxy.accepted();
    assert!(opened >= 3, "ready with {opened} connections");
    assert_checks_use_open_connections(&fixture, &proxy, &service, &session, 20, "after ready");

    // A failover: every connection is dropped, and no new one opens while the proxy is paused,
    // so whatever the service does meanwhile, it has no open connection.
    proxy.pause();
    proxy.cut();
    let response = check(&service, "/", Some(&session));
    assert_eq!(response.status, 503, "after the cut: {}", response.raw());
    assert!(response.header_values("x-internal-identity").is_empty());
    assert_eq!(
        response.json(),
        json!({"code": "unavailable", "message_key": "error.unavailable"})
    );
    // The keep-warm task notices without any further check, and readiness fails.
    service.wait_readiness(503, READINESS_BOUND);

    // Connections open again (each after 1 s): the service recovers on its own, and from the
    // moment readiness passes, every check is answered from open connections.
    proxy.resume();
    let resumed = Instant::now();
    service.wait_ready(READINESS_BOUND);
    let recovered_after = resumed.elapsed();
    assert_checks_use_open_connections(
        &fixture,
        &proxy,
        &service,
        &session,
        20,
        "after readiness returned",
    );
    println!(
        "slow connect: {opened} connections at ready, ready again {recovered_after:?} after the proxy resumed, {} connections in all\n{}",
        proxy.accepted(),
        readiness_log(&service)
    );
}

/// Laysi/trustiot-v2#341: every keep-warm round confirms all the warm connections, not just one.
/// After the database drops every connection and no check runs, the keep-warm task alone finds
/// them (readiness fails, logged), replaces all of them (the proxy sees at least
/// `WARM_CONNECTIONS` new connections), and passes readiness again; checks then never meet a
/// dropped connection, so none opens one. A round that pinged one connection would replace only
/// that one and stay ready, and this test would wait for a readiness failure that never comes.
#[tokio::test]
async fn a_keep_warm_round_replaces_every_dropped_connection() {
    let Some((fixture, proxy, service, session)) = slow_connect_service(
        "CREATE DATABASE identity_rewarm OWNER identity_owner",
        "identity_rewarm",
    )
    .await
    else {
        return;
    };
    let not_ready = service.stdout.count_messages(NOT_READY_LOG);
    let ready = service.stdout.count_messages(READY_LOG);
    assert!(ready >= 1, "ready without its log line");
    let opened = proxy.accepted();

    proxy.cut();
    assert!(
        service
            .stdout
            .wait_for_messages(NOT_READY_LOG, not_ready + 1, READINESS_BOUND),
        "the keep-warm task did not notice the dropped connections:\n{}",
        service.stdout.text()
    );
    assert!(
        service
            .stdout
            .wait_for_messages(READY_LOG, ready + 1, READINESS_BOUND),
        "the keep-warm task did not open the connections again:\n{}",
        service.stdout.text()
    );
    // service-kit reuses a readiness result for up to 1 s.
    service.wait_ready(READINESS_BOUND);
    let reopened = proxy.accepted() - opened;
    assert!(
        reopened >= WARM_CONNECTIONS,
        "the keep-warm task replaced {reopened} of the {WARM_CONNECTIONS} dropped connections"
    );
    assert_checks_use_open_connections(&fixture, &proxy, &service, &session, 10, "after the round");
    println!(
        "keep-warm round: {opened} connections at ready, {reopened} opened after the cut\n{}",
        readiness_log(&service)
    );
}

/// Settings for a service whose database is at `127.0.0.1:<port>`.
fn base_env(keys: &str, key_file: &std::path::Path, port: u16) -> Env {
    vec![
        ("TRUSTIOT_INTERNAL_IDENTITY_KEYS".into(), keys.into()),
        (
            "TRUSTIOT_IDENTITY_SIGNING_KEY_FILE".into(),
            key_file.display().to_string(),
        ),
        (
            "TRUSTIOT_INGRESS_ROUTES".into(),
            json!([{"host": APP, "path": "/", "match": "PathPrefix", "service": "assets"}])
                .to_string(),
        ),
        ("TRUSTIOT_POSTGRES_HOST".into(), "127.0.0.1".into()),
        ("TRUSTIOT_POSTGRES_PORT".into(), port.to_string()),
        ("TRUSTIOT_POSTGRES_DATABASE".into(), "identity".into()),
        ("TRUSTIOT_POSTGRES_SSLMODE".into(), "verify-full".into()),
        // Read when a connection is made, never at startup; nothing listens at `port` anyway.
        (
            "TRUSTIOT_POSTGRES_CA_FILE".into(),
            key_file
                .with_file_name("postgres-ca.crt")
                .display()
                .to_string(),
        ),
        ("TRUSTIOT__DATABASE__USERNAME".into(), "identity_app".into()),
        (
            "TRUSTIOT__DATABASE__PASSWORD".into(),
            "do-not-log-this-password".into(),
        ),
    ]
}

/// The service refuses to start, before serving anything, with a key it cannot use or settings
/// that would let something else decide the key, the routes or the database security; the
/// output names the problem and never the key.
#[test]
fn unusable_keys_and_settings_stop_startup() {
    let key = TestKey::new(63);
    let other = TestKey::new(64);
    let dir = scratch_dir("startup");
    let key_file = dir.join("signing-key.pem");
    std::fs::write(&key_file, key.pem()).expect("write the key");
    let not_a_key = dir.join("not-a-key.pem");
    std::fs::write(
        &not_a_key,
        "-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----\n",
    )
    .expect("write");
    let large = dir.join("large.pem");
    std::fs::write(&large, "A".repeat(20_000)).expect("write");
    let good_keys = jwks(&[(&key, KID)]);
    let private_jwk = json!({"keys": [{
        "kty": "OKP", "crv": "Ed25519", "kid": KID,
        "x": key.jwk(KID)["x"], "d": key.secret_spellings()[1]
    }]})
    .to_string();
    let rotated_too_early = jwks(&[(&other, "2027-04-a")]);
    let ambiguous = jwks(&[(&key, "a"), (&key, "b")]);

    let cases: Vec<(&str, Env, &str)> = vec![
        (
            "public key not deployed",
            base_env(&rotated_too_early, &key_file, 1),
            "is not in TRUSTIOT_INTERNAL_IDENTITY_KEYS",
        ),
        (
            "public key under two key ids",
            base_env(&ambiguous, &key_file, 1),
            "more than one key id",
        ),
        (
            "missing key file",
            base_env(&good_keys, &dir.join("missing.pem"), 1),
            "cannot be read",
        ),
        (
            "not a key",
            base_env(&good_keys, &not_a_key, 1),
            "not an Ed25519 private key",
        ),
        (
            "too large",
            base_env(&good_keys, &large, 1),
            "too large",
        ),
        (
            "private key in the public key setting",
            base_env(&private_jwk, &key_file, 1),
            "private key",
        ),
        (
            "insecure database connection",
            with(base_env(&good_keys, &key_file, 1), "TRUSTIOT_POSTGRES_SSLMODE", "require"),
            "TRUSTIOT_POSTGRES_SSLMODE",
        ),
        (
            "no routes",
            without(base_env(&good_keys, &key_file, 1), "TRUSTIOT_INGRESS_ROUTES"),
            "TRUSTIOT_INGRESS_ROUTES",
        ),
        (
            "no CA for the database server",
            without(base_env(&good_keys, &key_file, 1), "TRUSTIOT_POSTGRES_CA_FILE"),
            "TRUSTIOT_POSTGRES_CA_FILE",
        ),
        (
            "a route the service cannot issue for",
            with(
                base_env(&good_keys, &key_file, 1),
                "TRUSTIOT_INGRESS_ROUTES",
                &json!([{"host": APP, "path": "/", "match": "PathPrefix", "service": "ai-gateway"}])
                    .to_string(),
            ),
            "TRUSTIOT_INGRESS_ROUTES",
        ),
        (
            "key file overridden by a service setting",
            with(
                base_env(&good_keys, &key_file, 1),
                "TRUSTIOT__PLATFORM__IDENTITY_SIGNING_KEY_FILE",
                &not_a_key.display().to_string(),
            ),
            "TRUSTIOT__PLATFORM__IDENTITY_SIGNING_KEY_FILE",
        ),
    ];
    let mut secrets = key.secret_spellings();
    secrets.push("do-not-log-this-password".to_owned());
    for (name, env, expected) in cases {
        let env: Vec<(&str, &str)> = env.iter().map(|(n, v)| (n.as_str(), v.as_str())).collect();
        let (status, output) = run_to_exit(&env);
        assert_eq!(status.code(), Some(1), "{name}: {output}");
        assert!(output.contains(expected), "{name}: {output}");
        assert!(!output.contains("listening"), "{name}: {output}");
        for secret in &secrets {
            assert!(
                !output.contains(secret.as_str()),
                "{name} leaks a secret: {output}"
            );
        }
    }
}

fn with(mut env: Env, name: &str, value: &str) -> Env {
    env.retain(|(existing, _)| existing != name);
    env.push((name.to_owned(), value.to_owned()));
    env
}

fn without(mut env: Env, name: &str) -> Env {
    env.retain(|(existing, _)| existing != name);
    env
}
