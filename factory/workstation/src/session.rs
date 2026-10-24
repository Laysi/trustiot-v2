//! The platform administrator's login (services/identity/README.md, "登入" and "登出").
//!
//! `login` runs the two steps of `POST /auth/login`: `send_code` mails an eight-digit code,
//! `verify_code` exchanges it for the session cookie `__Host-trustiot-session`. The cookie is
//! kept, with the origin that set it, in [`SESSION_FILE`] (mode 0600) under
//! `$XDG_RUNTIME_DIR/trustiot-workstation/` until `logout` or until the session ends (idle for
//! 12 hours, or 7 days after login). There is no fallback to a persistent directory.
//!
//! `send_code` is sent once and never again by itself: every attempt may mail a code and uses
//! the address's quota (one a minute, five an hour, twenty a day), and the answer is 202 whether
//! or not a code went out.

use std::fmt;
use std::path::PathBuf;

use http::{Method, StatusCode};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};
use zeroize::Zeroizing;

use crate::Error;
use crate::config::Origin;
use crate::files;
use crate::http::{ApiError, Call, Client};
use crate::secret::{SessionCookie, VerificationCode};

/// The session file's name in the session directory.
pub const SESSION_FILE: &str = "session";

/// The login endpoint, the only one the ingress forwards without a cookie (decision 27).
pub const LOGIN_PATH: &str = "/auth/login";

/// Revokes the session of the cookie sent.
pub const LOGOUT_PATH: &str = "/api/identity/v1/sessions/current";

/// Largest session file.
const MAX_SESSION_BYTES: usize = 4096;

/// Longest email address accepted (RFC 5321's path limit).
const MAX_EMAIL_LEN: usize = 254;

/// A session: the cookie and the origin it belongs to. The cookie is only ever sent there.
#[derive(Clone, PartialEq, Eq)]
pub struct Session {
    pub origin: Origin,
    pub cookie: SessionCookie,
}

impl fmt::Debug for Session {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Session")
            .field("origin", &self.origin)
            .field("cookie", &self.cookie)
            .finish()
    }
}

impl Session {
    /// The file's text: `origin=<origin>` and `cookie=<value>`, one per line.
    fn to_file(&self) -> Zeroizing<String> {
        let mut text = Zeroizing::new(String::with_capacity(
            self.origin.as_str().len() + self.cookie.expose().len() + 32,
        ));
        text.push_str("origin=");
        text.push_str(self.origin.as_str());
        text.push_str("\ncookie=");
        text.push_str(self.cookie.expose());
        text.push('\n');
        text
    }

    fn from_file(bytes: &[u8]) -> Option<Self> {
        let text = std::str::from_utf8(bytes).ok()?;
        let mut origin = None;
        let mut cookie = None;
        for line in text.lines().filter(|line| !line.is_empty()) {
            match line.split_once('=')? {
                ("origin", value) if origin.is_none() => origin = Some(Origin::parse(value).ok()?),
                ("cookie", value) if cookie.is_none() => {
                    cookie = Some(SessionCookie::parse(value)?)
                }
                _ => return None,
            }
        }
        Some(Self {
            origin: origin?,
            cookie: cookie?,
        })
    }
}

/// Where the session is kept.
#[derive(Clone, Debug)]
pub struct SessionStore(PathBuf);

impl SessionStore {
    /// The store in `directory` (`$XDG_RUNTIME_DIR/trustiot-workstation`).
    #[must_use]
    pub fn new(directory: PathBuf) -> Self {
        Self(directory)
    }

    /// The session file.
    #[must_use]
    pub fn path(&self) -> PathBuf {
        self.0.join(SESSION_FILE)
    }

    /// The directory, created (0700) when missing.
    ///
    /// # Errors
    ///
    /// When it cannot be created or is writable by others.
    pub fn prepare(&self) -> Result<(), Error> {
        files::ensure_directory(&self.0)
    }

    /// The stored session; `None` when there is none.
    ///
    /// # Errors
    ///
    /// When the file is wider than 0600, is not a regular file, or does not hold a session.
    pub fn load(&self) -> Result<Option<Session>, Error> {
        let path = self.path();
        let bytes = match files::read_secret(&path, MAX_SESSION_BYTES) {
            Ok(bytes) => bytes,
            Err(Error::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        Session::from_file(&bytes).map(Some).ok_or(Error::BadFile {
            path,
            reason: "does not hold a session: remove it and log in again",
        })
    }

    /// Stores `session`, replacing any previous one (mode 0600, written atomically).
    ///
    /// # Errors
    ///
    /// When the directory or the file cannot be written.
    pub fn save(&self, session: &Session) -> Result<(), Error> {
        self.prepare()?;
        let path = self.path();
        files::replace_secret(&path, session.to_file().as_bytes())?;
        info!(path = %path.display(), origin = %session.origin, "stored the session");
        Ok(())
    }

    /// Removes the session file; `false` when there was none.
    ///
    /// # Errors
    ///
    /// When it exists and cannot be removed.
    pub fn remove(&self) -> Result<bool, Error> {
        let removed = files::remove(&self.path())?;
        if removed {
            info!(path = %self.path().display(), "removed the session file");
        }
        Ok(removed)
    }
}

/// `--email`: trimmed, ASCII, one `@` with text on both sides, no white space or control
/// characters, at most 254 bytes. The identity service checks it again.
///
/// # Errors
///
/// A usage error otherwise (the address is not repeated back).
pub fn check_email(email: &str) -> Result<&str, Error> {
    let email = email.trim();
    let valid = email.len() <= MAX_EMAIL_LEN
        && email.bytes().all(|byte| byte.is_ascii_graphic())
        && email.split_once('@').is_some_and(|(local, domain)| {
            !local.is_empty() && !domain.is_empty() && !domain.contains('@')
        });
    if valid {
        Ok(email)
    } else {
        Err(Error::Usage("--email is not an email address".to_owned()))
    }
}

#[derive(Serialize)]
struct SendCodeBody<'a> {
    step: &'static str,
    email: &'a str,
}

#[derive(Serialize)]
struct VerifyCodeBody<'a> {
    step: &'static str,
    email: &'a str,
    code: &'a str,
}

/// The `send_code` answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
pub struct CodeSent {
    pub code_length: u32,
    pub expires_in_secs: u64,
    pub resend_after_secs: u64,
}

/// What `send_code` got back after the service accepted it (202).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Accepted {
    /// The answer as documented.
    Answer(CodeSent),
    /// 202, but the body is not a `send_code` answer. The service still accepted the request, so a
    /// code is on its way if the address belongs to an account: the caller goes on to ask for it
    /// rather than reporting a failure that would make the operator ask for a second email.
    Unreadable,
}

/// Why `send_code` failed.
#[derive(Debug, thiserror::Error)]
pub enum SendCodeError {
    /// No answer after the request may have reached the service: a code may have been mailed.
    #[error(
        "{0}. A code may or may not have been sent: check the mailbox first. If no code \
         arrives, wait at least 60 seconds and run login again (each address gets at most 5 \
         codes an hour and 20 a day); the tool never resends by itself"
    )]
    Unknown(crate::http::RequestError),
    /// Every other failure: nothing was mailed.
    #[error(transparent)]
    Failed(#[from] Error),
}

/// `send_code` for `email`, sent exactly once.
///
/// # Errors
///
/// [`SendCodeError::Unknown`] when the request may have arrived but got no answer, otherwise
/// [`SendCodeError::Failed`].
pub async fn send_code(client: &Client, email: &str) -> Result<Accepted, SendCodeError> {
    let mut body = Zeroizing::new(Vec::with_capacity(512));
    serde_json::to_writer(
        &mut *body,
        &SendCodeBody {
            step: "send_code",
            email,
        },
    )
    .map_err(|_| Error::Failed("cannot encode the request".to_owned()))?;
    let response = match client
        .send(Call {
            method: Method::POST,
            path: LOGIN_PATH,
            cookie: None,
            json: Some(body),
        })
        .await
    {
        Ok(response) => response,
        Err(error) if error.may_have_arrived() => return Err(SendCodeError::Unknown(error)),
        Err(error) => return Err(Error::from(error).into()),
    };
    if response.status != StatusCode::ACCEPTED {
        return Err(Error::from(ApiError::new(&Method::POST, LOGIN_PATH, &response)).into());
    }
    debug!("send_code was accepted");
    Ok(serde_json::from_slice(&response.body).map_or_else(
        |_| {
            warn!("POST {LOGIN_PATH} answered 202 with a body that is not a send_code answer");
            Accepted::Unreadable
        },
        Accepted::Answer,
    ))
}

/// Why `verify_code` failed.
#[derive(Debug, thiserror::Error)]
pub enum VerifyError {
    /// 401: the code is wrong, used up or expired, or no code is open for the address.
    #[error(
        "the code was not accepted: it is wrong, has expired (10 minutes) or was tried too \
         often (5 times)"
    )]
    Refused,
    #[error(transparent)]
    Failed(#[from] Error),
}

/// `verify_code` for `email` with `code`; the session cookie on success. `previous` is a session
/// of the same origin this workstation already holds: it is sent along, so the identity service
/// revokes it when it opens the new one.
///
/// # Errors
///
/// [`VerifyError::Refused`] on 401, [`VerifyError::Failed`] otherwise.
pub async fn verify_code(
    client: &Client,
    email: &str,
    code: &VerificationCode,
    previous: Option<&SessionCookie>,
) -> Result<SessionCookie, VerifyError> {
    let mut body = Zeroizing::new(Vec::with_capacity(512));
    serde_json::to_writer(
        &mut *body,
        &VerifyCodeBody {
            step: "verify_code",
            email,
            code: code.expose(),
        },
    )
    .map_err(|_| Error::Failed("cannot encode the request".to_owned()))?;
    let response = client
        .send(Call {
            method: Method::POST,
            path: LOGIN_PATH,
            cookie: previous,
            json: Some(body),
        })
        .await
        .map_err(Error::from)?;
    match response.status {
        StatusCode::OK => {}
        StatusCode::UNAUTHORIZED => return Err(VerifyError::Refused),
        _ => return Err(Error::from(ApiError::new(&Method::POST, LOGIN_PATH, &response)).into()),
    }
    let cookie = SessionCookie::from_set_cookies(&response.set_cookies).ok_or_else(|| {
        Error::Failed(format!(
            "POST {LOGIN_PATH} answered 200 without exactly one well-formed session cookie"
        ))
    })?;
    debug!("verify_code opened a session");
    Ok(cookie)
}

/// What `logout` did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoggedOut {
    /// The identity service revoked the session.
    Revoked,
    /// The identity service no longer knew the session (401 with the code `unauthenticated`):
    /// it had ended already.
    AlreadyEnded,
}

/// Revokes `session` (`DELETE /api/identity/v1/sessions/current`). The caller removes the
/// session file on success.
///
/// # Errors
///
/// When there is no answer or the answer is neither 204 nor the platform's own 401 (code
/// `unauthenticated`; a bare 401 may come from a proxy in front of it): the session may still
/// be valid, so the file is kept.
pub async fn logout(client: &Client, session: &Session) -> Result<LoggedOut, Error> {
    let response = client
        .send(Call {
            method: Method::DELETE,
            path: LOGOUT_PATH,
            cookie: Some(&session.cookie),
            json: None,
        })
        .await?;
    if response.status == StatusCode::NO_CONTENT {
        return Ok(LoggedOut::Revoked);
    }
    let error = ApiError::new(&Method::DELETE, LOGOUT_PATH, &response);
    if error.is_session_rejected() {
        Ok(LoggedOut::AlreadyEnded)
    } else {
        Err(error.into())
    }
}

/// The session for `origin` (or the stored session's own origin when `origin` is `None`).
///
/// # Errors
///
/// When there is no session, or it belongs to another origin than `origin`.
pub fn session_for(store: &SessionStore, origin: Option<&Origin>) -> Result<Session, Error> {
    let session = store.load()?.ok_or_else(|| {
        Error::Failed(
            "no platform administrator is logged in on this workstation: run login".to_owned(),
        )
    })?;
    if let Some(origin) = origin
        && *origin != session.origin
    {
        return Err(Error::Failed(format!(
            "the session belongs to {}, not {origin}: log in to {origin} first",
            session.origin
        )));
    }
    Ok(session)
}

/// Whether `error` is the platform rejecting the session: 401 with the code `unauthenticated`
/// (see [`ApiError::is_session_rejected`]). Even then the session may be live (the certs service
/// also answers so when it cannot check the internal token, during a key rotation for example),
/// so the caller revokes it with [`logout`] before it forgets it.
#[must_use]
pub fn is_unauthenticated(error: &Error) -> bool {
    matches!(error, Error::Api(api) if api.is_session_rejected())
}

#[cfg(test)]
mod tests {
    use super::*;

    const COOKIE: &str = "AbCdEfGhIjKlMnOpQrStUvWxYz0123456789-_abcde";

    #[test]
    fn session_files_round_trip_and_refuse_anything_else() {
        let session = Session {
            origin: Origin::parse("https://app.example").expect("an origin"),
            cookie: SessionCookie::parse(COOKIE).expect("a cookie"),
        };
        let text = session.to_file();
        assert_eq!(
            text.as_str(),
            format!("origin=https://app.example\ncookie={COOKIE}\n")
        );
        assert_eq!(Session::from_file(text.as_bytes()), Some(session.clone()));
        for wrong in [
            format!("cookie={COOKIE}\n"),
            "origin=https://app.example\n".to_owned(),
            format!("origin=http://app.example\ncookie={COOKIE}\n"),
            format!("origin=https://app.example\ncookie={COOKIE}\ncookie={COOKIE}\n"),
            format!("origin=https://app.example\ncookie={COOKIE}\nextra=1\n"),
        ] {
            assert!(Session::from_file(wrong.as_bytes()).is_none(), "{wrong}");
        }
        assert!(!format!("{session:?}").contains(COOKIE));
    }

    #[test]
    fn emails_are_checked_without_being_repeated() {
        assert_eq!(
            check_email(" admin@example.com ").expect("valid"),
            "admin@example.com"
        );
        for wrong in [
            "",
            "admin",
            "@example.com",
            "admin@",
            "a b@example.com",
            "a@b@c",
        ] {
            let error = check_email(wrong).expect_err(wrong);
            assert!(matches!(error, Error::Usage(_)));
        }
    }
}
