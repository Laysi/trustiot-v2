//! Server-side sessions (decision 13): the session cookie, its hash, and the store that turns a
//! cookie into the user, tenant and role a request acts for.
//!
//! The cookie carries only a 256-bit random secret. The database keeps its SHA-256, never the
//! secret, so a copy of the sessions table cannot be replayed as cookies, and there is no session
//! signing key that could be hard-coded or leak (docs/requirements/rebuild-checklist.md A1).
//! Everything about the session (user, tenant, expiry, revocation, role) is read from the
//! database on every check, so logout, revocation and role changes apply to the next request.

use std::fmt;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use axum::http::HeaderMap;
use axum::http::header::COOKIE;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use contract::{TenantGrant, TenantId, TenantRole, UserId};
use service_kit::identity::ServiceIdentity;
use service_kit::postgres::InsecureConnection;
use service_kit::shutdown::Shutdown;
use service_kit::tenant::{BackgroundTenantDb, TenantDb, TenantDbError};
use sha2::{Digest, Sha256};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

/// The session cookie. The `__Host-` prefix makes browsers accept it only when it was set by this
/// host over HTTPS with `Path=/` and no `Domain`, so a sibling subdomain cannot plant or overwrite
/// it. The login flow (Laysi/trustiot-v2#103) sets it with `HttpOnly`, `Secure` and `SameSite`.
pub const SESSION_COOKIE: &str = "__Host-trustiot-session";

/// Bytes of randomness in a session secret.
pub const SESSION_SECRET_BYTES: usize = 32;

/// Length of the cookie value: [`SESSION_SECRET_BYTES`] in base64url without padding.
pub const SESSION_COOKIE_LEN: usize = 43;

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
    /// Wraps raw secret bytes, for whoever creates sessions (in MS0 only the test tooling; the
    /// login flow of Laysi/trustiot-v2#103 later). The bytes must come from a CSPRNG.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; SESSION_SECRET_BYTES]) -> Self {
        Self(bytes)
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

/// What an active session grants: the user, the tenant the user acts in with their role there,
/// and whether the user is a platform administrator.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionGrant {
    /// The user the session belongs to.
    pub user: UserId,
    /// The session's tenant and the user's current role in it; `None` when the session has not
    /// picked a tenant or the user is no longer a member of it.
    pub tenant: Option<TenantGrant>,
    /// Whether the user is a platform administrator (A2).
    pub platform_admin: bool,
}

/// Why the store could not answer. The ext_authz check answers 503 for all of them, which the
/// ingress gateway turns into its own 503 (deploy/istio/README.md).
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The database failed or refused the session.
    #[error("database error: {0}")]
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
/// database clock.
const ACTIVE_SESSION: &str = "SELECT s.user_id::text, s.active_tenant_id::text, u.platform_admin
FROM identity.sessions AS s
JOIN identity.users AS u ON u.id = s.user_id
WHERE s.token_hash = $1 AND s.revoked_at IS NULL AND s.expires_at > now()";

/// The user's role in the transaction's tenant. Row-level security already limits the rows to
/// that tenant; the tenant condition is repeated so the query means the same without it.
const MEMBERSHIP_ROLE: &str = "SELECT tenant_role FROM identity.memberships
WHERE tenant_id = $1::uuid AND user_id = $2::uuid";

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
/// - [`PgSessionStore::keep_warm`] keeps [`WARM_CONNECTIONS`] open, and readiness passes only
///   while they are, so the gateway sends checks only to a replica that will not have to open a
///   connection for them.
#[derive(Clone, Debug)]
pub struct PgSessionStore {
    db: TenantDb,
    memberships: BackgroundTenantDb,
    in_flight: Arc<Semaphore>,
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
            warm: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Keeps [`WARM_CONNECTIONS`] open until the service stops, for
    /// [`service_kit::Context::spawn`]: every [`KEEP_WARM_INTERVAL`] it opens what is missing,
    /// or else runs `SELECT 1` on one connection (which also replaces a connection the database
    /// dropped). Readiness ([`SessionStore::ping`]) follows the last round. Logs when the pool
    /// becomes warm or stops being warm, never a secret.
    pub async fn keep_warm(self, shutdown: Shutdown) {
        let mut was_warm = None;
        loop {
            let outcome = tokio::select! {
                () = shutdown.stopping() => return,
                outcome = tokio::time::timeout(WORK_LIMIT, self.warm_up()) => {
                    outcome.unwrap_or(Err(StoreError::TimedOut))
                }
            };
            let warm = outcome.is_ok();
            self.warm.store(warm, Ordering::Release);
            match outcome {
                Ok(()) if was_warm != Some(true) => tracing::info!(
                    connections = WARM_CONNECTIONS,
                    "the database connections are open"
                ),
                Err(error) if was_warm != Some(false) => tracing::warn!(
                    error = %error,
                    "cannot keep the database connections open; not ready"
                ),
                _ => {}
            }
            was_warm = Some(warm);
            tokio::select! {
                () = shutdown.stopping() => return,
                () = tokio::time::sleep(KEEP_WARM_INTERVAL) => {}
            }
        }
    }

    /// One keep-warm round. When the pool holds fewer than [`WARM_CONNECTIONS`], takes that many
    /// at once, so the missing ones are opened (a new connection passes service-kit's session
    /// check, an idle one is pinged before it is handed out). Otherwise runs `SELECT 1` on one,
    /// which also replaces it when the database dropped it.
    async fn warm_up(&self) -> Result<(), StoreError> {
        let pool = self.db.pool();
        if pool.size() >= WARM_CONNECTIONS {
            return Ok(self.db.ping().await?);
        }
        let mut opening = JoinSet::new();
        for _ in 0..WARM_CONNECTIONS {
            let pool = pool.clone();
            opening.spawn(async move { pool.acquire().await });
        }
        // Hold every connection until all are open, so none is taken twice.
        let mut held = Vec::new();
        while let Some(joined) = opening.join_next().await {
            held.push(joined.map_err(|_| StoreError::Interrupted)??);
        }
        Ok(())
    }

    /// The lookup itself, on the caller's task.
    async fn read(&self, hash: SessionHash) -> Result<Option<SessionGrant>, StoreError> {
        let row: Option<(String, Option<String>, bool)> = sqlx::query_as(ACTIVE_SESSION)
            .bind(hash.as_bytes().as_slice())
            .fetch_optional(self.db.pool())
            .await?;
        let Some((user, tenant, platform_admin)) = row else {
            return Ok(None);
        };
        let user: UserId = user
            .parse()
            .map_err(|_| StoreError::Inconsistent("user id"))?;
        let tenant = match tenant {
            Some(tenant) => {
                let tenant: TenantId = tenant
                    .parse()
                    .map_err(|_| StoreError::Inconsistent("tenant id"))?;
                self.role(tenant, user)
                    .await?
                    .map(|role| TenantGrant { id: tenant, role })
            }
            None => None,
        };
        Ok(Some(SessionGrant {
            user,
            tenant,
            platform_admin,
        }))
    }

    /// The user's role in `tenant`, or `None` when they are not a member (any more).
    async fn role(&self, tenant: TenantId, user: UserId) -> Result<Option<TenantRole>, StoreError> {
        let mut tx = self.memberships.begin(tenant, MEMBERSHIP_PURPOSE).await?;
        let role: Option<String> = sqlx::query_scalar(MEMBERSHIP_ROLE)
            .bind(tenant.to_string())
            .bind(user.to_string())
            .fetch_optional(&mut *tx)
            .await?;
        tx.commit().await?;
        role.map(|role| parse_role(&role)).transpose()
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

/// The roles the schema allows. Anything else is refused rather than issued: an unknown role
/// would reach services as [`TenantRole::Unrecognized`].
fn parse_role(role: &str) -> Result<TenantRole, StoreError> {
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
