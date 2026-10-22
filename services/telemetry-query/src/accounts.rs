//! Each tenant's read-only ClickHouse account (decision 14; services/telemetry-query/README.md,
//! "租戶帳號").
//!
//! - The account is `trustiot_tenant_account_` plus the tenant ID's 32 hex digits, with the
//!   tenant role `trustiot_tenant_reader` as its only role (a default role), the settings
//!   profile `trustiot_tenant_reader` and `SQL_tenant_id` fixed to the tenant (`CONST`), as
//!   services/ingest/migrations/clickhouse/0004_access.sql describes. The role reads only the
//!   tenant view, whose row policy returns the rows of the account's `SQL_tenant_id`.
//! - Its password is derived, not stored: HMAC-SHA-256 of the tenant ID, keyed with the
//!   service's own ClickHouse password, which the migration Job generates. ClickHouse stores a
//!   salted SHA-256 of it, and the statements carry only that hash, never the password. A new
//!   password generation of the service's account changes every tenant password; the first
//!   query of each tenant then fails to log in and [`Accounts::ensure`] sets the new one.
//! - [`TenantCreatedHandler`] creates the account when `identity.tenant_created` arrives, logs
//!   in as it to check its tenant, role, `readonly` and query limits, and only then records the
//!   tenant in `telemetry_query.tenant_accounts`, which the API reads before it queries
//!   ClickHouse. The API checks the account the same way ([`Accounts::verify`]) before every
//!   query it runs as it, and refuses to serve one that differs.
//!
//! The service's own account (`0007_telemetry_query.sql`) may create and change accounts with
//! that name prefix and hand out the tenant role, and nothing else.

use std::sync::Arc;

use contract::TenantId;
use contract::events::identity::TenantCreated;
use hmac::{Hmac, KeyInit, Mac};
use outbox::{Delivery, Handler, HandlerError};
use sha2::{Digest, Sha256};
use sqlx::{Postgres, Transaction};
use zeroize::Zeroizing;

use crate::clickhouse::{ClickHouse, Credentials, QueryError};

/// The prefix of every tenant account's name; `0007_telemetry_query.sql` lets the service's
/// account create and change exactly the accounts named so.
pub const ACCOUNT_PREFIX: &str = "trustiot_tenant_account_";

/// The tenant role, the only role a tenant account gets.
pub const TENANT_ROLE: &str = "trustiot_tenant_reader";

/// What the HMAC of a tenant password covers before the tenant ID, so the key never derives
/// anything else with the same input.
const PASSWORD_CONTEXT: &[u8] = b"trustiot-v2 telemetry-query tenant account password v1\0";

/// Creates the account with everything a tenant account has; nothing when it exists. The
/// tenant ID cannot be a query parameter here (ClickHouse 26.8 takes none in a `SETTINGS`
/// value), so [`create_statement`] puts it in place of `TENANT_ID_LITERAL`, from a [`TenantId`]
/// only, whose text is 36 characters of hex digits and hyphens.
const CREATE_ACCOUNT: &str = "CREATE USER IF NOT EXISTS {user:Identifier}
IDENTIFIED WITH sha256_hash BY {hash:String} SALT {salt:String}
HOST ANY
DEFAULT ROLE trustiot_tenant_reader
SETTINGS PROFILE 'trustiot_tenant_reader', SQL_tenant_id = 'TENANT_ID_LITERAL' CONST";

/// Sets the account's password, nothing else.
const SET_PASSWORD: &str =
    "ALTER USER {user:Identifier} IDENTIFIED WITH sha256_hash BY {hash:String} SALT {salt:String}";

/// The query limits of the settings profile `trustiot_tenant_reader`
/// (services/ingest/migrations/clickhouse/0004_access.sql) that a tenant account must have in
/// force, as `getSetting` renders them. `readonly = 1` keeps the account from changing any of
/// them, but an administrator could weaken one on the account itself (a setting after the
/// profile in `ALTER USER ... SETTINGS`) and leave `readonly` as it is: [`Accounts::verify`]
/// then refuses the account. A unit test keeps this list equal to the profile.
pub const LIMITS: [(&str, &str); 7] = [
    ("max_execution_time", "30"),
    ("max_memory_usage", "536870912"),
    ("max_threads", "2"),
    ("max_concurrent_queries_for_user", "4"),
    ("max_rows_to_read", "1000000000"),
    ("max_result_rows", "1000000"),
    ("result_overflow_mode", "throw"),
];

/// What a tenant account sees of itself when it logs in: its tenant, whether its only active
/// role is the tenant role, `readonly`, then each of [`LIMITS`] in order. Reads no table, so
/// the quota does not count it: ClickHouse 26.8 counts only queries that read a table
/// (docs/test-reports/2026-10-11-59-verify.md, finding F3), and tests/tenant_accounts.rs checks
/// that each API call counts once against the account's quota.
const WHO_AM_I: &str = "SELECT getSetting('SQL_tenant_id'),
  currentRoles() = ['trustiot_tenant_reader'], getSetting('readonly'),
  getSetting('max_execution_time'), getSetting('max_memory_usage'), getSetting('max_threads'),
  getSetting('max_concurrent_queries_for_user'), getSetting('max_rows_to_read'),
  getSetting('max_result_rows'), getSetting('result_overflow_mode')
FORMAT TSV";

/// A tenant's account and its password.
pub struct TenantAccount {
    user: String,
    password: Zeroizing<String>,
}

impl TenantAccount {
    /// The account's name.
    #[must_use]
    pub fn user(&self) -> &str {
        &self.user
    }

    /// The account's credentials.
    #[must_use]
    pub fn credentials(&self) -> Credentials<'_> {
        Credentials {
            user: &self.user,
            password: &self.password,
        }
    }
}

/// The name of `tenant`'s account.
#[must_use]
pub fn account_name(tenant: TenantId) -> String {
    format!("{ACCOUNT_PREFIX}{}", tenant.as_uuid().simple())
}

/// `CREATE_ACCOUNT` for `tenant`.
fn create_statement(tenant: TenantId) -> String {
    let literal = tenant.as_uuid().hyphenated().to_string();
    // A UUID's text: the only thing that ever goes into the statement's text.
    assert!(
        literal.len() == 36
            && literal
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() || byte == b'-'),
        "a tenant ID renders as hex digits and hyphens"
    );
    CREATE_ACCOUNT.replace("TENANT_ID_LITERAL", &literal)
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut text, byte| {
        let _ = write!(text, "{byte:02x}");
        text
    })
}

/// The outcome of [`Accounts::ensure`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ensured {
    /// The account logged in as it should, without a change.
    Ready,
    /// Its password had to be set first (a new password generation, or someone changed it).
    PasswordSet,
}

/// Why a tenant account is not usable.
#[derive(Debug, thiserror::Error)]
pub enum AccountError {
    /// ClickHouse failed or could not be reached; trying again may help.
    #[error("ClickHouse failed while {doing}: {}", .source.kind())]
    ClickHouse {
        doing: &'static str,
        #[source]
        source: QueryError,
    },
    /// The account exists but is not what the service creates (another tenant, other roles,
    /// not read-only, or other query limits): someone changed it, and an administrator must drop
    /// it (the next `ensure` creates it again). Never repaired here: the service's account has
    /// `GRANTEES NONE` (0008_telemetry_query_grantees.sql), so it can neither grant nor revoke any role,
    /// the tenant role included, and a changed account needs an administrator's look anyway.
    /// The service's account can still `ALTER USER` every account under the name prefix,
    /// including `RENAME` and `ADD IDENTIFIED`; nothing audits that yet
    /// (Laysi/trustiot-v2#528).
    #[error("the tenant account {user} is not as the service creates it ({what})")]
    Tampered { user: String, what: &'static str },
    /// No randomness for the salt.
    #[error("no randomness for the password salt")]
    Random,
}

/// Creates and checks tenant accounts as the service's own ClickHouse account.
#[derive(Clone)]
pub struct Accounts {
    clickhouse: ClickHouse,
    user: Arc<str>,
    password: Arc<Zeroizing<String>>,
}

impl Accounts {
    /// Tenant accounts on `clickhouse`, created by the service's account `user` with
    /// `password`, which also keys every tenant password.
    #[must_use]
    pub fn new(clickhouse: ClickHouse, user: &str, password: &str) -> Self {
        Self {
            clickhouse,
            user: Arc::from(user),
            password: Arc::new(Zeroizing::new(password.to_owned())),
        }
    }

    /// The ClickHouse client.
    #[must_use]
    pub fn clickhouse(&self) -> &ClickHouse {
        &self.clickhouse
    }

    /// `tenant`'s account and password.
    #[must_use]
    pub fn account(&self, tenant: TenantId) -> TenantAccount {
        let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(self.password.as_bytes())
            .unwrap_or_else(|_| unreachable!("HMAC-SHA-256 takes keys of any length"));
        mac.update(PASSWORD_CONTEXT);
        mac.update(tenant.as_uuid().as_bytes());
        TenantAccount {
            user: account_name(tenant),
            password: Zeroizing::new(hex(&mac.finalize().into_bytes())),
        }
    }

    fn own(&self) -> Credentials<'_> {
        Credentials {
            user: &self.user,
            password: &self.password,
        }
    }

    /// The salted SHA-256 hash ClickHouse stores for `account`'s password, and the salt.
    fn hash(account: &TenantAccount) -> Result<(String, String), AccountError> {
        let mut salt = [0_u8; 16];
        getrandom::fill(&mut salt).map_err(|_| AccountError::Random)?;
        let salt = hex(&salt);
        let mut digest = Sha256::new();
        digest.update(account.password.as_bytes());
        digest.update(salt.as_bytes());
        Ok((hex(&digest.finalize()), salt))
    }

    /// Makes `tenant`'s account exist and log in with its derived password: creates it when it
    /// does not exist, sets the password when the account refuses it, and checks, logged in as
    /// the account, that it has the tenant's `SQL_tenant_id`, the tenant role as its only role,
    /// `readonly = 1` and the query limits of [`LIMITS`]. Idempotent; replicas may run it at the
    /// same time.
    ///
    /// # Errors
    ///
    /// [`AccountError::Tampered`] for an account that is not as created here,
    /// [`AccountError::ClickHouse`] when ClickHouse fails.
    pub async fn ensure(&self, tenant: TenantId) -> Result<Ensured, AccountError> {
        let account = self.account(tenant);
        let (hash, salt) = Self::hash(&account)?;
        let auth = [
            ("user", account.user()),
            ("hash", hash.as_str()),
            ("salt", salt.as_str()),
        ];
        // The role parameter enables the tenant role for this statement only, which granting it
        // through DEFAULT ROLE requires (0007_telemetry_query.sql).
        self.clickhouse
            .query(
                self.own(),
                &create_statement(tenant),
                &auth,
                Some(TENANT_ROLE),
            )
            .await
            .map_err(|source| AccountError::ClickHouse {
                doing: "creating the tenant account",
                source,
            })?;
        match self.check(tenant, &account).await {
            Err(AccountError::ClickHouse { source, .. }) if source.is_authentication() => {}
            other => return other.map(|()| Ensured::Ready),
        }
        self.clickhouse
            .query(self.own(), SET_PASSWORD, &auth, None)
            .await
            .map_err(|source| AccountError::ClickHouse {
                doing: "setting the tenant account's password",
                source,
            })?;
        self.check(tenant, &account).await?;
        Ok(Ensured::PasswordSet)
    }

    /// Checks `tenant`'s account before the API runs a query as it: logs in as it and checks
    /// what [`Accounts::ensure`] checks, without creating it. When ClickHouse refuses the
    /// derived password (a new password generation of the service's account, or someone changed
    /// it), runs [`Accounts::ensure`], which sets it again. Reads no table, so the account's quota
    /// does not count it.
    ///
    /// # Errors
    ///
    /// [`AccountError::Tampered`] for an account that is not as created here: the API must not
    /// run the query, because the account's limits or tenant may not hold.
    /// [`AccountError::ClickHouse`] when ClickHouse fails, including its concurrency limit.
    pub async fn verify(&self, tenant: TenantId) -> Result<Ensured, AccountError> {
        let account = self.account(tenant);
        match self.check(tenant, &account).await {
            Err(AccountError::ClickHouse { source, .. }) if source.is_authentication() => {
                self.ensure(tenant).await
            }
            other => other.map(|()| Ensured::Ready),
        }
    }

    /// Logs in as `account` and checks what it is.
    async fn check(&self, tenant: TenantId, account: &TenantAccount) -> Result<(), AccountError> {
        let answer = self
            .clickhouse
            .query(account.credentials(), WHO_AM_I, &[], None)
            .await;
        let tampered = |what| AccountError::Tampered {
            user: account.user().to_owned(),
            what,
        };
        let answer = match answer {
            Ok(answer) => answer,
            // An account without the setting: not one the service created.
            Err(QueryError::Failed {
                code: Some(crate::clickhouse::code::UNKNOWN_SETTING),
                ..
            }) => {
                return Err(tampered("no SQL_tenant_id"));
            }
            Err(source) => {
                return Err(AccountError::ClickHouse {
                    doing: "logging in as the tenant account",
                    source,
                });
            }
        };
        let fields: Vec<&str> = answer.trim_end_matches('\n').split('\t').collect();
        let [setting, roles, readonly, limits @ ..] = fields.as_slice() else {
            return Err(tampered("an unexpected answer"));
        };
        if limits.len() != LIMITS.len() {
            return Err(tampered("an unexpected answer"));
        }
        let expected_tenant = tenant.to_string();
        if *setting != expected_tenant {
            return Err(tampered("another tenant"));
        }
        if *roles != "1" {
            return Err(tampered("other roles"));
        }
        if *readonly != "1" {
            return Err(tampered("not read-only"));
        }
        if limits
            .iter()
            .zip(LIMITS)
            .any(|(actual, (_, expected))| *actual != expected)
        {
            return Err(tampered("other query limits"));
        }
        Ok(())
    }
}

/// Creates the tenant's account when `identity.tenant_created` arrives, then records the tenant;
/// ignores the identity service's other events.
#[derive(Clone)]
pub struct TenantCreatedHandler {
    accounts: Accounts,
}

impl TenantCreatedHandler {
    #[must_use]
    pub fn new(accounts: Accounts) -> Self {
        Self { accounts }
    }
}

/// The delivery's tenant's row; nothing when it exists. `$1` is the tenant ID as text.
const INSERT_TENANT: &str = "INSERT INTO telemetry_query.tenant_accounts (tenant_id)
VALUES ($1::uuid) ON CONFLICT (tenant_id) DO NOTHING";

impl Handler for TenantCreatedHandler {
    async fn handle(
        &self,
        tx: &mut Transaction<'static, Postgres>,
        delivery: &Delivery,
    ) -> Result<(), HandlerError> {
        if !delivery.envelope.is::<TenantCreated>() {
            return Ok(());
        }
        let TenantCreated {} = delivery.envelope.decode()?;
        let Some(tenant) = delivery.tenant() else {
            return Err(HandlerError::permanent(
                "identity.tenant_created without a tenant",
            ));
        };
        let ensured = match self.accounts.ensure(tenant).await {
            Ok(ensured) => ensured,
            Err(error @ AccountError::Tampered { .. }) => {
                return Err(HandlerError::permanent(
                    "the tenant's ClickHouse account was changed by someone else; an \
                     administrator must drop it, then replay the event",
                )
                .with_source(error));
            }
            Err(error) => {
                return Err(HandlerError::retry(
                    "could not create the tenant's ClickHouse account",
                )
                .with_source(error));
            }
        };
        let inserted = sqlx::query(INSERT_TENANT)
            .bind(tenant.to_string())
            .execute(&mut **tx)
            .await?;
        tracing::info!(
            tenant_id = %tenant,
            event_id = %delivery.envelope.id,
            recorded = inserted.rows_affected() == 1,
            password_set = ensured == Ensured::PasswordSet,
            "applied identity.tenant_created"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn accounts(password: &str) -> Accounts {
        let clickhouse = ClickHouse::new("http://127.0.0.1:1", None).expect("a client");
        Accounts::new(clickhouse, "trustiot_telemetry_query", password)
    }

    #[test]
    fn names_and_passwords_are_per_tenant_and_per_key() {
        let tenant: TenantId = "0192a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b"
            .parse()
            .expect("an ID");
        let other = TenantId::generate();
        let key = accounts(&"k".repeat(64));
        let account = key.account(tenant);
        assert_eq!(
            account.user(),
            "trustiot_tenant_account_0192a1b2c3d47e5f8a9b0c1d2e3f4a5b"
        );
        assert_eq!(account.password.len(), 64);
        assert!(
            account
                .password
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        );
        assert_eq!(*key.account(tenant).password, *account.password, "derived");
        assert_ne!(*key.account(other).password, *account.password);
        assert_ne!(
            *accounts(&"j".repeat(64)).account(tenant).password,
            *account.password,
            "another key, another password"
        );
        assert!(!account.password.contains(key.password.as_str()));
    }

    /// The tenant profile's settings in 0004_access.sql, as `name value` pairs without quotes;
    /// read from the migration Job's list of ClickHouse files, which applies them.
    fn profile_settings() -> Vec<(String, String)> {
        let sql = migrate::registry::PRODUCT
            .clickhouse
            .iter()
            .find(|migration| migration.name == "0004_access.sql")
            .expect("the access file")
            .sql;
        let (_, body) = sql
            .split_once("CREATE SETTINGS PROFILE IF NOT EXISTS trustiot_tenant_reader")
            .expect("the tenant profile");
        let (body, _) = body.split_once(';').expect("the end of the statement");
        body.lines()
            .filter_map(|line| {
                let line = line.trim();
                line.strip_suffix(" CONST")
                    .or_else(|| line.strip_suffix(" CONST,"))
            })
            .map(|setting| {
                let (name, value) = setting.split_once(" = ").expect("name = value");
                (name.to_owned(), value.trim_matches('\'').to_owned())
            })
            .collect()
    }

    #[test]
    fn the_checked_limits_are_the_tenant_profile() {
        let mut profile = profile_settings();
        assert!(
            profile.contains(&("readonly".to_owned(), "1".to_owned())),
            "{profile:?}"
        );
        profile.retain(|(name, _)| name != "readonly");
        let checked: Vec<(String, String)> = LIMITS
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect();
        assert_eq!(
            checked, profile,
            "LIMITS must list the profile's settings in its order"
        );
        for (name, _) in LIMITS {
            assert!(
                WHO_AM_I.contains(&format!("getSetting('{name}')")),
                "{name} is read"
            );
        }
        let read = WHO_AM_I.matches("getSetting(").count();
        assert_eq!(
            read,
            2 + LIMITS.len(),
            "SQL_tenant_id, readonly and the limits"
        );
        let order: Vec<usize> = LIMITS
            .iter()
            .map(|(name, _)| {
                WHO_AM_I
                    .find(&format!("getSetting('{name}')"))
                    .expect("read")
            })
            .collect();
        assert!(order.is_sorted(), "read in the order of LIMITS");
    }

    #[test]
    fn only_the_tenant_id_goes_into_the_create_statement() {
        let tenant = TenantId::generate();
        let statement = create_statement(tenant);
        assert!(statement.contains(&format!("SQL_tenant_id = '{tenant}' CONST")));
        assert_eq!(
            statement.replace(&tenant.to_string(), "TENANT_ID_LITERAL"),
            CREATE_ACCOUNT
        );
    }
}
