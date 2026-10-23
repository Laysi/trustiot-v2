//! Logging in with an email verification code (Laysi/trustiot-v2#103; decision 13; checklist
//! A1; services/identity/README.md, "登入"). There is no other way in: no password, no public
//! sign-up, and no fake login in any build. Users are created by platform administrators, and
//! only a user with an `email` can log in.
//!
//! Both steps are `POST /auth/login` ([`LOGIN_PATH`]), the one cookie-free path the ingress
//! routes to this service (decision 27), with a JSON body that names the step:
//!
//! 1. `{"step": "send_code", "email": ...}` answers 202 at once, whether or not a user has the
//!    address, and a background task ([`CodeWorker`]) issues a code and mails it. So neither
//!    the answer nor its timing tells who has an account.
//! 2. `{"step": "verify_code", "email": ..., "code": ..., "redirect_to": ...}` checks the code
//!    and, in the same transaction, creates the session; the answer sets the session cookie and
//!    returns the validated `redirect_to`.
//!
//! | Request | Response |
//! | --- | --- |
//! | A cross-site request ([`crate::csrf::cross_site`]), decided before anything else: before the body is read, the database is asked, an attempt is counted or a code is queued | 403 `permission_denied`, message key `security.cross_site_request` |
//! | A query string, a `Content-Type` other than `application/json`, a body over [`MAX_BODY_BYTES`], or a body that is not a JSON object naming one of the two steps (unknown fields such as `password` and JSON arrays included) | 400 `invalid_request` |
//! | An `email` that is not an address, a `code` that is not [`CODE_DIGITS`] digits, a `redirect_to` that is not a path on this site | 422 `validation_failed`, fields `/email` (`login.email_invalid`), `/code` (`login.code_invalid`), `/redirect_to` (`login.redirect_invalid`) |
//! | `send_code` | 202 [`CodeSent`] |
//! | `send_code` without a usable SMTP relay, or with the send queue full | 503 `unavailable` |
//! | `verify_code` with no usable code for the address, or a wrong code | 401 `unauthenticated` |
//! | `verify_code` with the right code | 200 [`LoggedIn`] and `Set-Cookie` |
//! | `verify_code` before the signing keys have been read, or the database failed | 503 `unavailable`; nothing changed |
//!
//! The rules of a code (docs/architecture/secrets.md, "一次性 token 與驗證碼"):
//!
//! - [`CODE_DIGITS`] digits from the operating system's CSPRNG, valid for [`CODE_LIFETIME`],
//!   at most one usable code per user (a new one invalidates the old), and dead after
//!   [`MAX_ATTEMPTS`] attempts. Per address at most one code per [`RESEND_INTERVAL`],
//!   [`MAX_CODES_PER_HOUR`] per hour and [`MAX_CODES_PER_DAY`] per day; a request beyond that
//!   is answered the same but sends nothing.
//! - The database keeps only an HMAC-SHA-256 of the purpose, the address and the code
//!   ([`code_hash`]) under a key derived from the identity signing key
//!   ([`Issuer::code_key`]), which is not in the database: a code is bound to its use and to
//!   the address it was sent to, and a copy of the database cannot try the 10⁸ codes offline.
//! - One `UPDATE ... RETURNING` both counts the attempt and consumes the code when it matches;
//!   the session is created in the same transaction. Two requests with the same code: one wins.
//! - The code travels only in the POST body and in the email. It is never in a URL, a log
//!   line, or the email's subject.
//! - The path is public, so the ingress gateway's cookie check, which refuses cross-site
//!   requests for every other path (services/identity/README.md, "跨站請求（CSRF）"), does not
//!   see it: the login handler runs the same check first, for both steps. Otherwise another
//!   site could make a browser spend an address's codes and attempts, or log the browser in to
//!   an account the attacker controls (login CSRF).
//! - Neither step tells who has an account: `send_code` answers before anything is looked up,
//!   and both refusals of `verify_code` take at least [`REFUSAL_FLOOR`].
//!
//! A known limit, accepted for now and tracked (services/identity/README.md, "登入"): the send
//! limits and the attempts are counted per address, not per caller, so anyone who knows an
//! address can use up its codes and attempts and keep that user from logging in for up to a
//! day. They cannot log in as the user: guessing stays bounded by the limits above. Protecting
//! users from that (separate send and guess allowances, limits per source, an alert) is
//! Laysi/trustiot-v2#508, a precondition of opening the SaaS to the public
//! (Laysi/trustiot-v2#434); until then the only deployment is the demo, which Cloudflare Access
//! limits to invited people (docs/roadmap/milestones.md, "延後的項目與展示環境的補償").

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::http::header::{CACHE_CONTROL, SET_COOKIE};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use contract::{ErrorBody, ErrorCode, FieldError, MessageKey, ServiceName};
use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use service_kit::identity::ServiceIdentity;
use service_kit::postgres::InsecureConnection;
use service_kit::shutdown::Shutdown;
use service_kit::tenant::TenantDb;
use sha2::Sha256;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use tokio::sync::{Semaphore, mpsc};
use tokio::task::JoinSet;
use utoipa::ToSchema;
use zeroize::Zeroizing;

use crate::csrf::{CROSS_SITE_REQUEST, cross_site};
use crate::issuer::{CodeKey, Issuer, unix_now};
use crate::mail::{Mailer, Message};
use crate::session::{
    SESSION_IDLE_LIFETIME, SessionHash, SessionSecret, error_kind, session_secret, set_cookie,
};
use crate::tenants::{error, is_json};

/// The login path, as the ingress forwards it (`ingress.publicPaths.login` of the chart, which
/// must stay `/auth/login`).
pub const LOGIN_PATH: &str = "/auth/login";

/// Digits in a login code: 10⁸ possible codes.
pub const CODE_DIGITS: usize = 8;

/// How long a code is valid.
pub const CODE_LIFETIME: Duration = Duration::from_secs(10 * 60);

/// Attempts per code, the right one included; then the code is dead. With the send limits
/// below, a guesser gets at most [`MAX_CODES_PER_DAY`] × [`MAX_ATTEMPTS`] = 100 guesses a day
/// per address: a chance of 10⁻⁶ a day.
pub const MAX_ATTEMPTS: i32 = 5;

/// The shortest time between two codes for one address.
pub const RESEND_INTERVAL: Duration = Duration::from_secs(60);

/// Codes per address per hour.
pub const MAX_CODES_PER_HOUR: i64 = 5;

/// Codes per address per day.
pub const MAX_CODES_PER_DAY: i64 = 20;

/// How long code rows are kept: long enough for the daily limit.
pub const CODE_RETENTION: Duration = Duration::from_secs(24 * 3600);

/// How often each replica deletes code rows older than [`CODE_RETENTION`].
pub const CLEANUP_INTERVAL: Duration = Duration::from_secs(3600);

/// The largest request body read.
pub const MAX_BODY_BYTES: usize = 4096;

/// The longest `redirect_to`, in bytes.
pub const MAX_REDIRECT_BYTES: usize = 2048;

/// `send_code` requests waiting for the worker. A full queue answers 503.
pub const SEND_QUEUE: usize = 64;

/// Codes the worker issues and sends at once.
pub const CONCURRENT_SENDS: usize = 4;

/// How long sending one email may take, connecting included.
pub const SEND_LIMIT: Duration = Duration::from_secs(60);

/// Connections of the request pool, which `verify_code` and both logouts use: separate from the
/// session store's, so a burst of logins never takes a connection the ext_authz check is
/// waiting for, and from the send pool.
pub const POOL_CONNECTIONS: u32 = 4;

/// Connections of the send pool, which only the [`CodeWorker`] (issuing codes, invalidating
/// unsent ones) and the hourly clean-up use, so a flood of `send_code` never takes a connection
/// `verify_code` or a logout needs. Issuing holds a connection for one short transaction, not
/// while the email goes out, so two serve [`CONCURRENT_SENDS`] sends; a send that waits longer
/// than [`ACQUIRE_TIMEOUT`] for one is logged and dropped, and its user asks again.
pub const SEND_POOL_CONNECTIONS: u32 = 2;

/// How long a login waits for a connection (opening one included).
pub const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(5);

/// The least time a refused `verify_code` (401) takes, from when the code check starts. A wrong
/// code for an address with a usable code counts the attempt and commits it; an address
/// without one, or without a user, writes nothing. Without the floor the two would differ in
/// response time, and since only an address with a user ever gets a code, asking for a code and
/// then timing a wrong one would tell who has an account. The floor is far above either path's
/// database time on a database that answers normally; it only delays an answer that is a
/// refusal anyway, after the transaction has ended, so it holds no connection.
pub const REFUSAL_FLOOR: Duration = Duration::from_millis(500);

/// `/email` is not an address.
pub const EMAIL_INVALID: MessageKey = MessageKey::new("login.email_invalid");

/// `/code` is not [`CODE_DIGITS`] digits.
pub const CODE_INVALID: MessageKey = MessageKey::new("login.code_invalid");

/// `/redirect_to` is not a path on this site.
pub const REDIRECT_INVALID: MessageKey = MessageKey::new("login.redirect_invalid");

/// Where the browser goes after logging in when the request names nothing.
pub const DEFAULT_REDIRECT: &str = "/";

/// Separates the code hash from every other use of the key.
const CODE_HASH_PURPOSE: &[u8] = b"trustiot-login-code:v1\n";

const FIND_USER: &str = "SELECT id::text FROM identity.users WHERE email = $1";

/// Serializes issuing codes for one user (`$1`) until the transaction ends, so the send limits
/// are counted after the previous issue committed: without it, concurrent requests could all
/// pass the limits, and each would invalidate the code before it and send one more. An advisory
/// lock, because the service role may not lock rows of `identity.users` (that needs `UPDATE`).
const LOCK_USER_CODES: &str =
    "SELECT pg_advisory_xact_lock(hashtextextended('trustiot-login-code:' || $1, 0))";

/// The user's codes created within the resend interval (`$2` seconds), the last hour and the
/// last day.
const RECENT_CODES: &str = "SELECT
    count(*) FILTER (WHERE created_at > now() - make_interval(secs => $2)),
    count(*) FILTER (WHERE created_at > now() - interval '1 hour'),
    count(*) FILTER (WHERE created_at > now() - interval '1 day')
FROM identity.login_codes WHERE user_id = $1::uuid";

const INVALIDATE_OPEN_CODES: &str = "UPDATE identity.login_codes SET invalidated_at = now()
WHERE user_id = $1::uuid AND consumed_at IS NULL AND invalidated_at IS NULL";

const INSERT_CODE: &str = "INSERT INTO identity.login_codes (user_id, code_hash, expires_at)
VALUES ($1::uuid, $2, now() + make_interval(secs => $3))
RETURNING id::text";

const INVALIDATE_CODE: &str = "UPDATE identity.login_codes SET invalidated_at = now()
WHERE id = $1::uuid AND consumed_at IS NULL AND invalidated_at IS NULL";

const DELETE_OLD_CODES: &str =
    "DELETE FROM identity.login_codes WHERE created_at < now() - make_interval(secs => $1)";

/// Counts one attempt on the address's usable code and consumes it when the hash matches, in
/// one statement: `$1` the address, `$2` the hashes of the code given under each key a code in
/// use may have been hashed with ([`Issuer::code_keys`]), `$3` [`MAX_ATTEMPTS`]. Returns the
/// user and whether this attempt consumed the code; no row when the address has no usable code
/// (none, expired, used, invalidated or out of attempts) or no user has it.
const CONSUME_CODE: &str = "UPDATE identity.login_codes AS c
SET attempts = c.attempts + 1,
    consumed_at = CASE WHEN c.code_hash = ANY($2) THEN now() END
FROM identity.users AS u
WHERE u.email = $1 AND c.user_id = u.id
    AND c.consumed_at IS NULL AND c.invalidated_at IS NULL
    AND c.expires_at > now() AND c.attempts < $3
RETURNING c.user_id::text, c.consumed_at IS NOT NULL";

/// Ends the session the browser had before logging in again, whoever it belonged to: its
/// cookie is replaced anyway.
const REVOKE_PREVIOUS: &str = "UPDATE identity.sessions SET revoked_at = now()
WHERE token_hash = $1 AND revoked_at IS NULL";

/// A new session for `$2`, expiring [`SESSION_IDLE_LIFETIME`] (`$3` seconds) from now, in the
/// tenant of the user's latest session (switching tenants is Laysi/trustiot-v2#497).
const INSERT_SESSION: &str = "INSERT INTO identity.sessions
    (id, token_hash, user_id, active_tenant_id, expires_at)
VALUES (gen_random_uuid(), $1, $2::uuid,
    (SELECT s.active_tenant_id FROM identity.sessions AS s
     WHERE s.user_id = $2::uuid AND s.active_tenant_id IS NOT NULL
     ORDER BY s.created_at DESC LIMIT 1),
    now() + make_interval(secs => $3))";

/// Revokes the request's own session (logout): `$1` its hash, `$2` the user it must belong to.
const REVOKE_CURRENT: &str = "UPDATE identity.sessions SET revoked_at = now()
WHERE token_hash = $1 AND user_id = $2::uuid AND revoked_at IS NULL";

/// Revokes every session of `$1` that has not ended (logout everywhere).
const REVOKE_ALL: &str = "UPDATE identity.sessions SET revoked_at = now()
WHERE user_id = $1::uuid AND revoked_at IS NULL AND expires_at > now()";

/// SQLSTATE `unique_violation`: another code for the user was issued at the same moment.
const UNIQUE_VIOLATION: &str = "23505";

/// An email address as the service compares and stores it (src/email.rs, which the migration
/// Job's `bootstrap-platform-admin` compiles too, so the address it stores is the one a login
/// looks up).
pub use crate::email::EmailAddress;

/// A login code: [`CODE_DIGITS`] ASCII digits. `Debug` never prints it; wiped on drop.
#[derive(Clone, PartialEq, Eq)]
pub struct LoginCode(Zeroizing<String>);

impl LoginCode {
    /// A new code, uniform over all [`CODE_DIGITS`]-digit numbers.
    ///
    /// # Errors
    ///
    /// When the operating system has no randomness to give.
    pub fn generate() -> Result<Self, getrandom::Error> {
        // The largest multiple of 10⁸ that fits in a u32 is 42 × 10⁸; drawing below it and
        // taking the remainder gives every code the same chance.
        const RANGE: u32 = 100_000_000;
        const LIMIT: u32 = 42 * RANGE;
        loop {
            let mut bytes = Zeroizing::new([0_u8; 4]);
            getrandom::fill(bytes.as_mut())?;
            let draw = Zeroizing::new(u32::from_le_bytes(*bytes));
            if *draw < LIMIT {
                return Ok(Self(Zeroizing::new(format!("{:08}", *draw % RANGE))));
            }
        }
    }

    /// Checks a code as typed; `None` unless it is exactly [`CODE_DIGITS`] ASCII digits.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        (text.len() == CODE_DIGITS && text.bytes().all(|byte| byte.is_ascii_digit()))
            .then(|| Self(Zeroizing::new(text.to_owned())))
    }

    /// The digits. Never log them.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for LoginCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("LoginCode(<redacted>)")
    }
}

/// What the database stores of a code sent to `email`: HMAC-SHA-256 under `key` (a
/// [`CodeKey`] from the signing keys, never in the database) of [`CODE_HASH_PURPOSE`], the
/// address and the code. So a code only logs in the address it was sent to, a hash of this
/// table matches nothing else, and whoever reads the table without the key cannot tell which
/// of the 10⁸ codes it holds. An address never holds a line break.
#[must_use]
pub fn code_hash(key: &[u8; 32], email: &EmailAddress, code: &LoginCode) -> [u8; 32] {
    // HMAC accepts keys of any length, so this never fails.
    let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(key)
        .unwrap_or_else(|_| unreachable!("HMAC-SHA-256 takes keys of any length"));
    mac.update(CODE_HASH_PURPOSE);
    mac.update(email.as_str().as_bytes());
    mac.update(b"\n");
    mac.update(code.as_str().as_bytes());
    mac.finalize().into_bytes().into()
}

/// Checks `redirect_to`: a path on this site, returned unchanged; `None` for anything that could
/// leave it. Absent means [`DEFAULT_REDIRECT`].
///
/// Accepted is a path-absolute reference (`/` and then anything but another `/`) of at most
/// [`MAX_REDIRECT_BYTES`] visible ASCII characters without `\`. Browsers parse it against the
/// app's own origin (WHATWG URL, "relative slash state"). What is refused and why: a scheme or
/// an absolute URL (`https://evil.example`, `javascript:`), even of this host (its host is not
/// checked against anything); `//evil.example` (scheme-relative: another host); `\` anywhere,
/// which browsers read as `/` (`/\evil.example`); and spaces, tabs, line breaks and other
/// characters outside visible ASCII, which browsers strip or re-encode (`/\t/evil.example`
/// becomes `//evil.example`).
#[must_use]
pub fn redirect_target(requested: Option<&str>) -> Option<String> {
    let Some(target) = requested else {
        return Some(DEFAULT_REDIRECT.to_owned());
    };
    let bytes = target.as_bytes();
    let valid = (1..=MAX_REDIRECT_BYTES).contains(&bytes.len())
        && bytes.first() == Some(&b'/')
        && bytes.get(1) != Some(&b'/')
        && bytes
            .iter()
            .all(|byte| (0x21..=0x7e).contains(byte) && *byte != b'\\');
    valid.then(|| target.to_owned())
}

/// `send_code`: mail a login code to this address.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SendCode {
    /// The address to log in with.
    pub email: String,
}

impl fmt::Debug for SendCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SendCode(<redacted>)")
    }
}

/// `verify_code`: log in with the code from the email.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerifyCode {
    /// The address the code was sent to.
    pub email: String,
    /// The code, 8 digits.
    pub code: String,
    /// Where to go after logging in: a path on this site such as `/devices?page=2`. Default `/`.
    #[serde(default)]
    pub redirect_to: Option<String>,
}

impl fmt::Debug for VerifyCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("VerifyCode(<redacted>)")
    }
}

/// The body of `POST /auth/login`: one of the two steps, named by `step`.
#[derive(Debug, Deserialize)]
#[serde(tag = "step", rename_all = "snake_case")]
pub enum LoginRequest {
    /// Step 1.
    SendCode(SendCode),
    /// Step 2.
    VerifyCode(VerifyCode),
}

/// The OpenAPI schema of [`LoginRequest`], written out. The derived one composes each step with
/// `allOf` from the step's struct, whose `deny_unknown_fields` makes it
/// `additionalProperties: false`; by JSON Schema that refuses the `step` field itself, so no
/// valid body would validate. Here each step is one closed object with `step` among its
/// properties, which is what [`parse_request`] reads. (The builders are imported here only:
/// in the module's scope they would make the `utoipa::path` expansion's own paths "unnecessary
/// qualifications".)
impl utoipa::PartialSchema for LoginRequest {
    fn schema() -> utoipa::openapi::RefOr<utoipa::openapi::schema::Schema> {
        use utoipa::openapi::schema::{
            AdditionalProperties, ObjectBuilder, OneOfBuilder, Schema, SchemaType, Type,
        };

        let step = |name: &str| {
            ObjectBuilder::new()
                .schema_type(Type::String)
                .enum_values(Some([name]))
        };
        let string = |description: &str| {
            ObjectBuilder::new()
                .schema_type(Type::String)
                .description(Some(description))
        };
        let send_code = ObjectBuilder::new()
            .schema_type(Type::Object)
            .title(Some("send_code"))
            .description(Some("Step 1: mail a login code to this address."))
            .property("step", step("send_code"))
            .property("email", string("The address to log in with."))
            .required("step")
            .required("email")
            .additional_properties(Some(AdditionalProperties::<Schema>::FreeForm(false)));
        let verify_code = ObjectBuilder::new()
            .schema_type(Type::Object)
            .title(Some("verify_code"))
            .description(Some("Step 2: log in with the code from the email."))
            .property("step", step("verify_code"))
            .property("email", string("The address the code was sent to."))
            .property("code", string("The code, 8 digits."))
            .property(
                "redirect_to",
                ObjectBuilder::new()
                    .schema_type(
                        [Type::String, Type::Null]
                            .into_iter()
                            .collect::<SchemaType>(),
                    )
                    .description(Some(
                        "Where to go after logging in: a path on this site such as \
                         `/devices?page=2`. Default `/`.",
                    )),
            )
            .required("step")
            .required("email")
            .required("code")
            .additional_properties(Some(AdditionalProperties::<Schema>::FreeForm(false)));
        OneOfBuilder::new()
            .description(Some(
                "The body of `POST /auth/login`: a JSON object that names one of the two steps \
                 in `step`. Any other field is refused.",
            ))
            .item(send_code)
            .item(verify_code)
            .into()
    }
}

impl ToSchema for LoginRequest {}

/// The answer to `send_code`, the same whether or not a user has the address.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct CodeSent {
    /// Digits in the code.
    pub code_length: u32,
    /// How long the code is valid, in seconds.
    pub expires_in_secs: u64,
    /// How long to wait before asking for another code, in seconds.
    pub resend_after_secs: u64,
}

impl CodeSent {
    fn now() -> Self {
        Self {
            code_length: u32::try_from(CODE_DIGITS).unwrap_or(u32::MAX),
            expires_in_secs: CODE_LIFETIME.as_secs(),
            resend_after_secs: RESEND_INTERVAL.as_secs(),
        }
    }
}

/// The answer to a valid code; the session cookie is in `Set-Cookie`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct LoggedIn {
    /// The validated `redirect_to`, or `/`.
    pub redirect_to: String,
}

/// Why the login store could not answer.
#[derive(Debug, thiserror::Error)]
pub enum LoginError {
    /// The database failed; the transaction rolled back.
    #[error("{}", error_kind(.0))]
    Database(#[from] sqlx::Error),
    /// No randomness for a code or a session secret.
    #[error("the operating system gave no randomness")]
    Random,
}

/// The result of issuing a code.
#[derive(Debug)]
pub enum Issued {
    /// No user has the address; nothing was stored.
    NoUser,
    /// The address is at a send limit; nothing was stored.
    Throttled {
        /// The user.
        user: String,
    },
    /// A code for the user was issued at the same moment; this one was not stored.
    Concurrent {
        /// The user.
        user: String,
    },
    /// A new code, stored and to be sent.
    Code {
        /// The row, to invalidate it when sending fails.
        id: String,
        /// The user.
        user: String,
        /// The code.
        code: LoginCode,
    },
}

/// The result of a code check.
#[derive(Debug)]
pub enum Verified {
    /// The code matched; the session exists.
    LoggedIn {
        /// The user.
        user: String,
        /// The new session's secret, for the cookie.
        secret: SessionSecret,
    },
    /// No usable code for the address, or no user has it.
    NoUsableCode,
    /// The code did not match; the attempt was counted.
    WrongCode {
        /// The user.
        user: String,
    },
}

/// The login tables, written with the service role on a pool of its own. No tenant data: users,
/// codes and sessions span tenants.
#[derive(Clone, Debug)]
pub struct LoginStore {
    db: TenantDb,
}

impl LoginStore {
    /// A lazily connecting store with its own pool of `connections` connections:
    /// [`POOL_CONNECTIONS`] for requests, [`SEND_POOL_CONNECTIONS`] for the worker.
    ///
    /// # Errors
    ///
    /// When `options` would not verify the server.
    pub fn connect_lazy(
        options: PgConnectOptions,
        connections: u32,
    ) -> Result<Self, InsecureConnection> {
        let pool = PgPoolOptions::new()
            .max_connections(connections)
            .min_connections(0)
            .acquire_timeout(ACQUIRE_TIMEOUT);
        let actor = ServiceIdentity::new(ServiceName::Identity);
        let (db, _background) = TenantDb::connect_lazy_with_background(options, pool, actor)?;
        Ok(Self { db })
    }

    /// Issues a code for `email` when a user has it and it is within the send limits: in one
    /// transaction that holds the user's lock ([`LOCK_USER_CODES`]) from the limit check on,
    /// invalidates the user's usable codes and stores the new one, hashed with `key`.
    ///
    /// # Errors
    ///
    /// When the database fails or there is no randomness.
    pub async fn issue(&self, email: &EmailAddress, key: &CodeKey) -> Result<Issued, LoginError> {
        let mut tx = self.db.pool().begin().await?;
        let user: Option<String> = sqlx::query_scalar(FIND_USER)
            .bind(email.as_str())
            .fetch_optional(&mut *tx)
            .await?;
        let Some(user) = user else {
            return Ok(Issued::NoUser);
        };
        sqlx::query(LOCK_USER_CODES)
            .bind(&user)
            .execute(&mut *tx)
            .await?;
        // Read committed: this statement sees every code committed before the lock was granted.
        let (last_interval, last_hour, last_day): (i64, i64, i64) = sqlx::query_as(RECENT_CODES)
            .bind(&user)
            .bind(RESEND_INTERVAL.as_secs_f64())
            .fetch_one(&mut *tx)
            .await?;
        if last_interval > 0 || last_hour >= MAX_CODES_PER_HOUR || last_day >= MAX_CODES_PER_DAY {
            return Ok(Issued::Throttled { user });
        }
        sqlx::query(INVALIDATE_OPEN_CODES)
            .bind(&user)
            .execute(&mut *tx)
            .await?;
        let code = LoginCode::generate().map_err(|_| LoginError::Random)?;
        let inserted = sqlx::query_scalar::<_, String>(INSERT_CODE)
            .bind(&user)
            .bind(code_hash(key, email, &code).as_slice())
            .bind(CODE_LIFETIME.as_secs_f64())
            .fetch_one(&mut *tx)
            .await;
        let id = match inserted {
            Ok(id) => id,
            Err(sqlx::Error::Database(database))
                if database.code().as_deref() == Some(UNIQUE_VIOLATION) =>
            {
                return Ok(Issued::Concurrent { user });
            }
            Err(failure) => return Err(failure.into()),
        };
        tx.commit().await?;
        Ok(Issued::Code { id, user, code })
    }

    /// Invalidates a code that could not be sent.
    ///
    /// # Errors
    ///
    /// When the database fails.
    pub async fn invalidate(&self, id: &str) -> Result<(), LoginError> {
        sqlx::query(INVALIDATE_CODE)
            .bind(id)
            .execute(self.db.pool())
            .await?;
        Ok(())
    }

    /// Checks `code` for `email` against its hash under each of `keys` and, when it matches,
    /// creates the session in the same transaction, revoking the browser's previous session
    /// (`previous`) if it had one.
    ///
    /// # Errors
    ///
    /// When the database fails or there is no randomness; nothing changed then.
    pub async fn verify(
        &self,
        email: &EmailAddress,
        code: &LoginCode,
        keys: &[CodeKey],
        previous: Option<SessionHash>,
    ) -> Result<Verified, LoginError> {
        let secret = SessionSecret::generate().map_err(|_| LoginError::Random)?;
        let hashes: Vec<Vec<u8>> = keys
            .iter()
            .map(|key| code_hash(key, email, code).to_vec())
            .collect();
        let mut tx = self.db.pool().begin().await?;
        let consumed: Option<(String, bool)> = sqlx::query_as(CONSUME_CODE)
            .bind(email.as_str())
            .bind(&hashes)
            .bind(MAX_ATTEMPTS)
            .fetch_optional(&mut *tx)
            .await?;
        let user = match consumed {
            None => return Ok(Verified::NoUsableCode),
            Some((user, false)) => {
                tx.commit().await?;
                return Ok(Verified::WrongCode { user });
            }
            Some((user, true)) => user,
        };
        if let Some(previous) = previous {
            sqlx::query(REVOKE_PREVIOUS)
                .bind(previous.as_bytes().as_slice())
                .execute(&mut *tx)
                .await?;
        }
        sqlx::query(INSERT_SESSION)
            .bind(secret.hash().as_bytes().as_slice())
            .bind(&user)
            .bind(SESSION_IDLE_LIFETIME.as_secs_f64())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(Verified::LoggedIn { user, secret })
    }

    /// Revokes the session with this hash if it belongs to `user`. Returns whether it did.
    ///
    /// # Errors
    ///
    /// When the database fails.
    pub async fn revoke_current(&self, user: &str, hash: SessionHash) -> Result<bool, LoginError> {
        let done = sqlx::query(REVOKE_CURRENT)
            .bind(hash.as_bytes().as_slice())
            .bind(user)
            .execute(self.db.pool())
            .await?;
        Ok(done.rows_affected() > 0)
    }

    /// Revokes every session of `user` that has not ended. Returns how many.
    ///
    /// # Errors
    ///
    /// When the database fails.
    pub async fn revoke_all(&self, user: &str) -> Result<u64, LoginError> {
        let done = sqlx::query(REVOKE_ALL)
            .bind(user)
            .execute(self.db.pool())
            .await?;
        Ok(done.rows_affected())
    }

    /// Deletes code rows older than [`CODE_RETENTION`] every [`CLEANUP_INTERVAL`] until the
    /// service stops, for [`service_kit::Context::spawn`]. Every replica does it; deleting twice
    /// is harmless.
    pub async fn clean_up(self, shutdown: Shutdown) {
        let mut interval = tokio::time::interval(CLEANUP_INTERVAL);
        loop {
            tokio::select! {
                () = shutdown.stopping() => return,
                _ = interval.tick() => {}
            }
            let deleted = tokio::time::timeout(
                ACQUIRE_TIMEOUT * 2,
                sqlx::query(DELETE_OLD_CODES)
                    .bind(CODE_RETENTION.as_secs_f64())
                    .execute(self.db.pool()),
            )
            .await;
            match deleted {
                Ok(Ok(done)) if done.rows_affected() > 0 => {
                    tracing::debug!(deleted = done.rows_affected(), "deleted old login codes")
                }
                Ok(Ok(_)) => {}
                Ok(Err(failure)) => tracing::warn!(
                    error_kind = error_kind(&failure),
                    "could not delete old login codes; trying again later"
                ),
                Err(_) => tracing::warn!("deleting old login codes timed out; trying again later"),
            }
        }
    }
}

/// The login route's state.
#[derive(Clone)]
pub struct Login {
    store: LoginStore,
    issuer: Issuer,
    queue: Option<mpsc::Sender<EmailAddress>>,
}

impl Login {
    /// The route's state and, when there is a relay, the worker that sends the codes, for
    /// [`service_kit::Context::spawn`]. Without a relay `send_code` answers 503. `store` serves
    /// the requests and `sends` (its own pool, [`SEND_POOL_CONNECTIONS`]) the worker. Codes are
    /// hashed with keys derived from `issuer`'s signing keys ([`Issuer::code_key`]).
    #[must_use]
    pub fn new(
        store: LoginStore,
        sends: LoginStore,
        mailer: Option<Mailer>,
        issuer: Issuer,
    ) -> (Self, Option<CodeWorker>) {
        let Some(mailer) = mailer else {
            return (
                Self {
                    store,
                    issuer,
                    queue: None,
                },
                None,
            );
        };
        let (sender, receiver) = mpsc::channel(SEND_QUEUE);
        let worker = CodeWorker {
            queue: receiver,
            store: sends,
            issuer: issuer.clone(),
            mailer,
        };
        (
            Self {
                store,
                issuer,
                queue: Some(sender),
            },
            Some(worker),
        )
    }
}

/// Issues and sends the codes that `send_code` asked for, [`CONCURRENT_SENDS`] at a time, off
/// the request path, on the send pool ([`SEND_POOL_CONNECTIONS`]).
pub struct CodeWorker {
    queue: mpsc::Receiver<EmailAddress>,
    store: LoginStore,
    issuer: Issuer,
    mailer: Mailer,
}

impl CodeWorker {
    /// Runs until the service stops, then lets the sends in progress finish (each within
    /// [`SEND_LIMIT`]; the kit's drain timeout cuts off the rest). Requests still queued are
    /// dropped: their users ask again.
    pub async fn run(mut self, shutdown: Shutdown) {
        let permits = Arc::new(Semaphore::new(CONCURRENT_SENDS));
        let mut running = JoinSet::new();
        loop {
            let permit = tokio::select! {
                () = shutdown.stopping() => break,
                permit = Arc::clone(&permits).acquire_owned() => permit,
            };
            let Ok(permit) = permit else { break };
            let email = tokio::select! {
                () = shutdown.stopping() => break,
                email = self.queue.recv() => email,
            };
            let Some(email) = email else { break };
            let store = self.store.clone();
            let issuer = self.issuer.clone();
            let mailer = self.mailer.clone();
            running.spawn(async move {
                issue_and_send(&store, &issuer, &mailer, &email).await;
                drop(permit);
            });
            // Collect the sends that finished, so the set does not grow.
            while running.try_join_next().is_some() {}
        }
        while running.join_next().await.is_some() {}
    }
}

/// Issues a code for `email` and mails it; logs the outcome by user ID, never the address or
/// the code.
async fn issue_and_send(
    store: &LoginStore,
    issuer: &Issuer,
    mailer: &Mailer,
    email: &EmailAddress,
) {
    // Checked before the database, so older codes stay usable when no code can be issued.
    let Some(key) = issuer.code_key(unix_now()) else {
        tracing::error!(
            "could not issue a login code: no signing key may sign now, and codes are hashed with a key derived from it"
        );
        return;
    };
    let (id, user, code) = match store.issue(email, &key).await {
        Ok(Issued::Code { id, user, code }) => (id, user, code),
        Ok(Issued::NoUser) => {
            tracing::info!("a login code was asked for an address no user has; nothing sent");
            return;
        }
        Ok(Issued::Throttled { user }) => {
            tracing::info!(
                user_id = %user,
                "did not send a login code: the address is at its send limit"
            );
            return;
        }
        Ok(Issued::Concurrent { user }) => {
            tracing::info!(
                user_id = %user,
                "did not send a login code: another one was issued at the same moment"
            );
            return;
        }
        Err(failure) => {
            tracing::error!(error = %failure, "could not issue a login code");
            return;
        }
    };
    let message = code_message(email, &code);
    drop(code);
    let sent = tokio::time::timeout(SEND_LIMIT, mailer.send(&message)).await;
    let failure = match sent {
        Ok(Ok(())) => {
            tracing::info!(user_id = %user, "sent a login code");
            return;
        }
        Ok(Err(failure)) => failure.to_string(),
        Err(_) => format!("sending took longer than {} s", SEND_LIMIT.as_secs()),
    };
    tracing::error!(user_id = %user, error = %failure, "could not send a login code");
    if let Err(failure) = store.invalidate(&id).await {
        tracing::error!(
            user_id = %user,
            error = %failure,
            "could not invalidate a login code that was not sent; it expires on its own"
        );
    }
}

/// The email with a login code: Traditional Chinese, then English. The code is only in the
/// body.
fn code_message(email: &EmailAddress, code: &LoginCode) -> Message {
    let minutes = CODE_LIFETIME.as_secs() / 60;
    let body = Zeroizing::new(format!(
        "您的 TrustIoT 登入驗證碼是：{code}\n\
         \n\
         驗證碼 {minutes} 分鐘內有效，只能使用一次。如果您沒有要求登入，請忽略這封信，\n\
         不要把驗證碼告訴任何人。\n\
         \n\
         Your TrustIoT login code is: {code}\n\
         \n\
         It is valid for {minutes} minutes and works once. If you did not ask to log in,\n\
         ignore this email and do not share the code with anyone.\n",
        code = code.as_str()
    ));
    Message::new(
        email.as_str().to_owned(),
        "TrustIoT 登入驗證碼 / Login code".to_owned(),
        body,
    )
}

/// Reads a login body: a JSON object that names one step. Serde would also read a JSON array
/// as the step followed by its fields in order (`["send_code", "user@example.com"]`); only the
/// object is the documented API, so an array, or anything else that is not an object, is
/// refused.
fn parse_request(body: &[u8]) -> Option<LoginRequest> {
    let first = body
        .iter()
        .find(|byte| !matches!(byte, b' ' | b'\t' | b'\n' | b'\r'));
    if first != Some(&b'{') {
        return None;
    }
    serde_json::from_slice(body).ok()
}

/// The login route: [`LOGIN_PATH`].
pub fn routes(login: Login) -> Router {
    Router::new()
        .route(LOGIN_PATH, post(log_in))
        .with_state(Arc::new(login))
}

/// One field error of a 422.
fn invalid(field: &str, key: MessageKey) -> FieldError {
    FieldError::new(field, key)
}

/// Logs in with an email code, in two steps (see the module documentation).
#[utoipa::path(
    post,
    path = "/auth/login",
    tag = "identity",
    operation_id = "logIn",
    request_body = LoginRequest,
    responses(
        (status = 200, description = "`verify_code`: the code matched. `Set-Cookie` holds the session cookie `__Host-trustiot-session` (`HttpOnly`, `Secure`, `SameSite=Lax`, `Path=/`).", body = LoggedIn),
        (status = 202, description = "`send_code`: if a user has the address, a code is on its way. The same answer whether or not one has.", body = CodeSent),
        (status = 400, description = "A query string, a `Content-Type` other than `application/json`, a body over 4096 bytes, or a body that is not one of the two steps; unknown fields such as `password` are refused (`invalid_request`).", body = ErrorBody),
        (status = 401, description = "`verify_code`: no usable code for the address, or a wrong code (`unauthenticated`). A code dies after 5 attempts and 10 minutes.", body = ErrorBody),
        (status = 403, description = "A cross-site request: `Sec-Fetch-Site` other than `same-origin`, or without it an `Origin` that is not this site's (`permission_denied`, message key `security.cross_site_request`). Checked before anything else; nothing changed.", body = ErrorBody),
        (status = 422, description = "The email is not an address (`/email`, `login.email_invalid`), the code is not 8 digits (`/code`, `login.code_invalid`), or `redirect_to` is not a path on this site (`/redirect_to`, `login.redirect_invalid`) (`validation_failed`).", body = ErrorBody),
        (status = 503, description = "No usable SMTP relay is configured, too many codes are waiting to be sent, the signing keys have not been read, or the database failed; nothing changed (`unavailable`).", body = ErrorBody),
    )
)]
async fn log_in(State(login): State<Arc<Login>>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    // First of all, as the ingress gateway's cookie check does for every other path: a request
    // another site made the browser send changes nothing, not even an attempt or a send limit.
    if let Some(header) = cross_site(&parts.method, &parts.headers) {
        // The header's name only: Origin is the client's to choose.
        tracing::info!(
            reason = "cross_site_request",
            header,
            "refused a login request"
        );
        return error(
            ErrorBody::new(ErrorCode::PermissionDenied).with_message_key(CROSS_SITE_REQUEST),
        );
    }
    // The code travels in the body only: a URL ends up in histories and access logs.
    if parts.uri.query().is_some() || !is_json(&parts.headers) {
        return error(ErrorBody::new(ErrorCode::InvalidRequest));
    }
    let Ok(body) = axum::body::to_bytes(body, MAX_BODY_BYTES).await else {
        return error(ErrorBody::new(ErrorCode::InvalidRequest));
    };
    let body = Zeroizing::new(body.to_vec());
    let Some(request) = parse_request(&body) else {
        return error(ErrorBody::new(ErrorCode::InvalidRequest));
    };
    match request {
        LoginRequest::SendCode(SendCode { email }) => send_code(&login, &email),
        LoginRequest::VerifyCode(request) => {
            let previous = session_secret(&parts.headers)
                .ok()
                .map(|secret| secret.hash());
            verify_code(&login, &request, previous).await
        }
    }
}

fn send_code(login: &Login, email: &str) -> Response {
    let Some(email) = EmailAddress::parse(email) else {
        return error(
            ErrorBody::new(ErrorCode::ValidationFailed)
                .with_field_error(invalid("/email", EMAIL_INVALID)),
        );
    };
    let Some(queue) = &login.queue else {
        tracing::warn!(
            "cannot send a login code: no usable SMTP relay (smtp.*; the startup log says why)"
        );
        return error(ErrorBody::new(ErrorCode::Unavailable));
    };
    match queue.try_send(email) {
        Ok(()) => (
            StatusCode::ACCEPTED,
            [(CACHE_CONTROL, "no-store")],
            axum::Json(CodeSent::now()),
        )
            .into_response(),
        Err(mpsc::error::TrySendError::Full(_)) => {
            tracing::warn!(
                queued = SEND_QUEUE,
                "cannot take a login code request: the send queue is full"
            );
            error(ErrorBody::new(ErrorCode::Unavailable))
        }
        Err(mpsc::error::TrySendError::Closed(_)) => error(ErrorBody::new(ErrorCode::Unavailable)),
    }
}

async fn verify_code(
    login: &Login,
    request: &VerifyCode,
    previous: Option<SessionHash>,
) -> Response {
    let email = EmailAddress::parse(&request.email);
    let code = LoginCode::parse(&request.code);
    let redirect = redirect_target(request.redirect_to.as_deref());
    let (Some(email), Some(code), Some(redirect)) = (email.clone(), code.clone(), redirect.clone())
    else {
        let mut body = ErrorBody::new(ErrorCode::ValidationFailed);
        for (field, key, ok) in [
            ("/email", EMAIL_INVALID, email.is_some()),
            ("/code", CODE_INVALID, code.is_some()),
            ("/redirect_to", REDIRECT_INVALID, redirect.is_some()),
        ] {
            if !ok {
                body = body.with_field_error(invalid(field, key));
            }
        }
        return error(body);
    };
    let keys = login.issuer.code_keys();
    if keys.is_empty() {
        // No attempt is counted: without a key no code can be checked.
        tracing::error!("could not check a login code: no signing keys have been read");
        return error(ErrorBody::new(ErrorCode::Unavailable));
    }
    let started = tokio::time::Instant::now();
    let verified = login.store.verify(&email, &code, &keys, previous).await;
    if matches!(
        verified,
        Ok(Verified::NoUsableCode | Verified::WrongCode { .. })
    ) {
        // Both refusals take as long (`REFUSAL_FLOOR`), so the time does not tell them apart.
        tokio::time::sleep_until(started + REFUSAL_FLOOR).await;
    }
    match verified {
        Ok(Verified::LoggedIn { user, secret }) => match set_cookie(&secret) {
            Ok(cookie) => {
                tracing::info!(user_id = %user, "logged in with an email code");
                (
                    StatusCode::OK,
                    [
                        (
                            CACHE_CONTROL,
                            axum::http::HeaderValue::from_static("no-store"),
                        ),
                        (SET_COOKIE, cookie),
                    ],
                    axum::Json(LoggedIn {
                        redirect_to: redirect,
                    }),
                )
                    .into_response()
            }
            Err(_) => error(ErrorBody::new(ErrorCode::Unavailable)),
        },
        Ok(Verified::NoUsableCode) => {
            tracing::info!(reason = "no_usable_code", "refused a login code");
            error(ErrorBody::new(ErrorCode::Unauthenticated))
        }
        Ok(Verified::WrongCode { user }) => {
            tracing::info!(user_id = %user, reason = "wrong_code", "refused a login code");
            error(ErrorBody::new(ErrorCode::Unauthenticated))
        }
        Err(failure) => {
            tracing::error!(error = %failure, "could not check a login code");
            error(ErrorBody::new(ErrorCode::Unavailable))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_eight_digits_and_never_printed() {
        for _ in 0..1000 {
            let code = LoginCode::generate().expect("randomness");
            assert_eq!(code.as_str().len(), CODE_DIGITS);
            assert!(code.as_str().bytes().all(|byte| byte.is_ascii_digit()));
            assert_eq!(LoginCode::parse(code.as_str()).as_ref(), Some(&code));
            assert_eq!(format!("{code:?}"), "LoginCode(<redacted>)");
        }
        for invalid in [
            "",
            "1234567",
            "123456789",
            "1234 678",
            "abcdefgh",
            " 12345678",
            "１２３４５６７８",
        ] {
            assert!(LoginCode::parse(invalid).is_none(), "{invalid:?}");
        }
    }

    /// Not a statistical test of the CSPRNG: it catches a draw that is not spread over the
    /// whole range, such as a fixed or truncated value.
    #[test]
    fn codes_use_every_digit_position() {
        let mut seen = [[false; 10]; CODE_DIGITS];
        for _ in 0..2000 {
            let code = LoginCode::generate().expect("randomness");
            for (position, digit) in code.as_str().bytes().enumerate() {
                if let Some(slot) = seen
                    .get_mut(position)
                    .and_then(|digits| digits.get_mut(usize::from(digit - b'0')))
                {
                    *slot = true;
                }
            }
        }
        assert!(seen.iter().flatten().all(|seen| *seen), "{seen:?}");
    }

    #[test]
    fn the_code_hash_binds_the_key_the_purpose_the_address_and_the_code() {
        use sha2::Digest;

        // A throwaway key; the service derives its keys from the signing keys.
        let key = [7_u8; 32];
        let other_key = [8_u8; 32];
        let email = EmailAddress::parse("user@example.com").expect("an address");
        let other = EmailAddress::parse("other@example.com").expect("an address");
        let code = LoginCode::parse("12345678").expect("a code");
        let wrong = LoginCode::parse("12345679").expect("a code");
        let hash = code_hash(&key, &email, &code);
        assert_eq!(hash, code_hash(&key, &email, &code));
        assert_ne!(hash, code_hash(&other_key, &email, &code));
        assert_ne!(hash, code_hash(&key, &other, &code));
        assert_ne!(hash, code_hash(&key, &email, &wrong));
        // Not an unkeyed SHA-256, which anyone who reads the table could test all 10⁸ codes
        // against.
        let bare: [u8; 32] = Sha256::digest(b"12345678").into();
        assert_ne!(hash, bare);
        let unkeyed: [u8; 32] =
            Sha256::digest(b"trustiot-login-code:v1\nuser@example.com\n12345678".as_slice()).into();
        assert_ne!(hash, unkeyed);
        // Computed independently (OpenSSL and Bun agree):
        // printf 'trustiot-login-code:v1\nuser@example.com\n12345678' |
        //   openssl dgst -sha256 -mac HMAC -macopt hexkey:<32 bytes of 07>
        let hex: String = hash.iter().map(|byte| format!("{byte:02x}")).collect();
        assert_eq!(
            hex,
            "45c0a7d2297c42c1acb664f417afc1630cf862e48585b651a12f0e5679e52469"
        );
    }

    #[test]
    fn only_paths_on_this_site_are_redirect_targets() {
        assert_eq!(redirect_target(None).as_deref(), Some("/"));
        for valid in [
            "/",
            "/devices",
            "/devices?page=2&sort=name",
            "/devices#top",
            "/a/b/c",
            "/%2F%2Fevil.example",
            "/search?q=https://evil.example",
        ] {
            assert_eq!(
                redirect_target(Some(valid)).as_deref(),
                Some(valid),
                "{valid:?}"
            );
        }
        let long = format!("/{}", "a".repeat(MAX_REDIRECT_BYTES));
        for invalid in [
            "",
            "https://evil.example",
            "https://evil.example/",
            "http://app.saas.trustiot.test/devices",
            "//evil.example",
            "//evil.example/path",
            "///evil.example",
            "/\\evil.example",
            "\\\\evil.example",
            "\\/evil.example",
            "/\t/evil.example",
            "/\n/evil.example",
            "/\r\n/evil.example",
            " /devices",
            "/devices ",
            "/dev ices",
            "javascript:alert(1)",
            "JaVaScRiPt:alert(1)",
            "data:text/html,x",
            "evil.example",
            "devices",
            "?next=/",
            "#/devices",
            "/caf\u{e9}",
            "/\u{ff0f}evil.example",
            "/\u{0}",
            long.as_str(),
        ] {
            assert_eq!(redirect_target(Some(invalid)), None, "{invalid:?}");
        }
    }

    #[test]
    fn the_request_names_one_step_and_nothing_else() {
        let parse = |json: &str| parse_request(json.as_bytes());
        assert!(matches!(
            parse(r#"{"step":"send_code","email":"user@example.com"}"#),
            Some(LoginRequest::SendCode(_))
        ));
        assert!(matches!(
            parse(" \r\n\t{\"step\":\"send_code\",\"email\":\"user@example.com\"}\n"),
            Some(LoginRequest::SendCode(_))
        ));
        assert!(matches!(
            parse(r#"{"email":"user@example.com","code":"12345678","step":"verify_code"}"#),
            Some(LoginRequest::VerifyCode(VerifyCode {
                redirect_to: None,
                ..
            }))
        ));
        assert!(matches!(
            parse(
                r#"{"step":"verify_code","email":"u@example.com","code":"12345678","redirect_to":"/x"}"#
            ),
            Some(LoginRequest::VerifyCode(VerifyCode {
                redirect_to: Some(_),
                ..
            }))
        ));
        // Serde alone reads an array as the step and its fields in order.
        assert!(
            serde_json::from_slice::<LoginRequest>(br#"["send_code","user@example.com"]"#).is_ok()
        );
        for refused in [
            r#"{"email":"user@example.com"}"#,
            r#"{"step":"password","email":"user@example.com","password":"x"}"#,
            r#"{"step":"send_code","email":"user@example.com","password":"x"}"#,
            r#"{"step":"verify_code","email":"user@example.com","password":"x"}"#,
            r#"{"step":"verify_code","email":"user@example.com","code":"12345678","password":"x"}"#,
            r#"{"step":"sign_up","email":"user@example.com"}"#,
            r#"{"step":"dev_login","email":"user@example.com"}"#,
            r#"{"step":"send_code","email":"user@example.com","email":"other@example.com"}"#,
            r#"{"step":"send_code"}"#,
            r#"{"step":"send_code","email":1}"#,
            r#"["send_code","user@example.com"]"#,
            r#" ["verify_code","user@example.com","12345678",null]"#,
            r#""send_code""#,
            "null",
            "1",
            "",
        ] {
            assert!(parse(refused).is_none(), "{refused}");
        }
    }

    /// The documented body is the one the service reads: every body [`parse_request`] accepts
    /// matches exactly one step of the OpenAPI schema, and every body it refuses for a field
    /// matches none. A small JSON Schema check of the keywords the schema uses (`oneOf`, `type`,
    /// `properties`, `enum`, `required`, `additionalProperties: false`).
    #[test]
    fn the_documented_body_is_the_one_that_is_read() {
        use serde_json::{Value, json};

        fn type_matches(property: &Value, value: &Value) -> bool {
            let name = match value {
                Value::String(_) => "string",
                Value::Null => "null",
                _ => return false,
            };
            match &property["type"] {
                Value::String(single) => single == name,
                Value::Array(types) => types.iter().any(|kind| kind == name),
                _ => false,
            }
        }
        fn step_matches(step: &Value, body: &Value) -> bool {
            let Some(body) = body.as_object() else {
                return false;
            };
            let required = step["required"].as_array().expect("required");
            step["type"] == "object"
                && step["additionalProperties"] == false
                && body.iter().all(|(key, value)| {
                    let property = &step["properties"][key];
                    property.is_object()
                        && type_matches(property, value)
                        && property["enum"]
                            .as_array()
                            .is_none_or(|values| values.contains(value))
                })
                && required
                    .iter()
                    .all(|key| key.as_str().is_some_and(|key| body.contains_key(key)))
        }

        let schema = serde_json::to_value(<LoginRequest as utoipa::PartialSchema>::schema())
            .expect("the schema as JSON");
        let steps = schema["oneOf"].as_array().expect("oneOf");
        assert_eq!(steps.len(), 2, "{schema}");
        let matching = |body: &Value| steps.iter().filter(|step| step_matches(step, body)).count();
        let email = "user@example.com";
        for read in [
            json!({"step": "send_code", "email": email}),
            json!({"step": "verify_code", "email": email, "code": "12345678"}),
            json!({"step": "verify_code", "email": email, "code": "12345678", "redirect_to": "/x"}),
            json!({"step": "verify_code", "email": email, "code": "12345678", "redirect_to": null}),
        ] {
            assert!(
                parse_request(read.to_string().as_bytes()).is_some(),
                "{read}"
            );
            assert_eq!(matching(&read), 1, "{read}");
        }
        for refused in [
            json!({"step": "send_code", "email": email, "password": "x"}),
            json!({"step": "send_code", "email": email, "code": "12345678"}),
            json!({"step": "verify_code", "email": email, "code": "12345678", "password": "x"}),
            json!({"step": "verify_code", "email": email}),
            json!({"step": "password", "email": email, "password": "x"}),
            json!({"step": "send_code"}),
            json!({"email": email}),
            json!(["send_code", email]),
        ] {
            assert!(
                parse_request(refused.to_string().as_bytes()).is_none(),
                "{refused}"
            );
            assert_eq!(matching(&refused), 0, "{refused}");
        }
    }

    #[test]
    fn the_email_holds_the_code_in_its_body_only() {
        let email = EmailAddress::parse("user@example.com").expect("an address");
        let code = LoginCode::parse("90817263").expect("a code");
        let message = code_message(&email, &code);
        assert_eq!(message.to(), "user@example.com");
        assert_eq!(
            message.body().matches("90817263").count(),
            2,
            "{}",
            message.body()
        );
        assert!(!message.subject().contains("90817263"));
        assert_eq!(format!("{message:?}"), "Message(<redacted>)");
        let debug = format!("{:?}", (&email, &code));
        assert!(!debug.contains("90817263") && !debug.contains("user@example.com"));
    }
}
