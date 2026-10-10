//! Error text that is safe to log.
//!
//! A Postgres error message can quote the values of a row (`invalid input syntax for type uuid:
//! "..."`, constraint details), and logs must not carry data (M2). The crate's errors therefore
//! print only the SQLSTATE or the kind of failure; the original error stays available through
//! `Error::source` for code that needs it.

use std::fmt;

/// Formats a `sqlx::Error` as its SQLSTATE or kind, without the message.
pub(crate) struct SafeSqlx<'a>(pub(crate) &'a sqlx::Error);

impl fmt::Display for SafeSqlx<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            sqlx::Error::Database(error) => match error.code() {
                Some(code) => write!(f, "SQLSTATE {code}"),
                None => f.write_str("database error without SQLSTATE"),
            },
            sqlx::Error::PoolTimedOut => f.write_str("connection pool timed out"),
            sqlx::Error::PoolClosed => f.write_str("connection pool closed"),
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
    fn messages_are_replaced_by_kinds() {
        let error = sqlx::Error::Protocol("value 'secret-42' rejected".to_owned());
        let text = SafeSqlx(&error).to_string();
        assert_eq!(text, "protocol error");
        assert!(!text.contains("secret-42"));
        assert_eq!(
            SafeSqlx(&sqlx::Error::PoolTimedOut).to_string(),
            "connection pool timed out"
        );
    }
}
