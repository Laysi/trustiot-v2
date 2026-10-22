//! Logging in with an email code and logging out, end to end (Laysi/trustiot-v2#103): the real
//! binary on the product's schema, applied by the migration Job (`testkit::product`), mailing
//! codes through a fake SMTP relay this test starts (`support::smtp`, TLS with a test CA). Every
//! address is under example.com; no real mailbox is involved.
//!
//! What it shows, by acceptance criterion of the issue:
//!
//! - a used or expired code is refused, and a code dies after `MAX_ATTEMPTS` attempts
//!   ([`codes_are_used_once_and_die_after_five_attempts`],
//!   [`a_code_from_the_email_logs_in_once`]);
//! - `redirect_to` leaves only to a path on this site ([`redirects_stay_on_this_site`]);
//! - logging out everywhere refuses every other session on its next request
//!   ([`logging_out_refuses_the_sessions_next_request`]);
//! - there is no fake login, in this local build, whatever the cookies say, and no build switch
//!   that could add one ([`there_is_no_password_login_sign_up_or_fake_login`],
//!   [`no_build_switch_can_add_a_fake_login`]);
//! - the code is in no URL and no log line, at every log level
//!   ([`a_code_from_the_email_logs_in_once`]);
//! - a role change applies to the same session's next request
//!   ([`a_role_change_applies_to_the_next_request`]);
//! - the only routes are the code login, logout and the tenant API
//!   ([`there_is_no_password_login_sign_up_or_fake_login`]);
//! - another site cannot make the browser ask for a code, try one or log in, and the SPA's own
//!   requests still log in ([`cross_site_login_requests_change_nothing`]).
//!
//! Starts a throwaway Postgres with TLS (podman or docker); skipped without either unless `CI`
//! is set. The tests share the binary's product database, each with users of its own.

mod support;

use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use identity::login::{CODE_DIGITS, MAX_ATTEMPTS, REFUSAL_FLOOR};
use serde_json::{Value, json};
use sqlx::PgConnection;
use support::postgres::TestCa;
use support::smtp::{Delivery, FakeSmtp, Mode};
use support::{
    KeyFiles, Service, TestKey, cookie, new_secret, request, request_with_body, scratch_dir,
};
use testkit::postgres::TEST_PASSWORD;
use testkit::product::{ProductPostgres, product_postgres};

const APP: &str = "app.saas.trustiot.test";
const KID: &str = "2026-10-l";
const LOGIN: &str = "/auth/login";
const LOGOUT: &str = "/api/identity/v1/sessions/current";
const LOGOUT_EVERYWHERE: &str = "/api/identity/v1/sessions";
const SESSION_COOKIE: &str = "__Host-trustiot-session";
/// What `Set-Cookie` says after the value (services/identity/src/session.rs).
const COOKIE_ATTRIBUTES: &str = "Max-Age=604800; Path=/; Secure; HttpOnly; SameSite=Lax";
const CLEARED_COOKIE: &str =
    "__Host-trustiot-session=; Max-Age=0; Path=/; Secure; HttpOnly; SameSite=Lax";

/// Generous bounds for a loaded host (CLAUDE.md, "Test time limits"): a code reaching the relay
/// (one SMTP session over TLS), a log line, a background write reaching the database.
const MAIL_WITHIN: Duration = Duration::from_secs(60);
const LOG_WITHIN: Duration = Duration::from_secs(60);
const WRITE_WITHIN: Duration = Duration::from_secs(60);

/// Concurrent uses of one code: the login pool's connections (`identity::login::POOL_CONNECTIONS`).
const RACERS: usize = 4;

// The service's log messages (services/identity/src/login.rs and logout.rs).
const SENT: &str = "sent a login code";
const NO_USER: &str = "a login code was asked for an address no user has; nothing sent";
const THROTTLED: &str = "did not send a login code: the address is at its send limit";
const SAME_MOMENT: &str = "did not send a login code: another one was issued at the same moment";
const SEND_FAILED: &str = "could not send a login code";
const LOGGED_IN: &str = "logged in with an email code";
const REFUSED_CODE: &str = "refused a login code";
const REFUSED_CROSS_SITE: &str = "refused a login request";
const NO_RELAY: &str =
    "TRUSTIOT_SMTP_HOST is not set (the chart's smtp.host): nobody can get a login code";
const UNUSABLE_RELAY: &str = "the SMTP settings cannot be used: nobody can get a login code until \
                              they are fixed and the service restarts";

const INSERT_USER: &str = "INSERT INTO identity.users (id, email) VALUES ($1::uuid, $2)";
const INSERT_MEMBERSHIP: &str = "INSERT INTO identity.memberships (tenant_id, user_id, tenant_role)
VALUES ($1::uuid, $2::uuid, $3)";
const UPDATE_ROLE: &str = "UPDATE identity.memberships SET tenant_role = $3
WHERE tenant_id = $1::uuid AND user_id = $2::uuid";
const DELETE_MEMBERSHIP: &str =
    "DELETE FROM identity.memberships WHERE tenant_id = $1::uuid AND user_id = $2::uuid";
/// A session the test tooling makes, as on another device: `$3` tenant, `$4` started seconds
/// ago, `$5` expires in seconds.
const INSERT_SESSION: &str = "INSERT INTO identity.sessions
    (id, token_hash, user_id, active_tenant_id, created_at, expires_at)
VALUES (gen_random_uuid(), $1, $2::uuid, $3::uuid,
    now() - make_interval(secs => $4), now() + make_interval(secs => $5))";
const SESSIONS_OF: &str = "SELECT count(*) FROM identity.sessions WHERE user_id = $1::uuid";
const LIVE_SESSIONS_OF: &str = "SELECT count(*) FROM identity.sessions
WHERE user_id = $1::uuid AND revoked_at IS NULL AND expires_at > now()";
/// The lifetime of the user's newest session, and of its expiry from now, in seconds.
const NEWEST_SESSION: &str = "SELECT
    EXTRACT(EPOCH FROM expires_at - created_at)::float8,
    EXTRACT(EPOCH FROM expires_at - now())::float8
FROM identity.sessions WHERE user_id = $1::uuid ORDER BY created_at DESC LIMIT 1";
const SESSION_EXPIRY: &str = "SELECT
    EXTRACT(EPOCH FROM expires_at - now())::float8,
    expires_at <= created_at + interval '7 days'
FROM identity.sessions WHERE token_hash = $1";
/// The user's newest code: attempts, consumed, invalidated.
const NEWEST_CODE: &str = "SELECT attempts, consumed_at IS NOT NULL, invalidated_at IS NOT NULL
FROM identity.login_codes WHERE user_id = $1::uuid ORDER BY created_at DESC LIMIT 1";
const CODES_OF: &str = "SELECT count(*) FROM identity.login_codes WHERE user_id = $1::uuid";
/// Whether the user's newest code row holds the plain SHA-256 of `$2`.
const UNKEYED_HASH_MATCHES: &str = "SELECT code_hash = sha256(convert_to($2, 'UTF8'))
FROM identity.login_codes WHERE user_id = $1::uuid ORDER BY created_at DESC LIMIT 1";
/// Moves the user's codes 2 minutes into the past, beyond the resend interval.
const AGE_CODES: &str =
    "UPDATE identity.login_codes SET created_at = created_at - interval '2 minutes'
WHERE user_id = $1::uuid";
/// Expires the user's codes a minute ago.
const EXPIRE_CODES: &str = "UPDATE identity.login_codes
SET created_at = now() - interval '11 minutes', expires_at = now() - interval '1 minute'
WHERE user_id = $1::uuid";
/// `$2` used-up codes created `$3` to `$3 + $2 - 1` minutes ago, for the send limits.
const PAST_CODES: &str = "INSERT INTO identity.login_codes
    (user_id, code_hash, created_at, expires_at, invalidated_at)
SELECT $1::uuid, sha256(convert_to(g::text, 'UTF8')),
    now() - make_interval(mins => $3 + g - 1),
    now() - make_interval(mins => $3 + g - 1) + interval '10 minutes', now()
FROM generate_series(1, $2) AS g";

/// A user of this test binary with an address under example.com.
struct User {
    id: String,
    email: String,
}

async fn user(admin: &mut PgConnection, tag: &str) -> User {
    let id = contract::UserId::generate().to_string();
    // The user ID keeps the address unique across runs against a reused database.
    let email = format!("login-{tag}-{}@example.com", id.replace('-', ""));
    sqlx::query(INSERT_USER)
        .bind(&id)
        .bind(&email)
        .execute(&mut *admin)
        .await
        .expect("insert the user");
    User { id, email }
}

async fn run(admin: &mut PgConnection, statement: &'static str, binds: &[&str]) {
    let mut query = sqlx::query(statement);
    for value in binds {
        query = query.bind(*value);
    }
    query
        .execute(&mut *admin)
        .await
        .unwrap_or_else(|error| panic!("{statement}: {error}"));
}

async fn count(admin: &mut PgConnection, statement: &'static str, user: &str) -> i64 {
    sqlx::query_scalar(statement)
        .bind(user)
        .fetch_one(&mut *admin)
        .await
        .unwrap_or_else(|error| panic!("{statement}: {error}"))
}

async fn newest_code(admin: &mut PgConnection, user: &str) -> (i32, bool, bool) {
    sqlx::query_as(NEWEST_CODE)
        .bind(user)
        .fetch_one(&mut *admin)
        .await
        .expect("the user's newest code")
}

/// The identity service on the product database, with `relay` as its SMTP relay (none when
/// `None`) and `extra` settings on top.
fn start(product: &ProductPostgres, relay: Option<&FakeSmtp>, extra: &[(&str, &str)]) -> Service {
    let env = service_env(product, relay, extra);
    let env: Vec<(&str, &str)> = env.iter().map(|(n, v)| (n.as_str(), v.as_str())).collect();
    let service = Service::start(&env);
    service.wait_ready(Duration::from_secs(60));
    service
}

/// Every setting [`start`] gives the service.
fn service_env(
    product: &ProductPostgres,
    relay: Option<&FakeSmtp>,
    extra: &[(&str, &str)],
) -> Vec<(String, String)> {
    let files = KeyFiles::new("login", &TestKey::new(103), KID);
    let routes = json!([
        {"host": APP, "path": "/api/identity", "match": "PathPrefix", "service": "identity"},
        {"host": APP, "path": "/api/assets", "match": "PathPrefix", "service": "assets"},
    ]);
    let mut env = files.env();
    env.extend([
        ("TRUSTIOT_INGRESS_ROUTES".into(), routes.to_string()),
        ("TRUSTIOT__DATABASE__USERNAME".into(), "identity_app".into()),
        ("TRUSTIOT__DATABASE__PASSWORD".into(), TEST_PASSWORD.into()),
    ]);
    env.extend(
        product
            .platform_env()
            .into_iter()
            .map(|(name, value)| (name.to_owned(), value)),
    );
    if let Some(relay) = relay {
        env.extend(relay.env());
    }
    for (name, value) in extra {
        env.retain(|(existing, _)| existing != name);
        env.push(((*name).to_owned(), (*value).to_owned()));
    }
    env
}

fn post_login(service: &Service, body: &Value, cookie: Option<&str>) -> support::Response {
    post_login_raw(service, LOGIN, &body.to_string(), cookie)
}

fn post_login_raw(
    service: &Service,
    target: &str,
    body: &str,
    cookie: Option<&str>,
) -> support::Response {
    let mut headers = vec![("Host", APP), ("Content-Type", "application/json")];
    if let Some(cookie) = cookie {
        headers.push(("Cookie", cookie));
    }
    request_with_body(service.address, "POST", target, &headers, body.as_bytes())
        .expect("call the login endpoint")
}

fn send_code(service: &Service, email: &str) -> support::Response {
    post_login(service, &json!({"step": "send_code", "email": email}), None)
}

fn verify_code(
    service: &Service,
    email: &str,
    code: &str,
    redirect_to: Option<&str>,
    cookie: Option<&str>,
) -> support::Response {
    let mut body = json!({"step": "verify_code", "email": email, "code": code});
    if let Some(redirect_to) = redirect_to {
        body["redirect_to"] = json!(redirect_to);
    }
    post_login(service, &body, cookie)
}

/// [`verify_code`] without a redirect or a cookie, and how long the answer took.
fn timed_verify(service: &Service, email: &str, code: &str) -> (support::Response, Duration) {
    let started = Instant::now();
    let response = verify_code(service, email, code, None, None);
    (response, started.elapsed())
}

/// The code in a delivered email: its first run of exactly `CODE_DIGITS` digits.
fn code_in(delivery: &Delivery) -> String {
    let body = delivery.body_text();
    body.split(|c: char| !c.is_ascii_digit())
        .find(|run| run.len() == CODE_DIGITS)
        .unwrap_or_else(|| panic!("no code in the email:\n{body}"))
        .to_owned()
}

/// Whether `text` holds `digits` with no digit right before or after it, so a code is not
/// mistaken for part of a timestamp or another number.
fn contains_number(text: &str, digits: &str) -> bool {
    text.match_indices(digits).any(|(start, _)| {
        let before = text.get(..start).and_then(|head| head.chars().next_back());
        let after = text
            .get(start + digits.len()..)
            .and_then(|tail| tail.chars().next());
        !before.is_some_and(|c| c.is_ascii_digit()) && !after.is_some_and(|c| c.is_ascii_digit())
    })
}

/// A code that is not `code`: the next number, wrapping.
fn wrong(code: &str) -> String {
    let value: u32 = code.parse().expect("digits");
    format!("{:08}", (value + 1) % 100_000_000)
}

/// Asks for a code for `email` and returns it from the `index`-th message the relay receives
/// (0-based).
fn code_for(service: &Service, relay: &FakeSmtp, email: &str, index: usize) -> String {
    let response = send_code(service, email);
    assert_eq!(response.status, 202, "{}", response.raw());
    let deliveries = relay.wait_for_deliveries(index + 1, MAIL_WITHIN);
    let delivery = &deliveries[index];
    assert_eq!(delivery.rcpt_to, [email.to_owned()]);
    code_in(delivery)
}

/// The `Cookie` header value of a login answer's session cookie.
fn session_cookie(response: &support::Response) -> String {
    let set = response.header_values("set-cookie");
    let [value] = set.as_slice() else {
        panic!("expected one Set-Cookie: {}", response.raw());
    };
    let (pair, attributes) = value.split_once("; ").expect("cookie attributes");
    assert_eq!(attributes, COOKIE_ATTRIBUTES, "{value}");
    let secret = pair
        .strip_prefix(&format!("{SESSION_COOKIE}="))
        .expect("the session cookie");
    assert_eq!(secret.len(), 43, "32 bytes in base64url: {value}");
    pair.to_owned()
}

/// The ingress gateway's cookie check for a request to `path`.
fn check(service: &Service, cookie: &str, path: &str) -> support::Response {
    request(
        service.address,
        "POST",
        &format!("/internal/ext-authz{path}"),
        &[("Host", APP), ("Cookie", cookie)],
    )
    .expect("call the ext_authz check")
}

/// The claims of the token the check signed (the signature is the ext_authz tests' concern).
fn claims(response: &support::Response) -> Value {
    assert_eq!(response.status, 200, "{}", response.raw());
    let token = response.header_values("x-internal-identity")[0];
    let payload = token.split('.').nth(1).expect("a payload segment");
    serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).expect("base64url")).expect("JSON")
}

/// A logout request as the ingress forwards it: the cookie, and the token its check signed.
fn log_out(service: &Service, cookie: &str, path: &str) -> support::Response {
    let token = check(service, cookie, path);
    assert_eq!(token.status, 200, "{}", token.raw());
    let token = token.header_values("x-internal-identity")[0].to_owned();
    request(
        service.address,
        "DELETE",
        path,
        &[
            ("Host", APP),
            ("Cookie", cookie),
            ("x-internal-identity", &token),
        ],
    )
    .expect("call logout")
}

fn error_code(response: &support::Response) -> Value {
    response.json()["code"].clone()
}

/// The whole flow, at log level TRACE: the code reaches the relay over TLS only, logs in once,
/// sets the cookie as the docs say, and appears in no URL, no response and no log line.
#[tokio::test(flavor = "multi_thread")]
async fn a_code_from_the_email_logs_in_once() {
    let Some(product) = product_postgres(&["identity_app"]).await else {
        return;
    };
    let mut admin = product.superuser().await;
    let relay = FakeSmtp::start(Mode::StartTls);
    let service = start(
        &product,
        Some(&relay),
        &[("TRUSTIOT__TELEMETRY__LOG_FILTER", "trace")],
    );
    let alice = user(&mut admin, "flow").await;
    let mut responses = Vec::new();

    // 1. Step one answers 202 at once, not cached.
    let sent = send_code(&service, &format!("  {}  ", alice.email.to_uppercase()));
    assert_eq!(sent.status, 202, "{}", sent.raw());
    assert_eq!(
        sent.json(),
        json!({"code_length": 8, "expires_in_secs": 600, "resend_after_secs": 60})
    );
    assert_eq!(sent.header_values("cache-control"), ["no-store"]);
    responses.push(sent.raw());

    // 2. The email: to the normalized address, over STARTTLS with AUTH, the code in the body
    //    only, and no link.
    let deliveries = relay.wait_for_deliveries(1, MAIL_WITHIN);
    let [mail] = deliveries.as_slice() else {
        panic!("one email: {deliveries:?}");
    };
    assert_eq!(mail.rcpt_to, std::slice::from_ref(&alice.email));
    assert_eq!(mail.mail_from, "noreply@example.com");
    let expected_auth = format!("\0{}\0{}", relay.account.username, relay.account.password);
    assert_eq!(mail.auth.as_deref(), Some(expected_auth.as_str()));
    let plain = relay.plain_commands();
    assert!(
        plain.len() == 2 && plain[0].starts_with("EHLO ") && plain[1] == "STARTTLS",
        "nothing but EHLO and STARTTLS before TLS: {plain:?}"
    );
    assert_eq!(mail.header("To"), Some(format!("<{}>", alice.email)));
    let code = code_in(mail);
    let (head, _) = mail.data.split_once("\r\n\r\n").expect("headers");
    assert!(!contains_number(head, &code), "the code is in a header");
    let body = mail.body_text();
    assert!(
        !body.contains("://") && !body.contains("www."),
        "the email has a link: {body}"
    );
    assert!(service.stdout.wait_for_message(SENT, LOG_WITHIN).is_some());
    // The table holds a keyed hash: neither hash a reader of the database could test every code
    // against (src/login.rs, `code_hash`).
    for unkeyed in [
        format!("trustiot-login-code:v1\n{}\n{code}", alice.email),
        code.clone(),
    ] {
        let matches: bool = sqlx::query_scalar(UNKEYED_HASH_MATCHES)
            .bind(&alice.id)
            .bind(&unkeyed)
            .fetch_one(&mut admin)
            .await
            .expect("compare the stored hash");
        assert!(!matches, "the stored code hash is an unkeyed SHA-256");
    }

    // 3. The code in a URL is refused before it is read, and does not count as an attempt.
    let in_url = post_login_raw(
        &service,
        &format!("{LOGIN}?code={code}"),
        &json!({"step": "verify_code", "email": alice.email, "code": code}).to_string(),
        None,
    );
    assert_eq!(in_url.status, 400, "{}", in_url.raw());
    assert_eq!(error_code(&in_url), "invalid_request");
    assert_eq!(newest_code(&mut admin, &alice.id).await, (0, false, false));
    responses.push(in_url.raw());

    // 4. The right code logs in: the session cookie with its attributes, the validated
    //    redirect, no caching, a session of 12 hours.
    let logged_in = verify_code(
        &service,
        &alice.email,
        &code,
        Some("/devices?page=2#top"),
        None,
    );
    assert_eq!(logged_in.status, 200, "{}", logged_in.raw());
    assert_eq!(
        logged_in.json(),
        json!({"redirect_to": "/devices?page=2#top"})
    );
    assert_eq!(logged_in.header_values("cache-control"), ["no-store"]);
    let session = session_cookie(&logged_in);
    let lifetime: (f64, f64) = sqlx::query_as(NEWEST_SESSION)
        .bind(&alice.id)
        .fetch_one(&mut admin)
        .await
        .expect("the new session");
    assert!(
        (lifetime.0 - 12.0 * 3600.0).abs() < 5.0,
        "a new session lives 12 hours: {lifetime:?}"
    );
    assert_eq!(newest_code(&mut admin, &alice.id).await, (1, true, false));
    responses.push(logged_in.raw().replace(&session, "<cookie>"));
    assert!(
        service
            .stdout
            .wait_for_message(LOGGED_IN, LOG_WITHIN)
            .is_some()
    );

    // 5. The session passes the cookie check as the user.
    let passed = check(&service, &session, LOGOUT_EVERYWHERE);
    assert_eq!(claims(&passed)["sub"], alice.id.as_str());

    // 6. The same code again: refused, and no second session.
    let replay = verify_code(&service, &alice.email, &code, None, None);
    assert_eq!(replay.status, 401, "{}", replay.raw());
    assert_eq!(error_code(&replay), "unauthenticated");
    assert!(replay.header_values("set-cookie").is_empty());
    assert_eq!(count(&mut admin, SESSIONS_OF, &alice.id).await, 1);
    responses.push(replay.raw());

    // 7. Neither the code nor the address is in any response or log line; neither are the
    //    session cookie and the relay's password.
    let logs = service.stop();
    assert!(logs.lines().count() > 50, "TRACE logs: {logs}");
    for response in &responses {
        assert!(!contains_number(response, &code), "{response}");
    }
    assert!(!contains_number(&logs, &code), "the code is in the logs");
    let secret = session
        .strip_prefix(&format!("{SESSION_COOKIE}="))
        .expect("cookie value");
    for (what, value) in [
        ("the address", alice.email.as_str()),
        ("the session cookie", secret),
        ("the relay's password", relay.account.password.as_str()),
    ] {
        assert!(!logs.contains(value), "{what} is in the logs");
    }
}

/// A code works once, until it expires, for the address it was sent to, and dies after
/// `MAX_ATTEMPTS` attempts; a newer code replaces it; at most one of concurrent uses wins.
#[tokio::test(flavor = "multi_thread")]
async fn codes_are_used_once_and_die_after_five_attempts() {
    let Some(product) = product_postgres(&["identity_app"]).await else {
        return;
    };
    let mut admin = product.superuser().await;
    let relay = FakeSmtp::start(Mode::Tls);
    let service = start(&product, Some(&relay), &[]);
    let mut mails = 0;
    let mut next_code = |email: &str| {
        let code = code_for(&service, &relay, email, mails);
        mails += 1;
        code
    };

    // 1. Five wrong codes: each refused and counted; then the right one is refused too. Each
    //    refusal takes at least `REFUSAL_FLOOR`, as one for an address without a code does
    //    (step 7), so the time does not tell whether the address has an account.
    let guessed = user(&mut admin, "guessed").await;
    let code = next_code(&guessed.email);
    for attempt in 1..=MAX_ATTEMPTS {
        let (response, took) = timed_verify(&service, &guessed.email, &wrong(&code));
        assert_eq!(
            response.status,
            401,
            "attempt {attempt}: {}",
            response.raw()
        );
        assert!(took >= REFUSAL_FLOOR, "attempt {attempt} took {took:?}");
        assert_eq!(
            newest_code(&mut admin, &guessed.id).await,
            (attempt, false, false)
        );
    }
    let response = verify_code(&service, &guessed.email, &code, None, None);
    assert_eq!(response.status, 401, "{}", response.raw());
    assert_eq!(
        newest_code(&mut admin, &guessed.id).await,
        (MAX_ATTEMPTS, false, false),
        "a dead code is not even counted"
    );
    assert_eq!(count(&mut admin, SESSIONS_OF, &guessed.id).await, 0);

    // 2. Four wrong codes leave the fifth attempt, which the right code uses.
    let careless = user(&mut admin, "careless").await;
    let code = next_code(&careless.email);
    for _ in 1..MAX_ATTEMPTS {
        let response = verify_code(&service, &careless.email, &wrong(&code), None, None);
        assert_eq!(response.status, 401, "{}", response.raw());
    }
    let response = verify_code(&service, &careless.email, &code, None, None);
    assert_eq!(response.status, 200, "{}", response.raw());

    // 3. An expired code is refused.
    let late = user(&mut admin, "late").await;
    let code = next_code(&late.email);
    run(&mut admin, EXPIRE_CODES, &[&late.id]).await;
    let response = verify_code(&service, &late.email, &code, None, None);
    assert_eq!(response.status, 401, "{}", response.raw());
    assert_eq!(count(&mut admin, SESSIONS_OF, &late.id).await, 0);

    // 4. A code logs in only the address it was sent to: one user's code is a wrong code for
    //    another, while both stay usable by their own address.
    let first_user = user(&mut admin, "bound-a").await;
    let second_user = user(&mut admin, "bound-b").await;
    let first_code = next_code(&first_user.email);
    let second_code = next_code(&second_user.email);
    let response = verify_code(&service, &second_user.email, &first_code, None, None);
    assert_eq!(response.status, 401, "{}", response.raw());
    for (who, code) in [(&first_user, &first_code), (&second_user, &second_code)] {
        let response = verify_code(&service, &who.email, code, None, None);
        assert_eq!(response.status, 200, "{}", response.raw());
    }

    // 5. A newer code invalidates the older one.
    let twice = user(&mut admin, "twice").await;
    let first = next_code(&twice.email);
    run(&mut admin, AGE_CODES, &[&twice.id]).await;
    let second = next_code(&twice.email);
    let response = verify_code(&service, &twice.email, &first, None, None);
    assert_eq!(response.status, 401, "{}", response.raw());
    let response = verify_code(&service, &twice.email, &second, None, None);
    assert_eq!(response.status, 200, "{}", response.raw());

    // 6. Four requests with the same code at once (as many as the login pool has connections):
    //    exactly one logs in.
    let racer = user(&mut admin, "racer").await;
    let code = next_code(&racer.email);
    let start_line = Arc::new(Barrier::new(RACERS));
    let racers: Vec<_> = (0..RACERS)
        .map(|_| {
            let (start_line, address, email, code) = (
                Arc::clone(&start_line),
                service.address,
                racer.email.clone(),
                code.clone(),
            );
            thread::spawn(move || {
                let body = json!({"step": "verify_code", "email": email, "code": code}).to_string();
                start_line.wait();
                request_with_body(
                    address,
                    "POST",
                    LOGIN,
                    &[("Host", APP), ("Content-Type", "application/json")],
                    body.as_bytes(),
                )
                .expect("call the login endpoint")
                .status
            })
        })
        .collect();
    let mut statuses: Vec<u16> = racers
        .into_iter()
        .map(|racer| racer.join().expect("racer"))
        .collect();
    statuses.sort_unstable();
    assert_eq!(statuses, [200, 401, 401, 401]);
    assert_eq!(count(&mut admin, SESSIONS_OF, &racer.id).await, 1);
    assert_eq!(newest_code(&mut admin, &racer.id).await, (1, true, false));

    // 7. An address with a user but no code, and one without a user: refused like a wrong code,
    //    and no sooner than `REFUSAL_FLOOR` either.
    let idle = user(&mut admin, "idle").await;
    let nobody = format!(
        "login-nobody-{}@example.com",
        contract::UserId::generate().to_string().replace('-', "")
    );
    for email in [idle.email.as_str(), nobody.as_str()] {
        let (response, took) = timed_verify(&service, email, "12345678");
        assert_eq!(response.status, 401, "{}", response.raw());
        assert_eq!(error_code(&response), "unauthenticated");
        assert!(took >= REFUSAL_FLOOR, "took {took:?}");
    }
    assert_eq!(count(&mut admin, CODES_OF, &idle.id).await, 0);
}

/// `redirect_to` is a path on this site or the request is refused, before the code is looked at.
#[tokio::test(flavor = "multi_thread")]
async fn redirects_stay_on_this_site() {
    let Some(product) = product_postgres(&["identity_app"]).await else {
        return;
    };
    let mut admin = product.superuser().await;
    let relay = FakeSmtp::start(Mode::StartTls);
    let service = start(&product, Some(&relay), &[]);
    let bob = user(&mut admin, "redirect").await;
    let code = code_for(&service, &relay, &bob.email, 0);
    for target in [
        "https://evil.example/",
        "http://app.saas.trustiot.test/devices",
        "//evil.example",
        "/\\evil.example",
        "\\\\evil.example",
        "/\t/evil.example",
        "/ /evil.example",
        "javascript:alert(1)",
        "evil.example",
        "",
        "/caf\u{e9}",
        "/\r\nSet-Cookie: x=y",
    ] {
        let response = verify_code(&service, &bob.email, &code, Some(target), None);
        assert_eq!(response.status, 422, "{target:?}: {}", response.raw());
        assert_eq!(
            response.json(),
            json!({
                "code": "validation_failed",
                "message_key": "error.validation_failed",
                "field_errors": [{"field": "/redirect_to", "message_key": "login.redirect_invalid"}]
            }),
            "{target:?}"
        );
        assert!(response.header_values("set-cookie").is_empty());
    }
    let too_long = format!("/{}", "a".repeat(2048));
    let response = verify_code(&service, &bob.email, &code, Some(&too_long), None);
    assert_eq!(response.status, 422, "{}", response.raw());
    // None of them was an attempt on the code.
    assert_eq!(newest_code(&mut admin, &bob.id).await, (0, false, false));
    let response = verify_code(&service, &bob.email, &code, Some("/alerts/42?x=1"), None);
    assert_eq!(response.status, 200, "{}", response.raw());
    assert_eq!(response.json(), json!({"redirect_to": "/alerts/42?x=1"}));
}

/// Logging out ends the session; logging out everywhere ends every session of the user, each
/// refused on its next request, and no one else's; logging in again ends the browser's old
/// session.
#[tokio::test(flavor = "multi_thread")]
async fn logging_out_refuses_the_sessions_next_request() {
    let Some(product) = product_postgres(&["identity_app"]).await else {
        return;
    };
    let mut admin = product.superuser().await;
    let relay = FakeSmtp::start(Mode::StartTls);
    let service = start(&product, Some(&relay), &[]);
    let carol = user(&mut admin, "logout").await;
    let dave = user(&mut admin, "bystander").await;

    // Carol on three devices: the browser that logs in here, and two the tooling adds.
    let code = code_for(&service, &relay, &carol.email, 0);
    let response = verify_code(&service, &carol.email, &code, None, None);
    let browser = session_cookie(&response);
    let mut devices = Vec::new();
    for owner in [&carol.id, &carol.id, &dave.id] {
        let secret = new_secret();
        sqlx::query(INSERT_SESSION)
            .bind(secret.hash().as_bytes().as_slice())
            .bind(owner)
            .bind(None::<String>)
            .bind(60.0)
            .bind(12.0 * 3600.0)
            .execute(&mut admin)
            .await
            .expect("insert a session");
        devices.push(cookie(&secret));
    }
    let [phone, tablet, daves] = devices.as_slice() else {
        unreachable!("three sessions");
    };
    for session in [&browser, phone, tablet, daves] {
        assert_eq!(check(&service, session, LOGOUT).status, 200);
    }

    // 1. Logging out ends this session only, and clears the cookie.
    let response = log_out(&service, &browser, LOGOUT);
    assert_eq!(response.status, 204, "{}", response.raw());
    assert_eq!(response.header_values("set-cookie"), [CLEARED_COOKIE]);
    assert_eq!(response.header_values("cache-control"), ["no-store"]);
    assert_eq!(check(&service, &browser, LOGOUT).status, 401);
    assert_eq!(check(&service, phone, LOGOUT).status, 200);

    // 2. Without the session cookie there is nothing to log out.
    let token = check(&service, phone, LOGOUT);
    let token = token.header_values("x-internal-identity")[0].to_owned();
    let response = request(
        service.address,
        "DELETE",
        LOGOUT,
        &[("Host", APP), ("x-internal-identity", &token)],
    )
    .expect("call logout");
    assert_eq!(response.status, 401, "{}", response.raw());
    // And without the token neither.
    let response = request(
        service.address,
        "DELETE",
        LOGOUT_EVERYWHERE,
        &[("Host", APP), ("Cookie", phone)],
    )
    .expect("call logout");
    assert_eq!(response.status, 401, "{}", response.raw());
    assert_eq!(check(&service, phone, LOGOUT).status, 200);

    // 3. Logging out everywhere from the phone: the phone and the tablet are refused on their
    //    next request; Dave is not affected.
    let response = log_out(&service, phone, LOGOUT_EVERYWHERE);
    assert_eq!(response.status, 204, "{}", response.raw());
    assert_eq!(response.header_values("set-cookie"), [CLEARED_COOKIE]);
    assert_eq!(check(&service, phone, LOGOUT).status, 401);
    assert_eq!(check(&service, tablet, LOGOUT).status, 401);
    assert_eq!(count(&mut admin, LIVE_SESSIONS_OF, &carol.id).await, 0);
    assert_eq!(check(&service, daves, LOGOUT).status, 200);

    // 4. Logging in again while the browser still holds a session ends that session.
    let erin = user(&mut admin, "relogin").await;
    let code = code_for(&service, &relay, &erin.email, 1);
    let first = session_cookie(&verify_code(&service, &erin.email, &code, None, None));
    run(&mut admin, AGE_CODES, &[&erin.id]).await;
    let code = code_for(&service, &relay, &erin.email, 2);
    let second = session_cookie(&verify_code(
        &service,
        &erin.email,
        &code,
        None,
        Some(&first),
    ));
    assert_eq!(check(&service, &first, LOGOUT).status, 401);
    assert_eq!(check(&service, &second, LOGOUT).status, 200);
}

/// The role comes from the membership on every request: changed, it applies to the same
/// session's next request; removed, the session acts in no tenant. A new session acts in the
/// tenant of the user's previous one.
#[tokio::test(flavor = "multi_thread")]
async fn a_role_change_applies_to_the_next_request() {
    let Some(product) = product_postgres(&["identity_app"]).await else {
        return;
    };
    let mut admin = product.superuser().await;
    let relay = FakeSmtp::start(Mode::StartTls);
    let service = start(&product, Some(&relay), &[]);
    let frank = user(&mut admin, "roles").await;
    let tenant = contract::TenantId::generate().to_string();
    run(
        &mut admin,
        INSERT_MEMBERSHIP,
        &[&tenant, &frank.id, "viewer"],
    )
    .await;
    // An earlier session in the tenant, as the tooling makes it (switching tenants is
    // Laysi/trustiot-v2#497).
    let earlier = new_secret();
    sqlx::query(INSERT_SESSION)
        .bind(earlier.hash().as_bytes().as_slice())
        .bind(&frank.id)
        .bind(&tenant)
        .bind(3600.0)
        .bind(60.0)
        .execute(&mut admin)
        .await
        .expect("insert the earlier session");

    let code = code_for(&service, &relay, &frank.email, 0);
    let session = session_cookie(&verify_code(&service, &frank.email, &code, None, None));
    let grant = claims(&check(&service, &session, LOGOUT));
    assert_eq!(grant["tenant"], json!({"id": tenant, "role": "viewer"}));

    run(&mut admin, UPDATE_ROLE, &[&tenant, &frank.id, "admin"]).await;
    let grant = claims(&check(&service, &session, LOGOUT));
    assert_eq!(grant["tenant"], json!({"id": tenant, "role": "admin"}));

    run(&mut admin, DELETE_MEMBERSHIP, &[&tenant, &frank.id]).await;
    let grant = claims(&check(&service, &session, LOGOUT));
    assert!(grant["tenant"].is_null(), "{grant}");
}

/// Codes go out at most once a minute, five an hour and twenty a day per address; an unknown
/// address gets the same answer and nothing is sent.
#[tokio::test(flavor = "multi_thread")]
async fn sending_is_limited_and_tells_nobody_who_has_an_account() {
    let Some(product) = product_postgres(&["identity_app"]).await else {
        return;
    };
    let mut admin = product.superuser().await;
    let relay = FakeSmtp::start(Mode::StartTls);
    let service = start(&product, Some(&relay), &[]);

    // 1. An address no user has: the same 202, no email, no row.
    let nobody = "login-nobody@example.org";
    let known = user(&mut admin, "limits").await;
    let unknown_answer = send_code(&service, nobody);
    assert!(
        service.stdout.wait_for_messages(NO_USER, 1, LOG_WITHIN),
        "the unknown address was handled"
    );
    let known_answer = send_code(&service, &known.email);
    assert_eq!(unknown_answer.status, known_answer.status);
    assert_eq!(unknown_answer.body, known_answer.body);
    let deliveries = relay.wait_for_deliveries(1, MAIL_WITHIN);
    assert_eq!(deliveries.len(), 1);
    assert_eq!(deliveries[0].rcpt_to, std::slice::from_ref(&known.email));

    // 2. Again within a minute: the same answer, nothing sent, no new row.
    let again = send_code(&service, &known.email);
    assert_eq!(again.status, 202);
    assert!(service.stdout.wait_for_messages(THROTTLED, 1, LOG_WITHIN));
    assert_eq!(count(&mut admin, CODES_OF, &known.id).await, 1);

    // 3. Five codes in the last hour (2 to 6 minutes ago), or twenty in the last day (61 to 80
    //    minutes ago), stop the next one.
    let hourly = user(&mut admin, "hourly").await;
    let daily = user(&mut admin, "daily").await;
    for (who, codes, minutes_ago) in [(&hourly, 5_i32, 2_i32), (&daily, 20, 61)] {
        sqlx::query(PAST_CODES)
            .bind(&who.id)
            .bind(codes)
            .bind(minutes_ago)
            .execute(&mut admin)
            .await
            .expect("insert past codes");
    }
    let throttled = service.stdout.count_messages(THROTTLED);
    for who in [&hourly, &daily] {
        assert_eq!(send_code(&service, &who.email).status, 202);
    }
    assert!(
        service
            .stdout
            .wait_for_messages(THROTTLED, throttled + 2, LOG_WITHIN)
    );
    assert_eq!(relay.deliveries().len(), 1, "nothing more was sent");

    // 4. Eight requests for one address at once: one code and one email, the others are at the
    //    resend limit.
    const BURST: usize = 8;
    let burst = user(&mut admin, "burst").await;
    let handled = |stdout: &support::Output| {
        stdout.count_messages(SENT)
            + stdout.count_messages(THROTTLED)
            + stdout.count_messages(SAME_MOMENT)
    };
    let before = handled(&service.stdout);
    let start_line = Arc::new(Barrier::new(BURST));
    let senders: Vec<_> = (0..BURST)
        .map(|_| {
            let (start_line, address, email) = (
                Arc::clone(&start_line),
                service.address,
                burst.email.clone(),
            );
            thread::spawn(move || {
                let body = json!({"step": "send_code", "email": email}).to_string();
                start_line.wait();
                request_with_body(
                    address,
                    "POST",
                    LOGIN,
                    &[("Host", APP), ("Content-Type", "application/json")],
                    body.as_bytes(),
                )
                .expect("call the login endpoint")
                .status
            })
        })
        .collect();
    for sender in senders {
        assert_eq!(sender.join().expect("sender"), 202);
    }
    let deadline = Instant::now() + LOG_WITHIN;
    while handled(&service.stdout) < before + BURST {
        assert!(
            Instant::now() < deadline,
            "the worker did not handle all {BURST} requests for one address within {LOG_WITHIN:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let to_burst = relay
        .deliveries()
        .iter()
        .filter(|delivery| delivery.rcpt_to == [burst.email.clone()])
        .count();
    assert_eq!(to_burst, 1, "one email for {BURST} requests at once");
    assert_eq!(count(&mut admin, CODES_OF, &burst.id).await, 1);

    // 5. No address, code or password is in the logs.
    let logs = service.stop();
    for value in [
        nobody,
        known.email.as_str(),
        hourly.email.as_str(),
        daily.email.as_str(),
    ] {
        assert!(!logs.contains(value), "an address is in the logs");
    }
}

/// The relay must speak TLS with a certificate the configured CA signed: a relay that does not
/// offer STARTTLS, one that sends bytes before the handshake, and one with another CA's
/// certificate get no command beyond EHLO and STARTTLS in plain text and no message, and the code
/// is invalidated. TLS from the first byte works.
#[tokio::test(flavor = "multi_thread")]
async fn codes_travel_only_over_verified_tls() {
    let Some(product) = product_postgres(&["identity_app"]).await else {
        return;
    };
    let mut admin = product.superuser().await;
    let stranger = TestCa::new("identity-login-test-other-ca");
    let other_ca = scratch_dir("smtp-other-ca").join("ca.pem");
    std::fs::write(&other_ca, &stranger.pem).expect("write the other CA");
    let other_ca = other_ca.display().to_string();
    for (mode, ca_file) in [
        (Mode::NoStartTls, None),
        (Mode::InjectAfterStartTls, None),
        (Mode::StartTls, Some(other_ca.as_str())),
    ] {
        let relay = FakeSmtp::start(mode);
        let extra: Vec<(&str, &str)> = ca_file
            .map(|file| vec![("TRUSTIOT_SMTP_CA_FILE", file)])
            .unwrap_or_default();
        let service = start(&product, Some(&relay), &extra);
        let victim = user(&mut admin, "tls").await;
        assert_eq!(send_code(&service, &victim.email).status, 202);
        assert!(
            service
                .stdout
                .wait_for_message(SEND_FAILED, LOG_WITHIN)
                .is_some(),
            "{mode:?}: no failure logged\n{}",
            service.stdout.text()
        );
        let deadline = Instant::now() + WRITE_WITHIN;
        while !newest_code(&mut admin, &victim.id).await.2 {
            assert!(
                Instant::now() < deadline,
                "{mode:?}: the unsent code was not invalidated"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(relay.deliveries().is_empty(), "{mode:?}");
        let plain = relay.plain_commands();
        assert!(
            plain
                .iter()
                .all(|line| line.starts_with("EHLO ") || line == "STARTTLS"),
            "{mode:?}: {plain:?}"
        );
        let logs = service.stop();
        assert!(!logs.contains(&relay.account.password), "{mode:?}");
    }
}

/// Without a relay nobody can get a code (503). A relay setting that is incomplete is logged at
/// ERROR without printing any value, and the service still serves (the ext_authz check must not
/// depend on the relay), answering `send_code` with 503. That includes a relay that needs an
/// account without one: the chart's optional reference to a missing Secret
/// `smtp.credentialsSecret` leaves out both variables.
#[tokio::test(flavor = "multi_thread")]
async fn a_missing_or_incomplete_relay_is_reported() {
    let Some(product) = product_postgres(&["identity_app"]).await else {
        return;
    };
    let mut admin = product.superuser().await;
    let service = start(&product, None, &[]);
    assert!(
        service
            .stdout
            .wait_for_message(NO_RELAY, LOG_WITHIN)
            .is_some()
    );
    let grace = user(&mut admin, "norelay").await;
    let response = send_code(&service, &grace.email);
    assert_eq!(response.status, 503, "{}", response.raw());
    assert_eq!(error_code(&response), "unavailable");
    assert_eq!(count(&mut admin, CODES_OF, &grace.id).await, 0);
    drop(service);

    let relay = FakeSmtp::start(Mode::StartTls);
    let base = service_env(&product, Some(&relay), &[]);
    for (removed, setting) in [
        (&["TRUSTIOT_SMTP_FROM"][..], "platform.smtp_from"),
        (&["TRUSTIOT_SMTP_SECURITY"][..], "platform.smtp_security"),
        (&["TRUSTIOT_SMTP_PASSWORD"][..], "platform.smtp_password"),
        (
            &["TRUSTIOT_SMTP_USERNAME", "TRUSTIOT_SMTP_PASSWORD"][..],
            "platform.smtp_auth_required",
        ),
    ] {
        let change = removed.join(" and ");
        let env: Vec<(&str, &str)> = base
            .iter()
            .filter(|(name, _)| !removed.contains(&name.as_str()))
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .collect();
        let service = Service::start(&env);
        service.wait_ready(Duration::from_secs(60));
        let logged = service
            .stdout
            .wait_for_message(UNUSABLE_RELAY, LOG_WITHIN)
            .unwrap_or_else(|| panic!("{change}: no error logged\n{}", service.stdout.text()));
        assert_eq!(logged["level"], "ERROR", "{change}: {logged}");
        let victim = user(&mut admin, "badrelay").await;
        let response = send_code(&service, &victim.email);
        assert_eq!(response.status, 503, "{change}: {}", response.raw());
        assert_eq!(count(&mut admin, CODES_OF, &victim.id).await, 0, "{change}");
        let output = service.stop();
        assert!(output.contains(setting), "{change}: {output}");
        assert!(relay.deliveries().is_empty(), "{change}");
        for value in [
            relay.account.username.as_str(),
            relay.account.password.as_str(),
            "noreply@example.com",
        ] {
            assert!(!output.contains(value), "{change}: a value printed");
        }
    }
}

/// A session is extended on use, 12 hours from the request, but never beyond 7 days from its
/// start.
#[tokio::test(flavor = "multi_thread")]
async fn sessions_are_extended_up_to_seven_days() {
    let Some(product) = product_postgres(&["identity_app"]).await else {
        return;
    };
    let mut admin = product.superuser().await;
    let service = start(&product, None, &[]);
    let henry = user(&mut admin, "extend").await;
    // An hour old with 30 minutes left; 7 days less 1 hour old with that hour left.
    let mut sessions = Vec::new();
    for (age, left) in [(3600.0, 1800.0), (7.0 * 86400.0 - 3600.0, 3600.0)] {
        let secret = new_secret();
        sqlx::query(INSERT_SESSION)
            .bind(secret.hash().as_bytes().as_slice())
            .bind(&henry.id)
            .bind(None::<String>)
            .bind(age)
            .bind(left)
            .execute(&mut admin)
            .await
            .expect("insert a session");
        sessions.push(secret);
    }
    let [young, old] = sessions.as_slice() else {
        unreachable!("two sessions");
    };
    assert_eq!(check(&service, &cookie(old), LOGOUT).status, 200);
    // A check skips the extension when no connection is idle (src/session.rs, `extend`), and
    // the session's next request tries again: so does this loop.
    let deadline = Instant::now() + WRITE_WITHIN;
    loop {
        assert_eq!(check(&service, &cookie(young), LOGOUT).status, 200);
        let extended: bool = sqlx::query_scalar(
            "SELECT expires_at > now() + interval '11 hours 59 minutes' FROM identity.sessions
             WHERE token_hash = $1",
        )
        .bind(young.hash().as_bytes().as_slice())
        .fetch_one(&mut admin)
        .await
        .expect("the young session");
        if extended {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the young session extended to 12 hours from now within {WRITE_WITHIN:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let (left, within_cap): (f64, bool) = sqlx::query_as(SESSION_EXPIRY)
        .bind(old.hash().as_bytes().as_slice())
        .fetch_one(&mut admin)
        .await
        .expect("the old session");
    assert!(within_cap, "an extension passed 7 days");
    assert!(
        left < 3700.0,
        "the old session gained more than its cap: {left}"
    );
}

/// A login request another site made the browser send (`Sec-Fetch-Site` other than
/// `same-origin`, or without it an `Origin` that is not this site's) gets the ingress gateway's
/// cross-site 403 for both steps, and changes nothing: no code, no email, no attempt, no session,
/// and the browser's previous session stays. The SPA's own requests, same-origin by
/// `Sec-Fetch-Site` or by `Origin`, log in as before. Logging out goes through the cookie check,
/// which refuses a cross-site logout the same way.
#[tokio::test(flavor = "multi_thread")]
async fn cross_site_login_requests_change_nothing() {
    let Some(product) = product_postgres(&["identity_app"]).await else {
        return;
    };
    let mut admin = product.superuser().await;
    let relay = FakeSmtp::start(Mode::StartTls);
    let service = start(&product, Some(&relay), &[]);
    let victim = user(&mut admin, "csrf").await;
    let same_origin = format!("https://{APP}");
    let attacker = "https://attacker.example.test";
    let cross_site: [(&[(&str, &str)], &str); 3] = [
        (&[("Sec-Fetch-Site", "cross-site")], "sec-fetch-site"),
        (
            &[
                ("Sec-Fetch-Site", "same-site"),
                ("Origin", same_origin.as_str()),
            ],
            "sec-fetch-site",
        ),
        (&[("Origin", attacker)], "origin"),
    ];
    let post = |body: &Value, extra: &[(&str, &str)], cookie: Option<&str>| {
        let mut headers = vec![("Host", APP), ("Content-Type", "application/json")];
        headers.extend_from_slice(extra);
        if let Some(cookie) = cookie {
            headers.push(("Cookie", cookie));
        }
        request_with_body(
            service.address,
            "POST",
            LOGIN,
            &headers,
            body.to_string().as_bytes(),
        )
        .expect("call the login endpoint")
    };
    let refused = |response: &support::Response, what: &str| {
        assert_eq!(response.status, 403, "{what}: {}", response.raw());
        assert_eq!(
            response.json(),
            json!({"code": "permission_denied", "message_key": "security.cross_site_request"}),
            "{what}"
        );
        assert_eq!(
            response.header_values("cache-control"),
            ["no-store"],
            "{what}"
        );
        assert!(response.header_values("set-cookie").is_empty(), "{what}");
    };
    let handled = |stdout: &support::Output| {
        [SENT, NO_USER, THROTTLED, SAME_MOMENT]
            .iter()
            .map(|message| stdout.count_messages(message))
            .sum::<usize>()
    };

    // 1. send_code from another site: 403, nothing queued, no code row.
    let send = json!({"step": "send_code", "email": victim.email});
    for (extra, header) in cross_site {
        refused(&post(&send, extra, None), &format!("send_code {extra:?}"));
        assert!(
            service
                .stdout
                .wait_for_json(LOG_WITHIN, |line| {
                    line["message"] == REFUSED_CROSS_SITE
                        && line["fields"]["reason"] == "cross_site_request"
                        && line["fields"]["header"] == header
                })
                .is_some(),
            "the refusal of {extra:?} is logged with header {header}"
        );
    }
    assert_eq!(count(&mut admin, CODES_OF, &victim.id).await, 0);

    // 2. The SPA's own send_code (same-origin by Sec-Fetch-Site): 202 and one email, the first
    //    and only request the worker handles.
    let sent = post(
        &send,
        &[("Sec-Fetch-Site", "same-origin"), ("Origin", &same_origin)],
        None,
    );
    assert_eq!(sent.status, 202, "{}", sent.raw());
    let deliveries = relay.wait_for_deliveries(1, MAIL_WITHIN);
    let code = code_in(&deliveries[0]);
    assert!(service.stdout.wait_for_messages(SENT, 1, LOG_WITHIN));
    assert_eq!(handled(&service.stdout), 1);
    assert_eq!(count(&mut admin, CODES_OF, &victim.id).await, 1);

    // 3. verify_code from another site with the right code, while the browser holds a session:
    //    403, no attempt counted, no session created, the old session not revoked.
    let previous = new_secret();
    sqlx::query(INSERT_SESSION)
        .bind(previous.hash().as_bytes().as_slice())
        .bind(&victim.id)
        .bind(None::<String>)
        .bind(60.0)
        .bind(12.0 * 3600.0)
        .execute(&mut admin)
        .await
        .expect("insert the browser's session");
    let previous = cookie(&previous);
    let verify = json!({"step": "verify_code", "email": victim.email, "code": code});
    for (extra, _) in cross_site {
        refused(
            &post(&verify, extra, Some(&previous)),
            &format!("verify_code {extra:?}"),
        );
    }
    assert_eq!(newest_code(&mut admin, &victim.id).await, (0, false, false));
    assert_eq!(count(&mut admin, SESSIONS_OF, &victim.id).await, 1);
    assert_eq!(count(&mut admin, LIVE_SESSIONS_OF, &victim.id).await, 1);
    assert_eq!(check(&service, &previous, LOGOUT).status, 200);

    // 4. A cross-site logout is refused by the cookie check before it reaches the service, and
    //    the session stays.
    for path in [LOGOUT, LOGOUT_EVERYWHERE] {
        for (extra, _) in cross_site {
            let mut headers = vec![("Host", APP), ("Cookie", previous.as_str())];
            headers.extend_from_slice(extra);
            let response = request(
                service.address,
                "DELETE",
                &format!("/internal/ext-authz{path}"),
                &headers,
            )
            .expect("call the ext_authz check");
            refused(&response, &format!("logout {path} {extra:?}"));
            assert!(response.header_values("x-internal-identity").is_empty());
        }
    }
    assert_eq!(check(&service, &previous, LOGOUT).status, 200);

    // 5. The SPA's own verify_code (same-origin by Origin, no Sec-Fetch-Site) logs in with the
    //    code the cross-site requests could not touch, and ends the previous session.
    let logged_in = post(&verify, &[("Origin", &same_origin)], Some(&previous));
    assert_eq!(logged_in.status, 200, "{}", logged_in.raw());
    let session = session_cookie(&logged_in);
    assert_eq!(newest_code(&mut admin, &victim.id).await, (1, true, false));
    assert_eq!(check(&service, &session, LOGOUT).status, 200);
    assert_eq!(check(&service, &previous, LOGOUT).status, 401);

    // 6. Six refusals, each logged once by header name: no address, code, cookie or Origin value
    //    in the logs, and the code check never ran for them.
    assert_eq!(service.stdout.count_messages(REFUSED_CROSS_SITE), 6);
    assert_eq!(service.stdout.count_messages(REFUSED_CODE), 0);
    assert_eq!(handled(&service.stdout), 1);
    assert_eq!(relay.deliveries().len(), 1);
    let logs = service.stop();
    let previous_secret = previous
        .strip_prefix(&format!("{SESSION_COOKIE}="))
        .expect("cookie value");
    for (what, value) in [
        ("the address", victim.email.as_str()),
        ("the session cookie", previous_secret),
        ("the Origin", attacker),
    ] {
        assert!(!logs.contains(value), "{what} is in the logs");
    }
    assert!(!contains_number(&logs, &code), "the code is in the logs");
}

/// The routes that could log someone in without a code do not exist, in this local build, and
/// no cookie or field turns one on: no password login, no sign-up, no fake or dev login.
#[tokio::test(flavor = "multi_thread")]
async fn there_is_no_password_login_sign_up_or_fake_login() {
    let Some(product) = product_postgres(&["identity_app"]).await else {
        return;
    };
    let mut admin = product.superuser().await;
    let relay = FakeSmtp::start(Mode::StartTls);
    let service = start(&product, Some(&relay), &[]);
    let ivan = user(&mut admin, "routes").await;

    // 1. The documented API is the whole public surface.
    let document = serde_json::to_value(identity::openapi()).expect("the OpenAPI document");
    let mut paths: Vec<(String, Vec<String>)> = document["paths"]
        .as_object()
        .expect("paths")
        .iter()
        .map(|(path, item)| {
            let mut methods: Vec<String> =
                item.as_object().expect("methods").keys().cloned().collect();
            methods.sort();
            (path.clone(), methods)
        })
        .collect();
    paths.sort();
    assert_eq!(
        paths,
        [
            (
                "/api/identity/v1/sessions".to_owned(),
                vec!["delete".to_owned()]
            ),
            (
                "/api/identity/v1/sessions/current".to_owned(),
                vec!["delete".to_owned()]
            ),
            (
                "/api/identity/v1/tenants".to_owned(),
                vec!["post".to_owned()]
            ),
            ("/auth/login".to_owned(), vec!["post".to_owned()]),
        ]
    );

    // 2. Paths and methods a password login, a sign-up or a fake login would use: none exists.
    let fake_cookies = "trustiot-dev-login=1; dev_login=true; fake_login=1; trustiot_dev=1";
    for path in [
        "/auth/login/password",
        "/auth/password",
        "/auth/signup",
        "/auth/sign-up",
        "/auth/sign_up",
        "/auth/register",
        "/auth/dev-login",
        "/auth/dev_login",
        "/auth/fake-login",
        "/auth/login/dev",
        "/auth/session",
        "/dev/login",
        "/login",
        "/signup",
        "/api/identity/v1/login",
        "/api/identity/v1/signup",
        "/api/identity/v1/users",
        "/api/identity/v1/password",
        "/api/identity/v1/sessions/new",
    ] {
        for method in ["GET", "POST", "PUT"] {
            let body = json!({"email": ivan.email, "password": "x"}).to_string();
            let response = request_with_body(
                service.address,
                method,
                path,
                &[
                    ("Host", APP),
                    ("Content-Type", "application/json"),
                    ("Cookie", fake_cookies),
                ],
                body.as_bytes(),
            )
            .expect("probe");
            assert!(
                matches!(response.status, 404 | 405),
                "{method} {path}: {}",
                response.raw()
            );
            assert!(response.header_values("set-cookie").is_empty());
        }
    }
    for method in ["GET", "PUT", "PATCH", "DELETE"] {
        let response = request(service.address, method, LOGIN, &[("Host", APP)]).expect("probe");
        assert_eq!(response.status, 405, "{method} {LOGIN}");
    }
    let response = request_with_body(
        service.address,
        "POST",
        LOGOUT_EVERYWHERE,
        &[("Host", APP), ("Content-Type", "application/json")],
        b"{}",
    )
    .expect("probe");
    assert_eq!(response.status, 405, "{}", response.raw());

    // 3. Bodies that ask the login endpoint for another way in: refused as malformed.
    for body in [
        json!({"step": "password", "email": ivan.email, "password": "x"}),
        json!({"step": "sign_up", "email": ivan.email}),
        json!({"step": "dev_login", "email": ivan.email}),
        json!({"step": "fake_login", "user_id": ivan.id}),
        json!({"email": ivan.email, "password": "x"}),
        json!({"step": "verify_code", "email": ivan.email, "code": "12345678", "password": "x"}),
        json!({"step": "verify_code", "email": ivan.email, "code": "12345678", "dev": true}),
        json!({"step": "send_code", "email": ivan.email, "user_id": ivan.id}),
    ] {
        let response = post_login(&service, &body, Some(fake_cookies));
        assert_eq!(response.status, 400, "{body}: {}", response.raw());
        assert_eq!(error_code(&response), "invalid_request");
        assert!(response.header_values("set-cookie").is_empty());
    }

    // 4. Cookies that would switch on a fake login switch on nothing: a made-up code for a real
    //    user is refused, and the cookie check knows no such cookie.
    let response = verify_code(&service, &ivan.email, "00000000", None, Some(fake_cookies));
    assert_eq!(response.status, 401, "{}", response.raw());
    assert!(response.header_values("set-cookie").is_empty());
    assert_eq!(check(&service, fake_cookies, LOGOUT).status, 401);
    assert_eq!(count(&mut admin, SESSIONS_OF, &ivan.id).await, 0);
    assert!(relay.deliveries().is_empty());
}

/// The release build cannot differ from this one in how users log in: the crate has no Cargo
/// features (an optional dependency would be one), no build script, and no code that depends on
/// the build profile or the build environment, so there is no switch that could compile a fake
/// login in (services/identity/README.md, "沒有假登入"). The only `cfg` is `cfg(test)`, for
/// unit tests, and the only `env!` is the package version.
#[test]
fn no_build_switch_can_add_a_fake_login() {
    let crate_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    // 1. Cargo's own view of the package: no features, implicit ones included, and no build
    //    script, declared or found by name.
    let metadata = std::process::Command::new(env!("CARGO"))
        .args([
            "metadata",
            "--format-version",
            "1",
            "--no-deps",
            "--offline",
        ])
        .arg("--manifest-path")
        .arg(crate_dir.join("Cargo.toml"))
        .output()
        .expect("run cargo metadata");
    assert!(
        metadata.status.success(),
        "cargo metadata: {}",
        String::from_utf8_lossy(&metadata.stderr)
    );
    let metadata: Value = serde_json::from_slice(&metadata.stdout).expect("cargo metadata JSON");
    let identity = metadata["packages"]
        .as_array()
        .expect("packages")
        .iter()
        .find(|package| package["name"] == "identity")
        .expect("the identity package");
    assert_eq!(
        identity["features"],
        json!({}),
        "services/identity has features"
    );
    let kinds: Vec<&Value> = identity["targets"]
        .as_array()
        .expect("targets")
        .iter()
        .flat_map(|target| target["kind"].as_array().expect("target kinds"))
        .collect();
    assert!(
        !kinds.iter().any(|kind| *kind == "custom-build"),
        "services/identity has a build script"
    );
    assert!(
        !crate_dir.join("build.rs").exists(),
        "services/identity/build.rs"
    );
    // 2. The manifest text, which says the same without asking Cargo.
    let manifest = std::fs::read_to_string(crate_dir.join("Cargo.toml")).expect("Cargo.toml");
    for line in manifest.lines() {
        let line: String = line.split_whitespace().collect();
        if line.starts_with('#') {
            continue;
        }
        assert!(!line.starts_with("[features"), "Cargo.toml: {line}");
        assert!(!line.starts_with("build="), "Cargo.toml: {line}");
        assert!(!line.contains("optional=true"), "Cargo.toml: {line}");
    }
    // 3. The sources, with whitespace removed so `cfg (` counts as `cfg(`.
    let mut pending = vec![crate_dir.join("src")];
    let mut files = 0;
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).expect("read src") {
            let path = entry.expect("entry").path();
            if path.is_dir() {
                pending.push(path);
                continue;
            }
            if path.extension().is_none_or(|extension| extension != "rs") {
                continue;
            }
            files += 1;
            let text: String = std::fs::read_to_string(&path)
                .expect("read a source file")
                .split_whitespace()
                .collect();
            for needle in [
                "debug_assertions",
                "cfg!(",
                "cfg_attr",
                "option_env!(",
                "include!(",
            ] {
                assert!(!text.contains(needle), "{}: {needle}", path.display());
            }
            let near = |index: usize| text.get(index..).unwrap_or_default().chars().take(60);
            for (index, _) in text.match_indices("cfg(") {
                assert!(
                    text.get(index..)
                        .is_some_and(|rest| rest.starts_with("cfg(test)")),
                    "{}: a cfg other than cfg(test): {}",
                    path.display(),
                    near(index).collect::<String>()
                );
            }
            for (index, _) in text.match_indices("env!(") {
                assert!(
                    text.get(index..)
                        .is_some_and(|rest| rest.starts_with(r#"env!("CARGO_PKG_VERSION")"#)),
                    "{}: an env! other than the package version: {}",
                    path.display(),
                    near(index).collect::<String>()
                );
            }
        }
    }
    assert!(files > 5, "read the sources ({files} files)");
}
