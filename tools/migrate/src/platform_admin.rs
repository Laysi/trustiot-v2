//! `migrate bootstrap-platform-admin`: the deployment's first platform administrator
//! (Laysi/trustiot-v2#582; checklist A2; docs/runbooks/platform-admin.md).
//!
//! An operator runs it once, in the cluster, with the migration Job's image and credentials
//! (the chart's `<release>-platform-admin-bootstrap` CronJob, which never runs on a schedule).
//! It reads one email address from stdin and, in one transaction as the identity service's owner
//! role:
//!
//! 1. finds the user with that address, if there is one;
//! 2. writes the only row of `identity.platform_admin_bootstrap`, the once-only marker
//!    (services/identity/migrations/postgres/0006_platform_admin_bootstrap.sql);
//! 3. makes that user a platform administrator, or creates one with the address;
//! 4. publishes `identity.platform_admin_granted` (payload: the user ID and `via: "bootstrap"`)
//!    to the identity outbox, which the identity service records in
//!    `identity.platform_event_log`, the platform's operation log.
//!
//! Once ever: when the marker exists, nothing changes and the command exits with
//! [`EXIT_ALREADY_CREATED`]. Two runs at the same time queue on the marker's primary key; the
//! second one finds the first one's row once it commits, and exits the same way. Deleting the
//! marker is the documented recovery step (the runbook), never something this command does.
//!
//! The address is personal data: it is read from stdin only (never an argument, a setting or a
//! file), compared and stored as the login flow normalizes it (services/identity/src/email.rs),
//! and never written to a log, an error, stdout or the event. Database errors of the statements
//! that carry it are reduced to the step, the SQLSTATE and the constraint name, since a
//! Postgres message or detail can quote the row.

use std::fmt;
use std::io::Read;

use contract::events::identity::{PlatformAdminGrant, PlatformAdminGranted, USER_AGGREGATE};
use contract::id::UserId;
use outbox::Aggregate;
use sqlx::{Postgres, Transaction};

use crate::Error;
use crate::config::BootstrapConfig;
use crate::identity_email::EmailAddress;
use crate::postgres::Session;

/// The exit code when the first platform administrator was created before: nothing changed.
pub const EXIT_ALREADY_CREATED: u8 = 3;

/// The identity service's owner role and schema (the registry's `identity` service).
pub const IDENTITY_OWNER: &str = "identity_owner";
pub const IDENTITY_SCHEMA: &str = "identity";

/// The most stdin may hold before the line ends: an address is at most 254 bytes, so anything
/// longer is not one, and the command never buffers unbounded input.
const MAX_INPUT: usize = 1024;

const MARKER_TABLE_EXISTS: &str =
    "SELECT to_regclass('identity.platform_admin_bootstrap') IS NOT NULL";
const FIND_USER: &str = "SELECT id::text FROM identity.users WHERE email = $1 FOR UPDATE";
/// Waits for a concurrent run's uncommitted marker, then finds it and inserts nothing.
const INSERT_MARKER: &str = "INSERT INTO identity.platform_admin_bootstrap (user_id) \
     VALUES ($1::uuid) ON CONFLICT (singleton) DO NOTHING RETURNING user_id::text";
const READ_MARKER: &str = "SELECT user_id::text, \
     to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') \
     FROM identity.platform_admin_bootstrap";
const GRANT_EXISTING: &str = "UPDATE identity.users SET platform_admin = true WHERE id = $1::uuid";
const CREATE_USER: &str =
    "INSERT INTO identity.users (id, email, platform_admin) VALUES ($1::uuid, $2, true)";

/// What the command did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// It made `user_id` the first platform administrator: an existing user with the address
    /// (`existing_user`), or a new one.
    Created {
        user_id: UserId,
        existing_user: bool,
    },
    /// The first platform administrator was created before (the marker's user and time, in
    /// UTC); nothing changed.
    AlreadyCreated { user_id: String, created_at: String },
}

/// Why stdin did not hold an address. Never contains the input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum InputError {
    #[error("stdin could not be read")]
    Read,
    #[error("stdin is longer than {MAX_INPUT} bytes before its first line ends")]
    TooLong,
    #[error("stdin is not UTF-8")]
    NotUtf8,
    #[error(
        "the first line of stdin is not an email address (ASCII dot-atom local part, @, host name)"
    )]
    NotAnAddress,
}

/// Reads the first line of `input` (up to `\n` or the end) and parses it as the login flow
/// does: trimmed, ASCII lower case, a plain address. Reads nothing after the first line, so a
/// pipe that stays open after the line (`kubectl attach`) does not hold the command up.
///
/// # Errors
///
/// [`InputError`]; its message never quotes the input.
pub fn read_email(mut input: impl Read) -> Result<EmailAddress, InputError> {
    let mut line = Vec::with_capacity(256);
    let mut byte = [0_u8; 1];
    loop {
        match input.read(&mut byte) {
            Ok(0) => break,
            Ok(_) if byte[0] == b'\n' => break,
            Ok(_) => {
                if line.len() == MAX_INPUT {
                    return Err(InputError::TooLong);
                }
                line.push(byte[0]);
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return Err(InputError::Read),
        }
    }
    let text = String::from_utf8(line).map_err(|_| InputError::NotUtf8)?;
    EmailAddress::parse(&text).ok_or(InputError::NotAnAddress)
}

/// Makes the user with `email` the deployment's first platform administrator, once ever; see
/// the module documentation.
///
/// # Errors
///
/// When Postgres is not reachable in time, the migration role does not qualify
/// ([`Session::check_migrator`]), the marker table is missing (the release's migration Job has
/// not run), or a statement fails. Nothing is committed then.
pub async fn bootstrap(config: &BootstrapConfig, email: &EmailAddress) -> Result<Outcome, Error> {
    let mut session = Session::connect_with(&config.postgres(), config.wait.databases).await?;
    session.check_migrator().await?;
    let mut tx = session
        .begin_as_owner(IDENTITY_OWNER, IDENTITY_SCHEMA)
        .await?;
    let outcome = grant(&mut tx, email).await;
    match outcome {
        Ok(outcome @ Outcome::Created { .. }) => {
            tx.commit()
                .await
                .map_err(|error| safe("commit the transaction", &error))?;
            Ok(outcome)
        }
        Ok(outcome @ Outcome::AlreadyCreated { .. }) => {
            // Nothing was written; the rollback ends the transaction.
            tx.rollback()
                .await
                .map_err(|error| safe("end the transaction", &error))?;
            Ok(outcome)
        }
        // Dropping the transaction rolls it back.
        Err(error) => Err(error),
    }
}

async fn grant(tx: &mut Transaction<'_, Postgres>, email: &EmailAddress) -> Result<Outcome, Error> {
    let marker_table: bool = sqlx::query_scalar(MARKER_TABLE_EXISTS)
        .fetch_one(&mut **tx)
        .await
        .map_err(|error| safe("look for identity.platform_admin_bootstrap", &error))?;
    if !marker_table {
        return Err(Error::PlatformAdmin(
            "identity.platform_admin_bootstrap does not exist: run this release's migration Job \
             first"
                .to_owned(),
        ));
    }
    let existing: Option<String> = sqlx::query_scalar(FIND_USER)
        .bind(email.as_str())
        .fetch_optional(&mut **tx)
        .await
        .map_err(|error| safe("look up the user", &error))?;
    let user_id = match &existing {
        Some(id) => id
            .parse::<UserId>()
            .map_err(|_| Error::PlatformAdmin("a user ID is not a UUID".to_owned()))?,
        None => UserId::generate(),
    };
    let inserted: Option<String> = sqlx::query_scalar(INSERT_MARKER)
        .bind(user_id.to_string())
        .fetch_optional(&mut **tx)
        .await
        .map_err(|error| safe("write the marker", &error))?;
    if inserted.is_none() {
        let (user_id, created_at): (String, String) = sqlx::query_as(READ_MARKER)
            .fetch_one(&mut **tx)
            .await
            .map_err(|error| safe("read the marker", &error))?;
        return Ok(Outcome::AlreadyCreated {
            user_id,
            created_at,
        });
    }
    if existing.is_some() {
        sqlx::query(GRANT_EXISTING)
            .bind(user_id.to_string())
            .execute(&mut **tx)
            .await
            .map_err(|error| safe("make the user a platform administrator", &error))?;
    } else {
        sqlx::query(CREATE_USER)
            .bind(user_id.to_string())
            .bind(email.as_str())
            .execute(&mut **tx)
            .await
            .map_err(|error| safe("create the user", &error))?;
    }
    let aggregate = Aggregate::new(USER_AGGREGATE, user_id.to_string())
        .map_err(|_| Error::PlatformAdmin("the user aggregate is invalid".to_owned()))?;
    let event = PlatformAdminGranted {
        user_id,
        via: PlatformAdminGrant::Bootstrap,
    };
    // A platform-wide event (no tenant) without an actor: an operator ran the command, and no
    // user or service of the platform stands behind it. `via` says how.
    outbox::publish(tx, &aggregate, &event, None)
        .await
        .map_err(|error| {
            Error::PlatformAdmin(format!("publish identity.platform_admin_granted: {error}"))
        })?;
    Ok(Outcome::Created {
        user_id,
        existing_user: existing.is_some(),
    })
}

/// A database error as the step, the SQLSTATE and the constraint: never the message or the
/// detail, which can quote the row (the address).
fn safe(step: &str, error: &sqlx::Error) -> Error {
    Error::PlatformAdmin(format!("{step}: {}", SafeError(error)))
}

struct SafeError<'a>(&'a sqlx::Error);

impl fmt::Display for SafeError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            sqlx::Error::Database(error) => {
                match error.code() {
                    Some(code) => write!(f, "SQLSTATE {code}")?,
                    None => f.write_str("database error without SQLSTATE")?,
                }
                match error.constraint() {
                    Some(constraint) => write!(f, " (constraint {constraint})"),
                    None => Ok(()),
                }
            }
            sqlx::Error::Io(_) => f.write_str("I/O error"),
            sqlx::Error::Tls(_) => f.write_str("TLS error"),
            sqlx::Error::Protocol(_) => f.write_str("protocol error"),
            sqlx::Error::RowNotFound => f.write_str("row not found"),
            sqlx::Error::ColumnDecode { .. } | sqlx::Error::Decode(_) => {
                f.write_str("column decode error")
            }
            _ => f.write_str("database client error"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_line_is_the_address() {
        let email = read_email(&b"  First.Admin@Example.COM \nignored@example.com\n"[..])
            .expect("an address");
        assert_eq!(email.as_str(), "first.admin@example.com");
        let email = read_email(&b"admin@example.com"[..]).expect("an address without a newline");
        assert_eq!(email.as_str(), "admin@example.com");
        let email = read_email(&b"admin@example.com\r\n"[..]).expect("a CRLF line");
        assert_eq!(email.as_str(), "admin@example.com");
    }

    #[test]
    fn anything_else_is_refused_without_quoting_it() {
        let long = format!("{}@example.com\n", "a".repeat(MAX_INPUT));
        for (input, expected) in [
            (&b""[..], InputError::NotAnAddress),
            (&b"\n"[..], InputError::NotAnAddress),
            (&b"not-an-address\n"[..], InputError::NotAnAddress),
            (&b"user name@example.com\n"[..], InputError::NotAnAddress),
            (&b"\xff@example.com\n"[..], InputError::NotUtf8),
            (long.as_bytes(), InputError::TooLong),
        ] {
            let error = read_email(input).expect_err("refused");
            assert_eq!(error, expected);
            assert!(!error.to_string().contains("example"), "{error}");
        }
    }

    #[test]
    fn database_errors_never_quote_the_row() {
        let error = sqlx::Error::Protocol("value 'admin@example.com' rejected".to_owned());
        let text = safe("create the user", &error).to_string();
        assert_eq!(
            text,
            "platform administrator: create the user: protocol error"
        );
    }

    #[test]
    fn the_owner_role_is_the_registry_s() {
        let identity = crate::registry::PRODUCT
            .services
            .iter()
            .find(|service| service.name == "identity")
            .expect("the identity service");
        assert_eq!(identity.owner_role(), IDENTITY_OWNER);
        assert_eq!(identity.schema(), IDENTITY_SCHEMA);
    }
}
