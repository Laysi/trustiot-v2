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

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use contract::api::identity::{CreateRunDelegation, ExchangeRunDelegation, run_proof_message};
use contract::client::Endpoint;
use contract::{RunId, ServiceName, TenantRole};
use identity::delegation::DelegationClaims;
use identity::issuer::Issuer;
use identity::session::{SessionSecret, WARM_CONNECTIONS};
use identity_keys::SigningKeys;
use serde_json::json;
use service_kit::identity::{IdentityError, IdentityKeys, IdentityVerifier, UserIdentity};
use sqlx::{Connection, PgConnection};
use support::postgres::{SERVER_HOST, TEST_PASSWORD, TlsPostgres, tls_postgres};
use support::{
    KeyFiles, Service, SlowProxy, TestKey, cookie, jwks, kid_of, new_secret, request,
    request_with_body, run_to_exit, scratch_dir, signing_keys,
};

/// The roles of decision 29 for this service, created once per Postgres, with the service role's
/// `search_path` on its schema. The migration Job (Laysi/trustiot-v2#60) creates them so in a
/// deployment.
const ROLES: &str = "
CREATE ROLE identity_owner LOGIN PASSWORD 'service-kit-test';
CREATE ROLE identity_app LOGIN PASSWORD 'service-kit-test';
ALTER ROLE identity_app SET search_path = identity;
";

const MIGRATION: &str = include_str!("../migrations/postgres/0001_identity.sql");
/// The writes on sessions that logging in and extending a session need, and their guard
/// (Laysi/trustiot-v2#103).
const LOGIN_MIGRATION: &str = include_str!("../migrations/postgres/0004_login.sql");
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

/// One test's database with the migration applied, the key files and the CA file, and a
/// running service.
struct Fixture {
    postgres: Arc<TlsPostgres>,
    database: &'static str,
    key: TestKey,
    files: KeyFiles,
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
        sqlx::raw_sql(DATABASE_TEMPLATE)
            .execute(&mut owner)
            .await
            .expect("run the database template");
        sqlx::raw_sql(SCHEMA)
            .execute(&mut owner)
            .await
            .expect("create the schema");
        for migration in [MIGRATION, LOGIN_MIGRATION] {
            sqlx::raw_sql(migration)
                .execute(&mut owner)
                .await
                .expect("run the migration");
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

    /// Every setting the chart gives the service, pointing at this database.
    fn env(&self) -> Env {
        let routes = json!([
            {"host": APP, "path": "/", "match": "PathPrefix", "service": "assets"},
            {"host": APP, "path": "/api/certs", "match": "PathPrefix", "service": "certs"},
            {"host": APP, "path": "/api/assistant", "match": "PathPrefix", "service": "assistant"},
        ]);
        let mut env = self.files.env();
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
        env
    }

    /// Starts the service and waits until it is ready, which needs a session that passed
    /// service-kit's row-level security check.
    fn service(&self) -> Service {
        self.service_with(&[])
    }

    /// [`Fixture::service`] with `extra` settings added.
    fn service_with(&self, extra: &[(&str, &str)]) -> Service {
        let mut env = self.env();
        env.extend(
            extra
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned())),
        );
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

/// One token exchange request, as service-kit's `OnBehalf` sends it, to the exchange's own port.
fn exchange(service: &Service, token: Option<&str>, audience: &str) -> support::Response {
    let mut headers = vec![("Content-Type", "application/json")];
    if let Some(token) = token {
        headers.push(("x-internal-identity", token));
    }
    request_with_body(
        service.exchange_address,
        "POST",
        "/internal/token-exchange",
        &headers,
        json!({ "audience": audience }).to_string().as_bytes(),
    )
    .expect("exchange request")
}

/// Acceptance (on-behalf calls and impersonation, Laysi/trustiot-v2#106), with the real binary:
/// the token the check signed for assets is exchanged for certs, which the call list lets assets
/// call, and for nothing else. The exchanged token carries the session's identity, expires no
/// later than the original, and works at certs only.
#[tokio::test]
async fn a_routed_token_is_exchanged_along_the_call_list_only() {
    let Some(fixture) = Fixture::new(
        "CREATE DATABASE identity_exchange OWNER identity_owner",
        "identity_exchange",
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
    let calls = json!({"assets": ["certs"]}).to_string();
    let service = fixture.service_with(&[("TRUSTIOT_ON_BEHALF_CALLS", &calls)]);

    let response = check(&service, "/api/devices", Some(&cookie(&secret)));
    let user = verified(&fixture, &response, ServiceName::Assets);
    let token = response.header_values("x-internal-identity")[0].to_owned();

    let response = exchange(&service, Some(&token), "certs");
    assert_eq!(response.status, 200, "{}", response.raw());
    assert_eq!(response.header_values("cache-control"), ["no-store"]);
    let exchanged = response.json()["token"]
        .as_str()
        .expect("a token")
        .to_owned();
    let at_certs = fixture
        .verifier(ServiceName::Certs)
        .verify(&exchanged)
        .expect("certs accepts the exchanged token");
    assert_eq!(at_certs.user_id(), user.user_id());
    assert_eq!(at_certs.tenant(), user.tenant());
    assert_eq!(at_certs.tenant().map(|t| t.role), Some(TenantRole::Viewer));
    assert!(!at_certs.is_platform_admin());
    assert!(at_certs.claims().exp <= user.claims().exp);
    for audience in [ServiceName::Assets, ServiceName::Alerts] {
        assert_eq!(
            fixture.verifier(audience).verify(&exchanged).err(),
            Some(IdentityError::WrongAudience),
            "{audience} refuses a token for certs"
        );
    }

    // Refused: assets may not call alerts, and certs, holding the exchanged token, may call
    // nobody (that an exchanged token is never exchanged again, even along the list, is
    // src/exchange/tests.rs).
    for (holder, audience) in [(&token, "alerts"), (&exchanged, "assets")] {
        let response = exchange(&service, Some(holder), audience);
        assert_eq!(response.status, 403, "{}", response.raw());
        assert_eq!(response.json()["code"], json!("permission_denied"));
        assert!(!response.body.contains("token"), "{}", response.raw());
    }
    assert_unauthenticated(&exchange(&service, None, "certs"));
    assert_unauthenticated(&exchange(&service, Some("forged"), "certs"));
}

/// Revokes every session of a user, as logging out everywhere does.
const REVOKE_SESSIONS: &str = "UPDATE identity.sessions SET revoked_at = now()
WHERE user_id = $1::uuid AND revoked_at IS NULL";

/// A run's key, derived in code from a one-byte seed: a throwaway test key.
fn run_key(seed: u8) -> ed25519_dalek::SigningKey {
    ed25519_dalek::SigningKey::from_bytes(&[seed; 32])
}

fn public(key: &ed25519_dalek::SigningKey) -> String {
    URL_SAFE_NO_PAD.encode(key.verifying_key().as_bytes())
}

/// One request to a run delegation route, on the exchange's port, with `credential` in
/// `x-internal-identity`.
fn delegation_request(
    service: &Service,
    route: &str,
    credential: &str,
    body: &serde_json::Value,
) -> support::Response {
    request_with_body(
        service.exchange_address,
        "POST",
        route,
        &[
            ("Content-Type", "application/json"),
            ("x-internal-identity", credential),
        ],
        body.to_string().as_bytes(),
    )
    .expect("run delegation request")
}

/// Exchanges `delegation` for a token to `audience`, with a proof of `key` made now.
fn exchange_delegation(
    service: &Service,
    delegation: &str,
    key: &ed25519_dalek::SigningKey,
    audience: ServiceName,
) -> support::Response {
    use ed25519_dalek::Signer;
    let iat = identity::issuer::unix_now();
    let message = run_proof_message(delegation, audience, iat);
    let proof = URL_SAFE_NO_PAD.encode(key.sign(&message).to_bytes());
    delegation_request(
        service,
        ExchangeRunDelegation::ROUTE,
        delegation,
        &json!({"audience": audience.as_str(), "iat": iat, "proof": proof}),
    )
}

/// Acceptance (Laysi/trustiot-v2#551), with the real binary and Postgres: the assistant starts
/// a delegation with the token the check signed for it, and exchanges it for certs, which the
/// call list lets it call, and for nothing else. A delegation issued ten minutes ago still works;
/// each exchange reads the role again; a removed member and a revoked session stop the next
/// exchange; neither the delegation nor a token reaches the logs.
#[tokio::test]
async fn a_run_delegation_acts_for_the_user_until_the_session_ends() {
    let Some(fixture) = Fixture::new(
        "CREATE DATABASE identity_run_delegation OWNER identity_owner",
        "identity_run_delegation",
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
    let calls = json!({"assistant": ["certs"]}).to_string();
    let service = fixture.service_with(&[("TRUSTIOT_ON_BEHALF_CALLS", &calls)]);

    let response = check(&service, "/api/assistant/chat", Some(&cookie(&secret)));
    let user = verified(&fixture, &response, ServiceName::Assistant);
    let session = user.claims().sid.expect("the token names its session");
    let token = response.header_values("x-internal-identity")[0].to_owned();
    let key = run_key(21);
    let run: RunId = "01928f6e-7a3c-7d4e-8f00-0000000000a1"
        .parse()
        .expect("run id");
    let created = delegation_request(
        &service,
        CreateRunDelegation::ROUTE,
        &token,
        &json!({"run_id": run.to_string(), "key": public(&key), "lifetime_secs": 900}),
    );
    assert_eq!(created.status, 200, "{}", created.raw());
    assert_eq!(created.header_values("cache-control"), ["no-store"]);
    let delegation = created.json()["delegation"]
        .as_str()
        .expect("a delegation")
        .to_owned();
    for audience in ServiceName::ALL {
        assert!(
            fixture.verifier(audience).verify(&delegation).is_err(),
            "{audience} refuses the delegation as a token"
        );
    }

    let role_at_certs = |response: &support::Response| {
        assert_eq!(response.status, 200, "{}", response.raw());
        let token = response.json()["token"]
            .as_str()
            .expect("a token")
            .to_owned();
        let at_certs = fixture
            .verifier(ServiceName::Certs)
            .verify(&token)
            .expect("certs accepts the token");
        assert_eq!(at_certs.user_id(), user.user_id());
        assert_eq!(
            at_certs.claims().act.as_ref().map(|act| act.sub),
            Some(ServiceName::Assistant)
        );
        assert_eq!(at_certs.claims().sid, None);
        assert_eq!(at_certs.claims().exp - at_certs.claims().iat, 60);
        at_certs.tenant().map(|grant| grant.role)
    };
    let to_certs =
        |delegation: &str| exchange_delegation(&service, delegation, &key, ServiceName::Certs);
    assert_eq!(
        role_at_certs(&to_certs(&delegation)),
        Some(TenantRole::Viewer)
    );
    // Not on the call list.
    let refused = exchange_delegation(&service, &delegation, &key, ServiceName::Alerts);
    assert_eq!(refused.status, 403, "{}", refused.raw());
    // Someone who has the delegation but not the run's key.
    assert_unauthenticated(&exchange_delegation(
        &service,
        &delegation,
        &run_key(22),
        ServiceName::Certs,
    ));

    // A delegation this service issued ten minutes ago, long after the request's token expired:
    // signed here with the service's own test key, as the create route would have then.
    let issuer = Issuer::with_keys(
        SigningKeys::parse(&signing_keys(&[(&fixture.key, KID, 1, 0)])).expect("signing keys"),
        IdentityKeys::from_jwks(&fixture.keys())
            .expect("keys")
            .into(),
        0,
    );
    let now = identity::issuer::unix_now();
    let old = identity::delegation::sign(
        &issuer,
        &DelegationClaims {
            sub: user.user_id(),
            sid: session,
            tenant: user.tenant_id(),
            holder: ServiceName::Assistant,
            run,
            key: public(&key),
            iat: now - 600,
            exp: now + 300,
        },
        now - 600,
    )
    .expect("sign")
    .expose()
    .to_owned();
    assert_eq!(role_at_certs(&to_certs(&old)), Some(TenantRole::Viewer));

    // The role is read on every exchange; a removed member gets nothing.
    fixture
        .owner_execute(UPDATE_ROLE, &[TENANT_A, USER, "editor"])
        .await;
    assert_eq!(
        role_at_certs(&to_certs(&delegation)),
        Some(TenantRole::Editor)
    );
    fixture
        .owner_execute(DELETE_MEMBERSHIP, &[TENANT_A, USER])
        .await;
    let refused = to_certs(&delegation);
    assert_eq!(refused.status, 403, "{}", refused.raw());
    assert_eq!(refused.json()["code"], json!("permission_denied"));
    fixture.add_membership(TENANT_A, USER, "viewer").await;
    assert_eq!(
        role_at_certs(&to_certs(&delegation)),
        Some(TenantRole::Viewer)
    );

    // Logging out everywhere stops the run's next call.
    fixture.owner_execute(REVOKE_SESSIONS, &[USER]).await;
    for delegation in [&delegation, &old] {
        assert_unauthenticated(&to_certs(delegation));
    }

    let logs = service.stop();
    assert!(logs.contains("issued a run delegation"), "{logs}");
    for credential in [&delegation, &old, &token] {
        let (_, mac) = credential.rsplit_once('.').expect("a signed value");
        assert!(!logs.contains(mac), "a credential reached the logs");
    }
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

/// Acceptance (Laysi/trustiot-v2#123): a cross-site write gets 403 `security.cross_site_request`
/// with a valid session, and so does one with an unknown session (not 401: the session is never
/// looked up); the SPA's own write goes on.
#[tokio::test]
async fn cross_site_writes_get_403_and_same_origin_ones_go_on() {
    let Some(fixture) = Fixture::new(
        "CREATE DATABASE identity_csrf OWNER identity_owner",
        "identity_csrf",
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
    let service = fixture.service();
    let post = |cookie: &str, extra: &[(&str, &str)]| {
        let mut headers = vec![("Host", APP), ("Cookie", cookie)];
        headers.extend_from_slice(extra);
        request(
            service.address,
            "POST",
            "/internal/ext-authz/api/devices",
            &headers,
        )
        .expect("check request")
    };
    let valid = cookie(&secret);
    let origin = format!("https://{APP}");
    let response = post(
        &valid,
        &[("Sec-Fetch-Site", "same-origin"), ("Origin", &origin)],
    );
    verified(&fixture, &response, ServiceName::Assets);
    verified(
        &fixture,
        &post(&valid, &[("Origin", &origin)]),
        ServiceName::Assets,
    );

    let unknown = cookie(&new_secret());
    let cross_site: [&[(&str, &str)]; 2] = [
        &[("Origin", "https://attacker.example.test")],
        &[("Sec-Fetch-Site", "cross-site")],
    ];
    for extra in cross_site {
        for cookie in [&valid, &unknown] {
            let response = post(cookie, extra);
            assert_eq!(response.status, 403, "{extra:?}: {}", response.raw());
            assert!(response.header_values("x-internal-identity").is_empty());
            assert_eq!(
                response.json(),
                json!({"code": "permission_denied", "message_key": "security.cross_site_request"})
            );
        }
    }
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

/// The service role writes sessions only as logging in, logging out and extending a session do
/// (Laysi/trustiot-v2#103): it creates sessions of at most 7 days, moves their expiry within
/// that, and revokes them, and nothing else. A revoked session stays revoked; users, roles and
/// memberships stay read-only; memberships are invisible without a tenant.
#[tokio::test]
async fn the_service_role_writes_sessions_only_within_the_login_rules() {
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
    // One active and one revoked session for the statements below to touch.
    fixture
        .add_session(USER, Some(TENANT_A), SESSION_SECS, false)
        .await;
    fixture
        .add_session(USER, Some(TENANT_A), SESSION_SECS, true)
        .await;
    let mut app = fixture.connect("identity_app").await;
    // What the service does: allowed.
    for statement in [
        "INSERT INTO identity.sessions (id, token_hash, user_id, expires_at) VALUES (gen_random_uuid(), sha256('x'), '01928f6e-7a3c-7d4e-8f00-0123456789ab', now() + interval '12 hours')",
        "UPDATE identity.sessions SET expires_at = LEAST(now() + interval '12 hours', created_at + interval '7 days') WHERE revoked_at IS NULL",
        "UPDATE identity.sessions SET revoked_at = now() WHERE token_hash = sha256('x')",
    ] {
        sqlx::raw_sql(statement)
            .execute(&mut app)
            .await
            .unwrap_or_else(|error| panic!("{statement}: {error}"));
    }
    for statement in [
        // Longer than the absolute lifetime, new or extended (the trigger).
        "INSERT INTO identity.sessions (id, token_hash, user_id, expires_at) VALUES (gen_random_uuid(), sha256('y'), '01928f6e-7a3c-7d4e-8f00-0123456789ab', now() + interval '8 days')",
        "UPDATE identity.sessions SET expires_at = created_at + interval '7 days 1 second'",
        // A revoked session coming back, or its revocation moved (the trigger).
        "UPDATE identity.sessions SET revoked_at = NULL WHERE revoked_at IS NOT NULL",
        "UPDATE identity.sessions SET revoked_at = now() + interval '1 day' WHERE revoked_at IS NOT NULL",
        // A creation time of its own choosing, which would move the 7-day cap (no column grant).
        "INSERT INTO identity.sessions (id, token_hash, user_id, created_at, expires_at) VALUES (gen_random_uuid(), sha256('v'), '01928f6e-7a3c-7d4e-8f00-0123456789ab', now() + interval '30 days', now() + interval '36 days')",
        "INSERT INTO identity.sessions (id, token_hash, user_id, expires_at, revoked_at) VALUES (gen_random_uuid(), sha256('u'), '01928f6e-7a3c-7d4e-8f00-0123456789ab', now() + interval '1 hour', NULL)",
        // Columns and tables without a grant.
        "UPDATE identity.sessions SET user_id = '01928f6e-7a3c-7d4e-8f00-0000000000ad'",
        "UPDATE identity.sessions SET token_hash = sha256('z')",
        "UPDATE identity.sessions SET created_at = now()",
        // `active_tenant_id` is not here: 0005_preferences_and_tenant_switch.sql, which this
        // fixture does not apply, grants it for switching tenants (Laysi/trustiot-v2#497), and
        // tests/account.rs checks that grant on the product's schema.
        "DELETE FROM identity.sessions",
        "TRUNCATE identity.sessions",
        "INSERT INTO identity.users (id) VALUES (gen_random_uuid())",
        "UPDATE identity.users SET platform_admin = true",
        "UPDATE identity.users SET email = 'someone@example.com'",
        "INSERT INTO identity.memberships (tenant_id, user_id, tenant_role) VALUES ('01928f6e-7a3c-7d4e-8f00-00000000000a', '01928f6e-7a3c-7d4e-8f00-0123456789ab', 'admin')",
        "UPDATE identity.login_codes SET code_hash = sha256('w')",
        "UPDATE identity.login_codes SET expires_at = now() + interval '1 year'",
        "UPDATE identity.login_codes SET user_id = '01928f6e-7a3c-7d4e-8f00-0000000000ad'",
        "TRUNCATE identity.login_codes",
        // The guard itself.
        "DROP TRIGGER sessions_guard ON identity.sessions",
        "ALTER TABLE identity.sessions DISABLE TRIGGER sessions_guard",
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
    let files = KeyFiles::new("unreachable", &key, KID);
    // A port that refuses connections.
    let port = TcpListener::bind("127.0.0.1:0")
        .and_then(|listener| listener.local_addr())
        .expect("free port")
        .port();
    let env = base_env(&files, port);
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
fn base_env(files: &KeyFiles, port: u16) -> Env {
    let mut env = files.env();
    env.extend([
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
            files
                .signing_keys
                .with_file_name("postgres-ca.crt")
                .display()
                .to_string(),
        ),
        ("TRUSTIOT__DATABASE__USERNAME".into(), "identity_app".into()),
        (
            "TRUSTIOT__DATABASE__PASSWORD".into(),
            "do-not-log-this-password".into(),
        ),
    ]);
    env
}

/// The service refuses to start, before serving anything, with settings that would let
/// something else decide the keys, the routes or the database security; the output names the
/// problem and never a secret. Key files that are missing or unusable do not stop it
/// (`tests/adversarial.rs`, "Key files"): it starts unready, as on a fresh install.
#[test]
fn unusable_settings_stop_startup() {
    let key = TestKey::new(63);
    let files = KeyFiles::new("startup", &key, KID);
    let cases: Vec<(&str, Env, &str)> = vec![
        (
            "insecure database connection",
            with(base_env(&files, 1), "TRUSTIOT_POSTGRES_SSLMODE", "require"),
            "TRUSTIOT_POSTGRES_SSLMODE",
        ),
        (
            "no routes",
            without(base_env(&files, 1), "TRUSTIOT_INGRESS_ROUTES"),
            "TRUSTIOT_INGRESS_ROUTES",
        ),
        (
            "no CA for the database server",
            without(base_env(&files, 1), "TRUSTIOT_POSTGRES_CA_FILE"),
            "TRUSTIOT_POSTGRES_CA_FILE",
        ),
        (
            "a route the service cannot issue for",
            with(
                base_env(&files, 1),
                "TRUSTIOT_INGRESS_ROUTES",
                &json!([{"host": APP, "path": "/", "match": "PathPrefix", "service": "ai-gateway"}])
                    .to_string(),
            ),
            "TRUSTIOT_INGRESS_ROUTES",
        ),
        (
            "an on-behalf call to a service that takes no tokens",
            with(
                base_env(&files, 1),
                "TRUSTIOT_ON_BEHALF_CALLS",
                &json!({"assistant": ["ai-gateway"]}).to_string(),
            ),
            "TRUSTIOT_ON_BEHALF_CALLS",
        ),
        (
            "an on-behalf call list that is not one",
            with(base_env(&files, 1), "TRUSTIOT_ON_BEHALF_CALLS", "assets=certs"),
            "TRUSTIOT_ON_BEHALF_CALLS",
        ),
        (
            "on-behalf calls overridden by a service setting",
            with(
                base_env(&files, 1),
                "TRUSTIOT__PLATFORM__ON_BEHALF_CALLS",
                &json!({"assets": ["certs"]}).to_string(),
            ),
            "TRUSTIOT__PLATFORM__ON_BEHALF_CALLS",
        ),
        (
            "no signing keys file setting",
            without(base_env(&files, 1), support::SIGNING_KEYS_FILE),
            support::SIGNING_KEYS_FILE,
        ),
        (
            "no public keys file setting",
            without(base_env(&files, 1), support::PUBLIC_KEYS_FILE),
            support::PUBLIC_KEYS_FILE,
        ),
        (
            "signing keys file overridden by a service setting",
            with(
                base_env(&files, 1),
                "TRUSTIOT__PLATFORM__IDENTITY_SIGNING_KEYS_FILE",
                &files.public_keys.display().to_string(),
            ),
            "TRUSTIOT__PLATFORM__IDENTITY_SIGNING_KEYS_FILE",
        ),
        (
            "public keys file overridden by a service setting",
            with(
                base_env(&files, 1),
                "TRUSTIOT__PLATFORM__INTERNAL_IDENTITY_KEYS_FILE",
                &files.public_keys.display().to_string(),
            ),
            "TRUSTIOT__PLATFORM__INTERNAL_IDENTITY_KEYS_FILE",
        ),
        // The event log's platform role (Laysi/trustiot-v2#130): both credentials or neither,
        // and never the service role.
        (
            "a platform role without its password",
            with(
                base_env(&files, 1),
                "TRUSTIOT__DATABASE__PLATFORM_USERNAME",
                "identity_platform",
            ),
            "database.platform_password",
        ),
        (
            "a platform password without its role",
            with(
                base_env(&files, 1),
                "TRUSTIOT__DATABASE__PLATFORM_PASSWORD",
                "do-not-log-this-password",
            ),
            "database.platform_username",
        ),
        (
            "the service role as the platform role",
            with(
                with(
                    base_env(&files, 1),
                    "TRUSTIOT__DATABASE__PLATFORM_USERNAME",
                    "identity_app",
                ),
                "TRUSTIOT__DATABASE__PLATFORM_PASSWORD",
                "do-not-log-this-password",
            ),
            "database.platform_username",
        ),
        (
            "a propagation time below the chart's minimum",
            with(
                base_env(&files, 1),
                "TRUSTIOT_IDENTITY_KEY_PROPAGATION_SECONDS",
                "60",
            ),
            "platform.identity_key_propagation_seconds",
        ),
        (
            "a revocation above the highest generation",
            with(
                base_env(&files, 1),
                "TRUSTIOT_IDENTITY_REVOKE_BEFORE_GENERATION",
                "1000000000",
            ),
            "platform.identity_revoke_before_generation",
        ),
        (
            "a maximum key age above 365 days",
            with(
                base_env(&files, 1),
                "TRUSTIOT_IDENTITY_MAX_KEY_AGE_SECONDS",
                "31536001",
            ),
            "platform.identity_max_key_age_seconds",
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

/// How long a test waits for the service to act on changed key files: many reload intervals
/// (5 s) on a loaded machine.
const RELOAD_BOUND: Duration = Duration::from_secs(90);

/// Checks until a token for `audience` names `kid`; returns it.
fn wait_for_kid(service: &Service, cookie: &str, path: &str, kid: &str) -> String {
    let deadline = Instant::now() + RELOAD_BOUND;
    loop {
        let response = check(service, path, Some(cookie));
        assert_eq!(response.status, 200, "{}", response.raw());
        let token = response.header_values("x-internal-identity")[0].to_owned();
        if kid_of(&token) == kid {
            return token;
        }
        assert!(
            Instant::now() < deadline,
            "the service did not sign with {kid} within {RELOAD_BOUND:?}\n{}",
            service.stdout.text()
        );
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Acceptance (Laysi/trustiot-v2#404): a key rotation as the init container performs it
/// (`crates/identity-keys`, the same code), against a running service and without a restart.
/// While both public keys are published, a token from either key verifies in every service
/// (each audience the routes name, with the public keys file as that service reads it); the
/// service switches to the new key at its `signs_from`; once the old key is retired, its tokens
/// stop verifying and new ones still do.
#[tokio::test]
async fn a_key_rotation_needs_no_restart_and_tokens_verify_throughout() {
    use identity_keys::{Request, SigningKey, SigningKeys};

    let Some(fixture) =
        Fixture::new("CREATE DATABASE rotation OWNER identity_owner", "rotation").await
    else {
        return;
    };
    fixture.add_user(USER, false).await;
    fixture.add_membership(TENANT_A, USER, "viewer").await;
    let secret = fixture
        .add_session(USER, Some(TENANT_A), SESSION_SECS, false)
        .await;
    let session = cookie(&secret);

    // Generation 1, as a fresh install's init container leaves it.
    let now = identity::issuer::unix_now();
    let request = |generation| Request {
        generation,
        revoke_before: 0,
        legacy: None,
        max_key_age: None,
    };
    let mut first = SigningKeys::default()
        .plan(&request(1), now, SigningKey::generate)
        .expect("plan")
        .keys;
    first.publish(now, 600);
    let old_kid = first.keys()[0].kid().to_owned();
    fixture.files.set_signing_keys(&first.to_json());
    fixture.files.set_public_keys(&first.jwks());
    let service = fixture.service();
    let services = [
        ("/", ServiceName::Assets),
        ("/api/certs/x", ServiceName::Certs),
    ];
    let old_tokens: Vec<String> = services
        .iter()
        .map(|(path, _)| wait_for_kid(&service, &session, path, &old_kid))
        .collect();

    // Generation 2: step 1, the new key into the Secret, unpublished: nothing changes.
    let bumped = identity::issuer::unix_now();
    let mut second = first
        .clone()
        .plan(&request(2), bumped, SigningKey::generate)
        .expect("plan")
        .keys;
    let new_kid = second.keys()[1].kid().to_owned();
    fixture.files.set_signing_keys(&second.to_json());
    // Step 2: both public keys to every service.
    fixture.files.set_public_keys(&second.jwks());
    let both = second.jwks();
    // Step 3: the new key signs a short while from now (the chart waits
    // internalIdentity.keyPropagationSeconds; here a few reload intervals).
    let wait = 15;
    second.publish(identity::issuer::unix_now(), wait);
    let signs_from = second.keys()[1].signs_from().expect("published");
    fixture.files.set_signing_keys(&second.to_json());

    // Until signs_from the service keeps the old key; tokens of both verify everywhere.
    let early = check(&service, "/", Some(&session));
    let early_token = early.header_values("x-internal-identity")[0].to_owned();
    if identity::issuer::unix_now() < signs_from {
        assert_eq!(kid_of(&early_token), old_kid, "signed before signs_from");
    }
    let new_tokens: Vec<String> = services
        .iter()
        .map(|(path, _)| wait_for_kid(&service, &session, path, &new_kid))
        .collect();
    assert!(
        identity::issuer::unix_now() >= signs_from,
        "the new key signed before its signs_from"
    );
    for ((_, audience), (old, new)) in services.iter().zip(old_tokens.iter().zip(&new_tokens)) {
        let every_service = IdentityVerifier::new(
            *audience,
            IdentityKeys::from_jwks(&both).expect("the published keys"),
        );
        assert!(every_service.verify(old).is_ok(), "{audience}: old token");
        assert!(every_service.verify(new).is_ok(), "{audience}: new token");
        println!(
            "{audience}: old ({old_kid}) and new ({new_kid}) tokens verify during the overlap"
        );
    }

    // Retirement: the init container's run after the overlap leaves only the new key.
    let retire_at = second.retire_at(signs_from).expect("a signing key");
    let retired = second
        .clone()
        .plan(&request(2), retire_at, SigningKey::generate)
        .expect("plan")
        .keys;
    assert_eq!(
        retired.key_ids().collect::<Vec<_>>(),
        [new_kid.as_str()],
        "the old key retires"
    );
    fixture.files.set_public_keys(&retired.jwks());
    fixture.files.set_signing_keys(&retired.to_json());
    let after_retirement = IdentityVerifier::new(
        ServiceName::Assets,
        IdentityKeys::from_jwks(&retired.jwks()).expect("the published keys"),
    );
    assert_eq!(
        after_retirement.verify(&old_tokens[0]).err(),
        Some(IdentityError::UnknownKey)
    );
    let latest = wait_for_kid(&service, &session, "/", &new_kid);
    assert!(after_retirement.verify(&latest).is_ok());
    assert!(
        service
            .stdout
            .wait_for_message("signing internal identity tokens", Duration::from_secs(1))
            .is_some()
    );
    let logs = service.stop();
    for key in [&first, &second] {
        for stored in
            serde_json::from_str::<serde_json::Value>(&key.to_json()).expect("JSON")["keys"]
                .as_array()
                .expect("keys")
        {
            let d = stored["d"].as_str().expect("d");
            assert!(!logs.contains(d), "a private key reached the logs");
        }
    }
}

/// Revocation, the identity service's side: with `TRUSTIOT_IDENTITY_REVOKE_BEFORE_GENERATION`
/// above every key it reads, it signs nothing (unready, 503) instead of using a revoked key,
/// until a key of a high enough generation arrives.
#[tokio::test]
async fn a_revoked_generation_never_signs() {
    let Some(fixture) = Fixture::new(
        "CREATE DATABASE revocation OWNER identity_owner",
        "revocation",
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
    let env = with(
        fixture.env(),
        "TRUSTIOT_IDENTITY_REVOKE_BEFORE_GENERATION",
        "2",
    );
    let env: Vec<(&str, &str)> = env.iter().map(|(n, v)| (n.as_str(), v.as_str())).collect();
    let service = Service::start(&env);
    service.wait_readiness(503, READINESS_BOUND);
    let refused = check(&service, "/", Some(&session));
    assert_eq!(refused.status, 503, "{}", refused.raw());
    assert!(refused.header_values("x-internal-identity").is_empty());

    let replacement = TestKey::new(65);
    fixture
        .files
        .set_signing_keys(&signing_keys(&[(&replacement, "2026-10-b", 2, 0)]));
    fixture
        .files
        .set_public_keys(&jwks(&[(&replacement, "2026-10-b")]));
    service.wait_readiness(200, RELOAD_BOUND);
    let token = wait_for_kid(&service, &session, "/", "2026-10-b");
    let published = IdentityKeys::from_jwks(&jwks(&[(&replacement, "2026-10-b")])).expect("keys");
    assert!(
        IdentityVerifier::new(ServiceName::Assets, published)
            .verify(&token)
            .is_ok()
    );
    let logs = service.stop();
    for secret in fixture.key.secret_spellings() {
        assert!(!logs.contains(&secret), "the revoked key reached the logs");
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
