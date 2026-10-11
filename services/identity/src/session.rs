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
use std::time::Duration;

use axum::http::HeaderMap;
use axum::http::header::COOKIE;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use contract::{TenantGrant, TenantId, TenantRole, UserId};
use service_kit::identity::ServiceIdentity;
use service_kit::postgres::InsecureConnection;
use service_kit::tenant::{BackgroundTenantDb, TenantDb, TenantDbError};
use sha2::{Digest, Sha256};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

/// The session cookie. The `__Host-` prefix makes browsers accept it only when it was set by this
/// host over HTTPS with `Path=/` and no `Domain`, so a sibling subdomain cannot plant or overwrite
/// it. The login flow (Laysi/trustiot-v2#103) sets it with `HttpOnly`, `Secure` and `SameSite`.
pub const SESSION_COOKIE: &str = "__Host-trustiot-session";

/// Bytes of randomness in a session secret.
pub const SESSION_SECRET_BYTES: usize = 32;

/// Length of the cookie value: [`SESSION_SECRET_BYTES`] in base64url without padding.
pub const SESSION_COOKIE_LEN: usize = 43;

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

    /// Readiness: fails while the store cannot answer.
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
#[derive(Clone, Debug)]
pub struct PgSessionStore {
    db: TenantDb,
    memberships: BackgroundTenantDb,
}

impl PgSessionStore {
    /// A lazily connecting store. `acquire_timeout` should stay below the ext_authz deadline, so
    /// a saturated pool fails the check rather than outlives it.
    ///
    /// # Errors
    ///
    /// When `options` would not verify the server.
    pub fn connect_lazy(
        options: PgConnectOptions,
        max_connections: u32,
        acquire_timeout: Duration,
    ) -> Result<Self, InsecureConnection> {
        let pool = PgPoolOptions::new()
            .max_connections(max_connections)
            .acquire_timeout(acquire_timeout);
        let actor = ServiceIdentity::new(contract::ServiceName::Identity);
        let (db, memberships) = TenantDb::connect_lazy_with_background(options, pool, actor)?;
        Ok(Self { db, memberships })
    }
}

impl SessionStore for PgSessionStore {
    async fn lookup(&self, hash: SessionHash) -> Result<Option<SessionGrant>, StoreError> {
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

    async fn ping(&self) -> Result<(), StoreError> {
        Ok(self.db.ping().await?)
    }
}

impl PgSessionStore {
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
