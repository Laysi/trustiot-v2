//! The first platform administrator, end to end (Laysi/trustiot-v2#582): the migration Job's
//! `bootstrap-platform-admin` (tools/migrate, `migrate::platform_admin`) on the product's schema
//! creates the user; the real identity binary records `identity.platform_admin_granted` in the
//! platform-wide operation log, mails that user a login code through a fake SMTP relay
//! (`support::smtp`), logs them in, and lets them create a tenant. Every address is under
//! example.com.
//!
//! Starts a throwaway Postgres with TLS (podman or docker); skipped without either unless `CI`
//! is set. One test: the marker is once per database, and the binary shares one product
//! database.

mod support;

use std::time::{Duration, Instant};

use migrate::config::{BootstrapConfig, BootstrapWait, Platform, PostgresSettings};
use migrate::identity_email::EmailAddress;
use migrate::platform_admin::{self, Outcome};
use serde_json::{Value, json};
use service_kit::postgres::SslMode;
use sqlx::PgConnection;
use support::smtp::{FakeSmtp, Mode};
use support::{KeyFiles, Service, TestKey, request, request_with_body};
use testkit::postgres::{SERVER_HOST, TEST_PASSWORD};
use testkit::product::{DATABASE, MIGRATOR, ProductPostgres, product_postgres};

const APP: &str = "app.saas.trustiot.test";
const KID: &str = "2026-10-p";
const LOGIN: &str = "/auth/login";
const TENANTS: &str = "/api/identity/v1/tenants";
const ADDRESS: &str = "first.platform.admin@example.com";

/// Generous bounds for a loaded host (CLAUDE.md, "Test time limits").
const READY_WITHIN: Duration = Duration::from_secs(60);
const EVENT_WITHIN: Duration = Duration::from_secs(60);
const MAIL_WITHIN: Duration = Duration::from_secs(60);
const POLL_EVERY: Duration = Duration::from_millis(200);

const GRANTS: &str = "SELECT payload::text, aggregate_type, aggregate_id, actor_user_id::text,
    actor_service, producer
FROM identity.platform_event_log WHERE event_type = 'identity.platform_admin_granted'";
const MEMBERSHIPS_OF: &str = "SELECT count(*) FROM identity.memberships WHERE user_id = $1::uuid";

/// The identity service with its platform role (the event log's consumer) and `relay`.
fn start(product: &ProductPostgres, relay: &FakeSmtp) -> Service {
    let files = KeyFiles::new("platform-admin", &TestKey::new(182), KID);
    let routes = json!([
        {"host": APP, "path": "/api/identity", "match": "PathPrefix", "service": "identity"},
    ]);
    let mut env = files.env();
    env.extend([
        ("TRUSTIOT_INGRESS_ROUTES".into(), routes.to_string()),
        ("TRUSTIOT__DATABASE__USERNAME".into(), "identity_app".into()),
        ("TRUSTIOT__DATABASE__PASSWORD".into(), TEST_PASSWORD.into()),
        (
            "TRUSTIOT__DATABASE__PLATFORM_USERNAME".into(),
            "identity_platform".into(),
        ),
        (
            "TRUSTIOT__DATABASE__PLATFORM_PASSWORD".into(),
            TEST_PASSWORD.into(),
        ),
    ]);
    env.extend(
        product
            .platform_env()
            .into_iter()
            .map(|(name, value)| (name.to_owned(), value)),
    );
    env.extend(relay.env());
    let env: Vec<(&str, &str)> = env.iter().map(|(n, v)| (n.as_str(), v.as_str())).collect();
    let service = Service::start(&env);
    service.wait_ready(READY_WITHIN);
    service
}

/// The command's settings, as the chart gives them: the migration role on the product database.
fn bootstrap_config(product: &ProductPostgres) -> BootstrapConfig {
    BootstrapConfig {
        platform: Platform {
            postgres_host: SERVER_HOST.to_owned(),
            postgres_port: product.postgres.address.port(),
            postgres_database: DATABASE.to_owned(),
            postgres_sslmode: SslMode::VerifyFull,
            postgres_ca_file: product.ca_file.clone(),
            clickhouse_url: None,
            clickhouse_ca_file: None,
        },
        postgres: PostgresSettings {
            username: MIGRATOR.to_owned(),
            password: TEST_PASSWORD.to_owned(),
        },
        wait: BootstrapWait {
            databases: Duration::from_secs(30),
        },
    }
}

fn post_login(service: &Service, body: &Value) -> support::Response {
    request_with_body(
        service.address,
        "POST",
        LOGIN,
        &[("Host", APP), ("Content-Type", "application/json")],
        body.to_string().as_bytes(),
    )
    .expect("call the login endpoint")
}

/// A row of [`GRANTS`].
type Grant = (
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    String,
);

/// The grant events in the platform-wide operation log, once the consumer recorded one.
async fn wait_for_grant(admin: &mut PgConnection) -> Vec<Grant> {
    let started = Instant::now();
    loop {
        let rows = sqlx::query_as(GRANTS)
            .fetch_all(&mut *admin)
            .await
            .expect("read the platform-wide operation log");
        if !rows.is_empty() {
            return rows;
        }
        assert!(
            started.elapsed() < EVENT_WITHIN,
            "identity.platform_admin_granted did not reach identity.platform_event_log within \
             {EVENT_WITHIN:?}"
        );
        tokio::time::sleep(POLL_EVERY).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_first_platform_admin_logs_in_and_creates_a_tenant() {
    let Some(product) = product_postgres(&["identity_app", "identity_platform"]).await else {
        return;
    };
    let mut admin = product.superuser().await;
    let relay = FakeSmtp::start(Mode::StartTls);
    let service = start(&product, &relay);

    // 1. The command creates the user, a platform administrator, with the address as the login
    //    flow normalizes it.
    let email =
        EmailAddress::parse(&format!("  {}  ", ADDRESS.to_uppercase())).expect("an address");
    let outcome = platform_admin::bootstrap(&bootstrap_config(&product), &email)
        .await
        .expect("the bootstrap");
    let Outcome::Created {
        user_id,
        existing_user,
    } = outcome
    else {
        panic!("the first run creates the administrator: {outcome:?}");
    };
    assert!(!existing_user);
    let user_id = user_id.to_string();

    // 2. Exactly one grant in the platform-wide operation log: the user ID and `via` only, no
    //    actor, no address.
    let grants = wait_for_grant(&mut admin).await;
    let [(payload, aggregate_type, aggregate_id, actor_user, actor_service, producer)] =
        grants.as_slice()
    else {
        panic!("exactly one grant: {grants:?}");
    };
    let payload: Value = serde_json::from_str(payload).expect("the payload");
    assert_eq!(payload, json!({"user_id": user_id, "via": "bootstrap"}));
    assert_eq!(
        (aggregate_type.as_str(), aggregate_id.as_str()),
        ("user", user_id.as_str())
    );
    assert_eq!((actor_user, actor_service), (&None, &None));
    assert_eq!(producer, "identity");
    // No implicit tenant access.
    let memberships: i64 = sqlx::query_scalar(MEMBERSHIPS_OF)
        .bind(&user_id)
        .fetch_one(&mut admin)
        .await
        .expect("count the memberships");
    assert_eq!(memberships, 0);

    // 3. The user logs in with an email code.
    let sent = post_login(&service, &json!({"step": "send_code", "email": ADDRESS}));
    assert_eq!(sent.status, 202, "{}", sent.raw());
    let deliveries = relay.wait_for_deliveries(1, MAIL_WITHIN);
    let [mail] = deliveries.as_slice() else {
        panic!("one email: {deliveries:?}");
    };
    assert_eq!(mail.rcpt_to, [ADDRESS.to_owned()]);
    let body = mail.body_text();
    let code = body
        .split(|c: char| !c.is_ascii_digit())
        .find(|run| run.len() == identity::login::CODE_DIGITS)
        .unwrap_or_else(|| panic!("no code in the email:\n{body}"))
        .to_owned();
    let logged_in = post_login(
        &service,
        &json!({"step": "verify_code", "email": ADDRESS, "code": code}),
    );
    assert_eq!(logged_in.status, 200, "{}", logged_in.raw());
    let set_cookie = logged_in.header_values("set-cookie");
    let [set_cookie] = set_cookie.as_slice() else {
        panic!("one session cookie: {}", logged_in.raw());
    };
    let cookie = set_cookie
        .split_once("; ")
        .map_or(*set_cookie, |(pair, _)| pair)
        .to_owned();

    // 4. As a platform administrator, the session creates a tenant: the ingress gateway's check
    //    signs the token, and the tenant API answers 201.
    let check = request(
        service.address,
        "POST",
        &format!("/internal/ext-authz{TENANTS}"),
        &[("Host", APP), ("Cookie", &cookie)],
    )
    .expect("call the ext_authz check");
    assert_eq!(check.status, 200, "{}", check.body);
    let token = check.header_values("x-internal-identity")[0].to_owned();
    let created = request_with_body(
        service.address,
        "POST",
        TENANTS,
        &[
            ("Host", APP),
            ("Content-Type", "application/json"),
            ("x-internal-identity", &token),
        ],
        json!({"name": "First tenant"}).to_string().as_bytes(),
    )
    .expect("call the tenant API");
    assert_eq!(created.status, 201, "{}", created.body);

    // 5. A second bootstrap changes nothing.
    let again = platform_admin::bootstrap(
        &bootstrap_config(&product),
        &EmailAddress::parse("another.admin@example.com").expect("an address"),
    )
    .await
    .expect("the second bootstrap");
    assert!(
        matches!(&again, Outcome::AlreadyCreated { user_id: marker, .. } if *marker == user_id),
        "{again:?}"
    );

    // 6. The service never logged the address.
    let logs = service.stdout.text().to_ascii_lowercase();
    assert!(!logs.contains(ADDRESS), "the service logged the address");
}
