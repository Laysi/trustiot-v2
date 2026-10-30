//! Server-side sessions (decision 13): the session cookie, its hash, and the store that turns a
//! cookie into the user, tenant and role a request acts for.
//!
//! The cookie carries only a 256-bit random secret. The database keeps its SHA-256, never the
//! secret, so a copy of the sessions table cannot be replayed as cookies, and there is no session
//! signing key that could be hard-coded or leak (docs/requirements/rebuild-checklist.md A1).
//! Everything about the session (user, tenant, expiry, revocation, role) is read from the
//! database on every check, so logout, revocation, role changes and a suspension of the tenant
//! apply to the next request.
//!
//! Lifetime (services/identity/README.md, "session 的期限"): a session created by logging in
//! ([`crate::login`]) expires [`SESSION_IDLE_LIFETIME`] after its last use, and never later than
//! [`SESSION_ABSOLUTE_LIFETIME`] after it was created. The check extends it in the background
//! ([`PgSessionStore`]), so the browser never renews anything.

use std::fmt;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use axum::http::header::COOKIE;
use axum::http::{HeaderMap, HeaderValue};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use contract::{SessionId, TenantGrant, TenantId, TenantRole, UserId};
use service_kit::identity::ServiceIdentity;
use service_kit::postgres::InsecureConnection;
use service_kit::shutdown::Shutdown;
use service_kit::tenant::{BackgroundTenantDb, TenantDb, TenantDbError};
use sha2::{Digest, Sha256};
use sqlx::Postgres;
use sqlx::pool::PoolConnection;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::tenant_state::TenantState;

/// The session cookie. The `__Host-` prefix makes browsers accept it only when it was set by this
/// host over HTTPS with `Path=/` and no `Domain`, so a sibling subdomain cannot plant or overwrite
/// it. Logging in sets it with `HttpOnly`, `Secure` and `SameSite=Lax` ([`set_cookie`]).
pub const SESSION_COOKIE: &str = "__Host-trustiot-session";

/// Bytes of randomness in a session secret.
pub const SESSION_SECRET_BYTES: usize = 32;

/// Length of the cookie value: [`SESSION_SECRET_BYTES`] in base64url without padding.
pub const SESSION_COOKIE_LEN: usize = 43;

/// How long a session lasts after its last use: 12 hours, the session length of checklist A1.
/// Logging in sets `expires_at` this far ahead, and the check moves it forward as the session is
/// used ([`SESSION_EXTEND_STEP`]).
pub const SESSION_IDLE_LIFETIME: Duration = Duration::from_secs(12 * 3600);

/// The longest a session lives, used or not: 7 days from its creation. Then the user logs in
/// again. `migrations/postgres/0004_login.sql` enforces the same bound; a test compares them.
pub const SESSION_ABSOLUTE_LIFETIME: Duration = Duration::from_secs(7 * 24 * 3600);

/// The check extends a session only when that moves its expiry by at least this much, so an
/// active session costs one write per 10 minutes, not one per request.
pub const SESSION_EXTEND_STEP: Duration = Duration::from_secs(10 * 60);

/// Extensions that may run at once per replica. Beyond that, or when no connection of the
/// check's pool is idle, a check skips the extension; the session's next request extends it.
pub const EXTENSIONS_IN_FLIGHT: usize = 8;

/// How long getting a database connection may take, opening a new one included (TCP, TLS,
/// SCRAM and service-kit's session check): the pool's acquire timeout. It is far longer than an
/// ext_authz check may take ([`crate::ext_authz::CHECK_DEADLINE`]) on purpose: a check that gives
/// up does not cancel the connection it was waiting for (see [`PgSessionStore`]), and a pool
/// whose new connections were cut off at the check deadline would never get any on a slow
/// network or a loaded node (finding F1 of docs/test-reports/2026-10-11-61-verify.md).
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Connections the pool keeps open, and the fewest a replica may be configured with. One check
/// holds up to two at once (the session query's connection is still being returned while the
/// membership transaction starts on another), and the keep-warm task one more, so a steady
/// stream of checks never waits for a new connection.
pub const WARM_CONNECTIONS: u32 = 3;

/// How often the keep-warm task checks the pool ([`PgSessionStore::keep_warm`]).
pub const KEEP_WARM_INTERVAL: Duration = Duration::from_secs(2);

/// How long a keep-warm round waits before it looks again for a connection it has not confirmed
/// yet, while checks hold the others or new ones are still being opened.
const CONFIRM_POLL: Duration = Duration::from_millis(50);

/// The probe a keep-warm round runs on each connection. It fails on a connection the database
/// dropped, and the backend process ID tells open connections apart, so a round can confirm
/// [`WARM_CONNECTIONS`] different ones while it holds only one at a time.
const BACKEND_PID: &str = "SELECT pg_backend_pid()";

/// Upper bound on one lookup or one keep-warm round, [`CONNECT_TIMEOUT`] included, so a database
/// that stops answering cannot hold tasks and connections forever.
pub const WORK_LIMIT: Duration = Duration::from_secs(10);

/// Lookups that may be in flight per pool connection. Beyond that a check fails at once with
/// 503 instead of queueing work that would outlive the check anyway.
pub const IN_FLIGHT_PER_CONNECTION: u32 = 4;

/// The secret of one session, as carried in [`SESSION_COOKIE`]. `Debug` never prints it, there
/// is no `Display` or `Serialize`, and the bytes are wiped on drop.
#[derive(Clone, PartialEq, Eq)]
pub struct SessionSecret([u8; SESSION_SECRET_BYTES]);

impl SessionSecret {
    /// Wraps raw secret bytes, for test tooling that creates sessions. The bytes must come from
    /// a CSPRNG; the service itself uses [`SessionSecret::generate`].
    #[must_use]
    pub const fn from_bytes(bytes: [u8; SESSION_SECRET_BYTES]) -> Self {
        Self(bytes)
    }

    /// A new secret from the operating system's CSPRNG.
    ///
    /// # Errors
    ///
    /// When the operating system has no randomness to give.
    pub fn generate() -> Result<Self, getrandom::Error> {
        let mut secret = Self([0; SESSION_SECRET_BYTES]);
        getrandom::fill(&mut secret.0)?;
        Ok(secret)
    }

    /// Reads a cookie value: exactly [`SESSION_COOKIE_LEN`] characters of base64url without
    /// padding and with zero trailing bits, so every secret has exactly one spelling.
    #[must_use]
    pub fn parse(value: &[u8]) -> Option<Self> {
        if value.len() != SESSION_COOKIE_LEN {
            return None;
        }
        let decoded = zeroize::Zeroizing::new(URL_SAFE_NO_PAD.decode(value).ok()?);
        let bytes = <[u8; SESSION_SECRET_BYTES]>::try_from(decoded.as_slice()).ok()?;
        Some(Self(bytes))
    }

    /// The cookie value. Never log it.
    #[must_use]
    pub fn to_cookie_value(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.0)
    }

    /// The SHA-256 of the secret, which is what the database stores and looks up.
    #[must_use]
    pub fn hash(&self) -> SessionHash {
        SessionHash(Sha256::digest(self.0).into())
    }
}

impl Drop for SessionSecret {
    fn drop(&mut self) {
        zeroize::Zeroize::zeroize(&mut self.0);
    }
}

impl fmt::Debug for SessionSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SessionSecret(<redacted>)")
    }
}

/// The SHA-256 of a [`SessionSecret`]: the key of the sessions table.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct SessionHash([u8; 32]);

impl SessionHash {
    /// The hash bytes, as stored in `identity.sessions.token_hash`.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Debug for SessionHash {
    /// Not secret, but it identifies a live session, so it stays out of logs too.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SessionHash(<redacted>)")
    }
}

/// Why a request carries no usable session cookie.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CookieError {
    /// No [`SESSION_COOKIE`] in any `Cookie` header.
    #[error("the request has no session cookie")]
    Missing,
    /// [`SESSION_COOKIE`] appears more than once. With the `__Host-` prefix a browser never sends
    /// two, so this is a planted cookie or a broken client; neither is guessed between.
    #[error("the request has more than one session cookie")]
    Repeated,
    /// The cookie's value is not a session secret.
    #[error("the session cookie is malformed")]
    Malformed,
}

impl CookieError {
    /// A stable name for logs.
    #[must_use]
    pub const fn reason(self) -> &'static str {
        match self {
            Self::Missing => "no_session_cookie",
            Self::Repeated => "repeated_session_cookie",
            Self::Malformed => "malformed_session_cookie",
        }
    }
}

/// Finds the session secret in a request's `Cookie` headers (RFC 6265 section 5.4: `name=value`
/// pairs separated by `;`, possibly split over several headers by HTTP/2). Works on bytes, so
/// another cookie with characters outside visible ASCII does not get in the way; names compare
/// exactly, as browsers send them.
///
/// # Errors
///
/// When there is no session cookie, more than one, or its value is not a session secret.
pub fn session_secret(headers: &HeaderMap) -> Result<SessionSecret, CookieError> {
    let mut found: Option<&[u8]> = None;
    for header in headers.get_all(COOKIE) {
        for pair in header.as_bytes().split(|byte| *byte == b';') {
            let pair = pair.trim_ascii();
            let Some(equals) = pair.iter().position(|byte| *byte == b'=') else {
                continue;
            };
            let (name, value) = pair.split_at(equals);
            if name.trim_ascii() != SESSION_COOKIE.as_bytes() {
                continue;
            }
            if found.is_some() {
                return Err(CookieError::Repeated);
            }
            // Skip the `=`; the value may not be surrounded by spaces or quotes.
            found = Some(value.get(1..).unwrap_or_default());
        }
    }
    let value = found.ok_or(CookieError::Missing)?;
    SessionSecret::parse(value).ok_or(CookieError::Malformed)
}

/// The attributes of the session cookie. `__Host-` requires `Secure`, `Path=/` and no `Domain`.
/// `HttpOnly` keeps it from scripts. `SameSite=Lax` leaves it off requests that other sites
/// start, except a top-level navigation (a link to the app still arrives logged in); every write
/// that the cookie authenticates needs a CORS preflight on top, which nothing allows
/// (services/identity/README.md, "與入口的約定").
const COOKIE_ATTRIBUTES: &str = "Path=/; Secure; HttpOnly; SameSite=Lax";

/// The `Set-Cookie` value that hands a new session to the browser. The browser keeps it for
/// [`SESSION_ABSOLUTE_LIFETIME`]; whether the session is still active is decided by the server.
/// Never log it.
///
/// # Errors
///
/// Never in practice: the value is visible ASCII.
pub fn set_cookie(
    secret: &SessionSecret,
) -> Result<HeaderValue, axum::http::header::InvalidHeaderValue> {
    let value = zeroize::Zeroizing::new(format!(
        "{SESSION_COOKIE}={}; Max-Age={}; {COOKIE_ATTRIBUTES}",
        zeroize::Zeroizing::new(secret.to_cookie_value()).as_str(),
        SESSION_ABSOLUTE_LIFETIME.as_secs()
    ));
    let mut header = HeaderValue::from_str(&value)?;
    header.set_sensitive(true);
    Ok(header)
}

/// The `Set-Cookie` value that removes the session cookie from the browser (logout).
#[must_use]
pub fn clear_cookie() -> HeaderValue {
    HeaderValue::from_static(CLEAR_COOKIE)
}

/// [`clear_cookie`]: an empty value that expires at once, with the same attributes.
const CLEAR_COOKIE: &str =
    "__Host-trustiot-session=; Max-Age=0; Path=/; Secure; HttpOnly; SameSite=Lax";

/// What an active session grants: the user, the tenant the user acts in with their role there,
/// and whether the user is a platform administrator.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionGrant {
    /// The session itself, for the token's `sid` claim: an agent run's delegation is bound to it
    /// (Laysi/trustiot-v2#551).
    pub session: SessionId,
    /// The user the session belongs to.
    pub user: UserId,
    /// The session's tenant and the user's current role in it; `None` when the session has not
    /// picked a tenant or the user is no longer a member of it.
    pub tenant: Option<TenantGrant>,
    /// Whether the user is a platform administrator (A2).
    pub platform_admin: bool,
    /// Whether the session's tenant is suspended or deleted (checklist B1,
    /// Laysi/trustiot-v2#777), read with the role; `false` without a tenant. The cookie check
    /// then refuses every route but the identity service's own, and signs that one with the
    /// `tenant_suspended` claim.
    pub tenant_suspended: bool,
}

/// Why the store could not answer. The ext_authz check answers 503 for all of them, which the
/// ingress gateway turns into its own 503 (deploy/istio/README.md).
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The database failed or refused the session. The message is the [`TenantDbError`]'s own,
    /// which already names the database, so it is not prefixed again.
    #[error(transparent)]
    Database(#[from] TenantDbError),
    /// A row holds a value the schema forbids, so it is not trusted.
    #[error("the sessions data is inconsistent: {0}")]
    Inconsistent(&'static str),
    /// [`IN_FLIGHT_PER_CONNECTION`] lookups per connection are already waiting for the database.
    #[error("too many session lookups are in flight")]
    Busy,
    /// The lookup or keep-warm round did not finish within [`WORK_LIMIT`].
    #[error("the database did not answer in time")]
    TimedOut,
    /// The lookup ended without an answer: it panicked, or the service is stopping.
    #[error("the session lookup was interrupted")]
    Interrupted,
    /// The pool does not hold its [`WARM_CONNECTIONS`] (yet), so checks could wait for new ones.
    #[error("the database connections are not open yet")]
    NotWarm,
}

impl From<sqlx::Error> for StoreError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(TenantDbError::Database(error))
    }
}

/// Looks up active sessions. [`PgSessionStore`] is the real one; tests use their own.
pub trait SessionStore: Send + Sync + 'static {
    /// The grant of the active session with this hash: not expired, not revoked. `None` when
    /// there is none.
    fn lookup(
        &self,
        hash: SessionHash,
    ) -> impl Future<Output = Result<Option<SessionGrant>, StoreError>> + Send;

    /// Readiness: fails while the store cannot answer every check in time.
    fn ping(&self) -> impl Future<Output = Result<(), StoreError>> + Send;
}

/// The session and the user, without tenant data: neither table has a `tenant_id`, because a
/// user and their sessions span tenants (services/identity/README.md). Expiry is decided by the
/// database clock. The last column says whether extending the session now would move its expiry
/// by at least [`SESSION_EXTEND_STEP`]: `$2` is [`SESSION_IDLE_LIFETIME`], `$3`
/// [`SESSION_ABSOLUTE_LIFETIME`], `$4` [`SESSION_EXTEND_STEP`], all in seconds.
const ACTIVE_SESSION: &str = "SELECT s.id::text, s.user_id::text, s.active_tenant_id::text,
    u.platform_admin,
    LEAST(now() + make_interval(secs => $2), s.created_at + make_interval(secs => $3))
        >= s.expires_at + make_interval(secs => $4)
FROM identity.sessions AS s
JOIN identity.users AS u ON u.id = s.user_id
WHERE s.token_hash = $1 AND s.revoked_at IS NULL AND s.expires_at > now()";

/// Moves an active session's expiry to [`SESSION_IDLE_LIFETIME`] from now, but never past
/// [`SESSION_ABSOLUTE_LIFETIME`] after its creation and never backwards. `$1` is the session ID,
/// `$2` and `$3` the two lifetimes in seconds.
const EXTEND_SESSION: &str = "UPDATE identity.sessions
SET expires_at = LEAST(now() + make_interval(secs => $2), created_at + make_interval(secs => $3))
WHERE id = $1::uuid AND revoked_at IS NULL AND expires_at > now()
    AND LEAST(now() + make_interval(secs => $2), created_at + make_interval(secs => $3))
        > expires_at";

/// The user's role in the transaction's tenant and the tenant's state (Laysi/trustiot-v2#777),
/// read together, so a suspension applies from the next request on like a role change.
/// Row-level security already limits both tables to that tenant; the tenant condition is
/// repeated so the query means the same without it. A membership whose tenant has no row (only
/// data written outside the API has one) reads as active: suspending a tenant needs its row (the
/// state API answers 404 without one), and the service role cannot delete a row, so no suspended
/// tenant ever loses it.
pub(crate) const MEMBERSHIP: &str = "SELECT m.tenant_role, t.state
FROM identity.memberships AS m
LEFT JOIN identity.tenants AS t ON t.tenant_id = m.tenant_id
WHERE m.tenant_id = $1::uuid AND m.user_id = $2::uuid";

/// Names the work in the kit's DEBUG log of each tenant transaction.
const MEMBERSHIP_PURPOSE: &str = "ext-authz-membership";

/// The sessions in Postgres (`migrations/postgres`), read with the service role.
///
/// Memberships are tenant data under row-level security, so the role is read in a transaction
/// for the session's tenant. That tenant comes from the session row in the database, never from
/// the request, so this store holds the background handle that can open a transaction for a
/// given tenant and keeps it private: its only use is [`PgSessionStore::lookup`], which takes the
/// tenant from the row it just read.
///
/// Opening a connection must never race the check deadline (services/identity/README.md,
/// "連線池"):
///
/// - Each lookup runs in its own task, which the check does not cancel when it gives up: a
///   connection the lookup was opening is finished and kept for the next check. At most
///   [`IN_FLIGHT_PER_CONNECTION`] lookups per connection run at once, each for at most
///   [`WORK_LIMIT`].
/// - [`PgSessionStore::keep_warm`] keeps [`WARM_CONNECTIONS`] open and confirms all of them every
///   round, and readiness passes only while they are open, so the gateway sends checks only to a
///   replica that will not have to open a connection for them.
///
/// A lookup that finds the session due for extension ([`SESSION_EXTEND_STEP`]) starts the
/// extension on another task and answers without waiting for it; at most
/// [`EXTENSIONS_IN_FLIGHT`] run at once, each only on a connection that is idle when the lookup
/// ends, and one that is skipped or fails is left to the session's next request.
#[derive(Clone, Debug)]
pub struct PgSessionStore {
    db: TenantDb,
    memberships: BackgroundTenantDb,
    in_flight: Arc<Semaphore>,
    extensions: Arc<Semaphore>,
    warm: Arc<AtomicBool>,
}

impl PgSessionStore {
    /// A lazily connecting store with up to `max_connections` connections (at least
    /// [`WARM_CONNECTIONS`], which [`Config`](crate::config::Config) enforces). Nothing connects
    /// until [`PgSessionStore::keep_warm`] or a lookup runs.
    ///
    /// # Errors
    ///
    /// When `options` would not verify the server.
    pub fn connect_lazy(
        options: PgConnectOptions,
        max_connections: u32,
    ) -> Result<Self, InsecureConnection> {
        // The default lifetime (30 min) and idle timeout (10 min) stay, so service-kit's session
        // check runs again on every replacement: a role or policy that went wrong is caught
        // within 30 minutes. `min_connections` opens the replacements in the background rather
        // than in a check.
        let pool = PgPoolOptions::new()
            .max_connections(max_connections)
            .min_connections(WARM_CONNECTIONS.min(max_connections))
            .acquire_timeout(CONNECT_TIMEOUT);
        let actor = ServiceIdentity::new(contract::ServiceName::Identity);
        let (db, memberships) = TenantDb::connect_lazy_with_background(options, pool, actor)?;
        let permits = usize::try_from(max_connections.saturating_mul(IN_FLIGHT_PER_CONNECTION))
            .unwrap_or(Semaphore::MAX_PERMITS)
            .min(Semaphore::MAX_PERMITS);
        Ok(Self {
            db,
            memberships,
            in_flight: Arc::new(Semaphore::new(permits)),
            extensions: Arc::new(Semaphore::new(EXTENSIONS_IN_FLIGHT)),
            warm: Arc::new(AtomicBool::new(false)),
        })
    }

    /// The service role's pool, for queries outside a tenant: the migrations readiness check
    /// (`service_kit::migrations::MigrationCheck`), which reads the schema's migration records
    /// until they are complete and then never again.
    #[must_use]
    pub fn pool(&self) -> &sqlx::PgPool {
        self.db.pool()
    }

    /// Keeps [`WARM_CONNECTIONS`] open until the service stops, for
    /// [`service_kit::Context::spawn`]: every [`KEEP_WARM_INTERVAL`] a round
    /// ([`PgSessionStore::warm_up`], at most [`WORK_LIMIT`]) confirms that many different
    /// connections answer and opens what is missing. Readiness ([`SessionStore::ping`]) passes
    /// once a round has confirmed them, and fails when a round fails or as soon as it finds a
    /// connection the database dropped. Logs when readiness changes, never a secret.
    pub async fn keep_warm(self, shutdown: Shutdown) {
        let mut readiness = Readiness::new(Arc::clone(&self.warm));
        loop {
            let outcome = tokio::select! {
                () = shutdown.stopping() => return,
                outcome = tokio::time::timeout(WORK_LIMIT, self.warm_up(&mut readiness)) => {
                    outcome.unwrap_or(Err(StoreError::TimedOut))
                }
            };
            match outcome {
                Ok(()) => readiness.ready(),
                Err(error) => readiness.not_ready(&error),
            }
            tokio::select! {
                () = shutdown.stopping() => return,
                () = tokio::time::sleep(KEEP_WARM_INTERVAL) => {}
            }
        }
    }

    /// One keep-warm round: confirms that [`WARM_CONNECTIONS`] different connections answer
    /// [`BACKEND_PID`]. It holds one connection at a time and gives it back before it takes the
    /// next, so the two a check may need stay free (see [`WARM_CONNECTIONS`]).
    ///
    /// 1. It probes every connection that is idle when the round starts. The pool hands out idle
    ///    connections first in, first out, and a connection given back joins the end, so each of
    ///    them is taken by this round or by a check (whose acquire pings it first) before a
    ///    connection given back comes up again. A connection the database dropped, as in a
    ///    failover, fails its probe: readiness fails at once and the connection is closed, and
    ///    the pool's minimum opens a replacement in the background.
    /// 2. Then, until [`WARM_CONNECTIONS`] different connections have answered, it probes the next
    ///    idle one, opens what the pool lacks below [`WARM_CONNECTIONS`], or waits
    ///    [`CONFIRM_POLL`] for checks to give theirs back or new ones to open.
    ///
    /// The pool's own acquire would instead ping an idle connection and silently reopen a dropped
    /// one, so the round takes idle connections with `try_acquire`, which hands them out as they
    /// are.
    async fn warm_up(&self, readiness: &mut Readiness) -> Result<(), StoreError> {
        let pool = self.db.pool();
        let mut confirmed = Vec::with_capacity(WARM_CONNECTIONS as usize);
        for _ in 0..pool.num_idle() {
            let Some(connection) = pool.try_acquire() else {
                break;
            };
            probe(connection, &mut confirmed, readiness).await;
        }
        while confirmed.len() < WARM_CONNECTIONS as usize {
            if let Some(connection) = pool.try_acquire() {
                if probe(connection, &mut confirmed, readiness).await {
                    continue;
                }
            } else if let Some(missing) = WARM_CONNECTIONS
                .checked_sub(pool.size())
                .filter(|missing| *missing > 0)
            {
                self.open(missing).await?;
                continue;
            }
            tokio::time::sleep(CONFIRM_POLL).await;
        }
        Ok(())
    }

    /// Opens up to `missing` connections at once: each `SELECT 1` opens one when no connection
    /// is idle (a new connection passes service-kit's session check first) and gives it back to
    /// the pool. Errors name a session the check refused.
    async fn open(&self, missing: u32) -> Result<(), StoreError> {
        let mut opening = JoinSet::new();
        for _ in 0..missing {
            let db = self.db.clone();
            opening.spawn(async move { db.ping().await });
        }
        while let Some(joined) = opening.join_next().await {
            joined.map_err(|_| StoreError::Interrupted)??;
        }
        Ok(())
    }

    /// The lookup itself, on the caller's task.
    async fn read(&self, hash: SessionHash) -> Result<Option<SessionGrant>, StoreError> {
        let row: Option<(String, String, Option<String>, bool, bool)> =
            sqlx::query_as(ACTIVE_SESSION)
                .bind(hash.as_bytes().as_slice())
                .bind(SESSION_IDLE_LIFETIME.as_secs_f64())
                .bind(SESSION_ABSOLUTE_LIFETIME.as_secs_f64())
                .bind(SESSION_EXTEND_STEP.as_secs_f64())
                .fetch_optional(self.db.pool())
                .await?;
        let Some((session, user, tenant, platform_admin, due)) = row else {
            return Ok(None);
        };
        let session_id: SessionId = session
            .parse()
            .map_err(|_| StoreError::Inconsistent("session id"))?;
        let user: UserId = user
            .parse()
            .map_err(|_| StoreError::Inconsistent("user id"))?;
        let tenant = match tenant {
            Some(tenant) => {
                let tenant: TenantId = tenant
                    .parse()
                    .map_err(|_| StoreError::Inconsistent("tenant id"))?;
                self.membership(tenant, user).await?.map(|membership| {
                    (
                        TenantGrant {
                            id: tenant,
                            role: membership.role,
                        },
                        membership.suspended,
                    )
                })
            }
            None => None,
        };
        let (tenant, tenant_suspended) = match tenant {
            Some((grant, suspended)) => (Some(grant), suspended),
            None => (None, false),
        };
        // After the check's own queries, so the extension never takes a connection they need.
        if due {
            self.extend(session);
        }
        Ok(Some(SessionGrant {
            session: session_id,
            user,
            tenant,
            platform_admin,
            tenant_suspended,
        }))
    }

    /// Starts extending the session `session` (its ID) on its own task with a connection that
    /// is idle right now, unless [`EXTENSIONS_IN_FLIGHT`] are already running. Never waits and
    /// never opens a connection: this pool is sized for the checks and the keep-warm round (see
    /// [`WARM_CONNECTIONS`]), so with no idle connection the extension is skipped and the
    /// session's next request tries again. Logs a failure with its SQLSTATE only.
    fn extend(&self, session: String) {
        let Ok(permit) = Arc::clone(&self.extensions).try_acquire_owned() else {
            tracing::debug!("skipped a session extension: too many in flight");
            return;
        };
        let Some(mut connection) = self.db.pool().try_acquire() else {
            tracing::debug!("skipped a session extension: no idle connection");
            return;
        };
        tokio::spawn(async move {
            let outcome = tokio::time::timeout(
                WORK_LIMIT,
                sqlx::query(EXTEND_SESSION)
                    .bind(session)
                    .bind(SESSION_IDLE_LIFETIME.as_secs_f64())
                    .bind(SESSION_ABSOLUTE_LIFETIME.as_secs_f64())
                    .execute(&mut *connection),
            )
            .await;
            drop(connection);
            drop(permit);
            match outcome {
                Ok(Ok(_)) => {}
                Ok(Err(error)) => tracing::warn!(
                    error_kind = error_kind(&error),
                    "could not extend a session; its next request tries again"
                ),
                Err(_) => tracing::warn!(
                    limit_secs = WORK_LIMIT.as_secs(),
                    "extending a session did not finish in time; its next request tries again"
                ),
            }
        });
    }

    /// The user's role in `tenant` and whether `tenant` is suspended, or `None` when they are
    /// not a member (any more).
    async fn membership(
        &self,
        tenant: TenantId,
        user: UserId,
    ) -> Result<Option<Membership>, StoreError> {
        let mut tx = self.memberships.begin(tenant, MEMBERSHIP_PURPOSE).await?;
        let row: Option<MembershipRow> = sqlx::query_as(MEMBERSHIP)
            .bind(tenant.to_string())
            .bind(user.to_string())
            .fetch_optional(&mut *tx)
            .await?;
        tx.commit().await?;
        row.map(Membership::from_row).transpose()
    }
}

impl SessionStore for PgSessionStore {
    /// Runs the lookup on its own task, which goes on when the caller stops waiting (see
    /// [`PgSessionStore`]).
    async fn lookup(&self, hash: SessionHash) -> Result<Option<SessionGrant>, StoreError> {
        let permit = Arc::clone(&self.in_flight)
            .try_acquire_owned()
            .map_err(|_| StoreError::Busy)?;
        let store = self.clone();
        let lookup = tokio::spawn(async move {
            let grant = tokio::time::timeout(WORK_LIMIT, store.read(hash))
                .await
                .unwrap_or(Err(StoreError::TimedOut));
            drop(permit);
            grant
        });
        lookup.await.unwrap_or(Err(StoreError::Interrupted))
    }

    async fn ping(&self) -> Result<(), StoreError> {
        if self.warm.load(Ordering::Acquire) {
            Ok(())
        } else {
            Err(StoreError::NotWarm)
        }
    }
}

/// Readiness as the keep-warm task sets it: the flag [`SessionStore::ping`] reads, and one log
/// line each time it changes (INFO when ready, WARN with the error when not).
struct Readiness {
    warm: Arc<AtomicBool>,
    /// What was logged last; `None` before the first round.
    logged: Option<bool>,
}

impl Readiness {
    const fn new(warm: Arc<AtomicBool>) -> Self {
        Self { warm, logged: None }
    }

    fn ready(&mut self) {
        self.warm.store(true, Ordering::Release);
        if self.logged != Some(true) {
            tracing::info!(
                connections = WARM_CONNECTIONS,
                "the database connections are open"
            );
            self.logged = Some(true);
        }
    }

    fn not_ready(&mut self, error: &StoreError) {
        self.warm.store(false, Ordering::Release);
        if self.logged != Some(false) {
            tracing::warn!(
                error = %error,
                "cannot keep the database connections open; not ready"
            );
            self.logged = Some(false);
        }
    }
}

/// Runs [`BACKEND_PID`] on one connection for a keep-warm round and gives it back. Returns
/// whether it answered and is one the round had not confirmed yet. A connection that fails is
/// closed instead, and readiness fails until the round has confirmed enough others.
async fn probe(
    mut connection: PoolConnection<Postgres>,
    confirmed: &mut Vec<i32>,
    readiness: &mut Readiness,
) -> bool {
    match sqlx::query_scalar::<_, i32>(BACKEND_PID)
        .fetch_one(&mut *connection)
        .await
    {
        Ok(pid) => {
            // Give it back now rather than on drop: dropping returns it in a background task
            // (which pings it first), and until that task is done the connection is out of reach
            // of checks too, so taking the next one meanwhile would hold two. `return_to_pool`
            // runs that same drop handler in place (sqlx's own `PgListener` calls it this way);
            // it is hidden from sqlx's docs, so review it when upgrading sqlx.
            connection.return_to_pool().await;
            if confirmed.contains(&pid) {
                false
            } else {
                confirmed.push(pid);
                true
            }
        }
        Err(error) => {
            connection.close_on_drop();
            readiness.not_ready(&StoreError::from(error));
            false
        }
    }
}

/// A database error for the log: its SQLSTATE or kind, never the message, which may quote data.
pub(crate) fn error_kind(error: &sqlx::Error) -> String {
    match error {
        sqlx::Error::Database(database) => database
            .code()
            .map_or_else(|| "database".to_owned(), |code| format!("SQLSTATE {code}")),
        sqlx::Error::PoolTimedOut => "pool timed out".to_owned(),
        _ => "database client".to_owned(),
    }
}

/// One row of [`MEMBERSHIP`]: the role, and the tenant's state (`NULL` without a tenant row).
pub(crate) type MembershipRow = (String, Option<String>);

/// A user's membership of one tenant as [`MEMBERSHIP`] reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Membership {
    /// The user's role in the tenant.
    pub(crate) role: TenantRole,
    /// Whether the tenant is suspended or deleted: any state but `active`.
    pub(crate) suspended: bool,
}

impl Membership {
    /// Reads a [`MEMBERSHIP`] row. A role or state the schema does not allow is refused, never
    /// guessed: the check answers 503 (fail closed), as for any inconsistent row.
    pub(crate) fn from_row((role, state): MembershipRow) -> Result<Self, StoreError> {
        let suspended = match state.as_deref().map(TenantState::parse) {
            None => false,
            Some(Some(state)) => state != TenantState::Active,
            Some(None) => return Err(StoreError::Inconsistent("tenant state")),
        };
        Ok(Self {
            role: parse_role(&role)?,
            suspended,
        })
    }
}

/// The roles the schema allows. Anything else is refused rather than issued: an unknown role
/// would reach services as [`TenantRole::Unrecognized`].
pub(crate) fn parse_role(role: &str) -> Result<TenantRole, StoreError> {
    match role {
        "viewer" => Ok(TenantRole::Viewer),
        "editor" => Ok(TenantRole::Editor),
        "admin" => Ok(TenantRole::Admin),
        _ => Err(StoreError::Inconsistent("membership role")),
    }
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::*;

    const SECRET: [u8; 32] = [7; 32];

    #[test]
    fn a_membership_row_is_suspended_unless_its_tenant_is_active() {
        let row = |role: &str, state: Option<&str>| {
            Membership::from_row((role.to_owned(), state.map(str::to_owned)))
        };
        assert_eq!(
            row("editor", Some("active")).expect("active"),
            Membership {
                role: TenantRole::Editor,
                suspended: false
            }
        );
        for state in ["suspended", "deleted"] {
            assert_eq!(
                row("admin", Some(state)).expect(state),
                Membership {
                    role: TenantRole::Admin,
                    suspended: true
                }
            );
        }
        // No tenant row: never suspended (see MEMBERSHIP).
        assert!(!row("viewer", None).expect("no tenant row").suspended);
        // Values the schema forbids are refused, not guessed.
        for (role, state) in [
            ("viewer", Some("Suspended")),
            ("viewer", Some("")),
            ("owner", Some("active")),
        ] {
            assert!(
                matches!(row(role, state), Err(StoreError::Inconsistent(_))),
                "{role} {state:?}"
            );
        }
    }

    #[test]
    fn a_store_error_logs_the_tenant_db_message_once() {
        let failed = StoreError::Database(TenantDbError::Database(sqlx::Error::PoolTimedOut));
        let message = failed.to_string();
        assert_eq!(
            message,
            TenantDbError::Database(sqlx::Error::PoolTimedOut).to_string()
        );
        assert_eq!(message.matches("database error").count(), 1, "{message}");
        assert!(message.contains("pool timed out"), "{message}");

        for inner in [
            TenantDbError::NoTenant,
            TenantDbError::UnsafeSession(service_kit::postgres::UnsafeSession::BypassesRls),
        ] {
            let expected = inner.to_string();
            assert_eq!(StoreError::from(inner).to_string(), expected);
        }
    }

    fn cookie_headers(values: &[&[u8]]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for value in values {
            headers.append(
                COOKIE,
                HeaderValue::from_bytes(value).expect("header value"),
            );
        }
        headers
    }

    fn cookie(value: &str) -> String {
        format!("{SESSION_COOKIE}={value}")
    }

    #[test]
    fn a_secret_round_trips_through_its_cookie_value() {
        let secret = SessionSecret::from_bytes(SECRET);
        let value = secret.to_cookie_value();
        assert_eq!(value.len(), SESSION_COOKIE_LEN);
        assert_eq!(SessionSecret::parse(value.as_bytes()), Some(secret));
    }

    #[test]
    fn only_the_one_canonical_spelling_is_a_secret() {
        let value = SessionSecret::from_bytes(SECRET).to_cookie_value();
        let mut last_bit_set = value.clone().into_bytes();
        // 43 characters carry 258 bits; the last two must be zero.
        if let Some(last) = last_bit_set.last_mut() {
            *last = b'B';
        }
        for bad in [
            String::new(),
            format!("{value}="),
            value.get(..42).unwrap_or_default().to_owned(),
            format!("{value}A"),
            value.replace('B', "+"),
            format!("+{}", value.get(1..).unwrap_or_default()),
            format!("/{}", value.get(1..).unwrap_or_default()),
            format!("\"{}\"", value.get(..41).unwrap_or_default()),
            String::from_utf8(last_bit_set).expect("ascii"),
            " ".repeat(43),
        ] {
            assert_eq!(SessionSecret::parse(bad.as_bytes()), None, "{bad:?}");
        }
    }

    /// SHA-256 of 32 bytes of 0x07, computed independently:
    /// `printf '\x07%.0s' $(seq 32) | sha256sum`.
    #[test]
    fn the_hash_is_sha256_of_the_secret_bytes() {
        let hash = SessionSecret::from_bytes(SECRET).hash();
        let hex: String = hash
            .as_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        assert_eq!(
            hex,
            "4bb06f8e4e3a7715d201d573d0aa423762e55dabd61a2c02278fa56cc6d294e0"
        );
        assert_ne!(hash, SessionSecret::from_bytes([8; 32]).hash());
    }

    #[test]
    fn debug_output_never_shows_the_secret_or_its_hash() {
        let secret = SessionSecret::from_bytes(SECRET);
        assert_eq!(format!("{secret:?}"), "SessionSecret(<redacted>)");
        assert_eq!(format!("{:?}", secret.hash()), "SessionHash(<redacted>)");
    }

    #[test]
    fn the_session_cookie_is_found_among_other_cookies_and_headers() {
        let value = SessionSecret::from_bytes(SECRET).to_cookie_value();
        let expected = SessionSecret::from_bytes(SECRET);
        let cases: Vec<Vec<Vec<u8>>> = vec![
            vec![cookie(&value).into_bytes()],
            vec![format!("theme=dark; {}; lang=zh-TW", cookie(&value)).into_bytes()],
            vec![format!("theme=dark;{}", cookie(&value)).into_bytes()],
            // HTTP/2 may send each cookie in its own header.
            vec![b"theme=dark".to_vec(), cookie(&value).into_bytes()],
            // Another cookie with bytes outside visible ASCII does not matter.
            vec![
                b"other=caf\xe9; "
                    .iter()
                    .copied()
                    .chain(cookie(&value).into_bytes())
                    .collect(),
            ],
            // Similar names are other cookies.
            vec![
                format!(
                    "trustiot-session=x; __host-trustiot-session=y; {}",
                    cookie(&value)
                )
                .into_bytes(),
            ],
        ];
        for case in cases {
            let slices: Vec<&[u8]> = case.iter().map(Vec::as_slice).collect();
            assert_eq!(
                session_secret(&cookie_headers(&slices)).as_ref(),
                Ok(&expected),
                "{case:?}"
            );
        }
    }

    #[test]
    fn a_missing_repeated_or_malformed_session_cookie_is_refused() {
        let value = SessionSecret::from_bytes(SECRET).to_cookie_value();
        let other = SessionSecret::from_bytes([9; 32]).to_cookie_value();
        let both = format!("{}; {}", cookie(&value), cookie(&other));
        let cases: Vec<(Vec<String>, CookieError)> = vec![
            (vec![], CookieError::Missing),
            (vec!["theme=dark; sid=abc".to_owned()], CookieError::Missing),
            (vec![SESSION_COOKIE.to_owned()], CookieError::Missing),
            (vec![both], CookieError::Repeated),
            (vec![cookie(&value), cookie(&value)], CookieError::Repeated),
            (vec![cookie("")], CookieError::Malformed),
            (vec![cookie("valid-abc")], CookieError::Malformed),
            (
                vec![cookie(&format!("\"{value}\""))],
                CookieError::Malformed,
            ),
            (vec![cookie(&format!("{value}x"))], CookieError::Malformed),
        ];
        for (headers, expected) in cases {
            let slices: Vec<&[u8]> = headers.iter().map(String::as_bytes).collect();
            assert_eq!(
                session_secret(&cookie_headers(&slices)).err(),
                Some(expected),
                "{headers:?}"
            );
        }
    }

    #[test]
    fn only_the_three_schema_roles_are_issued() {
        assert_eq!(parse_role("viewer").ok(), Some(TenantRole::Viewer));
        assert_eq!(parse_role("editor").ok(), Some(TenantRole::Editor));
        assert_eq!(parse_role("admin").ok(), Some(TenantRole::Admin));
        for bad in ["", "Admin", "owner", "unrecognized"] {
            assert!(parse_role(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_session_cookie_is_set_and_cleared_with_the_same_attributes() {
        let secret = SessionSecret::from_bytes(SECRET);
        let set = set_cookie(&secret).expect("a header value");
        assert!(set.is_sensitive());
        let set = set.to_str().expect("ascii");
        assert_eq!(
            set,
            format!(
                "__Host-trustiot-session={}; Max-Age=604800; Path=/; Secure; HttpOnly; \
                 SameSite=Lax",
                secret.to_cookie_value()
            )
        );
        let cleared = clear_cookie();
        let cleared = cleared.to_str().expect("ascii");
        assert_eq!(
            cleared,
            format!("{SESSION_COOKIE}=; Max-Age=0; {COOKIE_ATTRIBUTES}")
        );
        // The browser stores what it set and sends it back as the session cookie.
        let (pair, _) = set.split_once(';').expect("attributes");
        assert_eq!(
            session_secret(&cookie_headers(&[pair.as_bytes()])).as_ref(),
            Ok(&secret)
        );
    }

    #[test]
    fn generated_secrets_differ() {
        let first = SessionSecret::generate().expect("randomness");
        let second = SessionSecret::generate().expect("randomness");
        assert_ne!(first, second);
        assert_ne!(first, SessionSecret::from_bytes([0; SESSION_SECRET_BYTES]));
    }

    /// The migration's trigger refuses what the service would never write: the same 7 days as
    /// [`SESSION_ABSOLUTE_LIFETIME`].
    #[test]
    fn the_absolute_lifetime_matches_the_migration() {
        assert_eq!(SESSION_ABSOLUTE_LIFETIME.as_secs(), 7 * 24 * 3600);
        let migration = include_str!("../migrations/postgres/0004_login.sql");
        assert!(
            migration.contains("NEW.expires_at > NEW.created_at + interval '7 days'"),
            "0004_login.sql no longer caps sessions at 7 days"
        );
        assert!(SESSION_IDLE_LIFETIME < SESSION_ABSOLUTE_LIFETIME);
        assert!(SESSION_EXTEND_STEP < SESSION_IDLE_LIFETIME);
    }

    #[test]
    fn cookie_errors_have_distinct_reasons() {
        let reasons = [
            CookieError::Missing.reason(),
            CookieError::Repeated.reason(),
            CookieError::Malformed.reason(),
        ];
        assert_eq!(reasons.len(), {
            let mut unique = reasons.to_vec();
            unique.sort_unstable();
            unique.dedup();
            unique.len()
        });
    }
}
