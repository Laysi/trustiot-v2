//! A user's preferences (Laysi/trustiot-v2#497; services/identity/README.md, "偏好設定"): the
//! interface's language, the language AI answers are written in, the time zone and the theme.
//! Every request reads and changes the preferences of its own user only, the one its internal
//! identity token names; no request names a user.
//!
//! | Request | Response |
//! | --- | --- |
//! | No valid internal identity token | 401 `unauthenticated` |
//! | Either, with a token another service exchanged (`act`) | 403 `permission_denied`: only the user's own requests read or change their preferences |
//! | `GET` | 200 [`Preferences`]; [`Preferences::default`] for a user who never changed one |
//! | `PATCH`, cross-site ([`crate::csrf::cross_site`], checked again after the cookie check) | 403 `permission_denied`, message key `security.cross_site_request` |
//! | `PATCH` with a `Content-Type` other than `application/json`, a body over [`MAX_BODY_BYTES`], or a body that is not a JSON object of the four fields, each a string (unknown fields and `null` included) | 400 `invalid_request` |
//! | `PATCH` with a value outside its allow list | 422 `validation_failed`, one field error per value: `/language` (`preferences.language_invalid`), `/ai_language` (`preferences.ai_language_invalid`), `/time_zone` (`preferences.time_zone_invalid`), `/theme` (`preferences.theme_invalid`); nothing changed |
//! | `PATCH` | 200 [`Preferences`] after the change; fields left out keep their value |
//! | `PATCH` for a user who no longer exists | 401 `unauthenticated` |
//! | The database failed | 503 `unavailable`; nothing changed |
//!
//! The allow lists: [`Language`] for both languages, [`Theme`], and for the time zone `auto` or
//! a name of the IANA Time Zone Database as it spells it (`Asia/Taipei`, not `asia/taipei`),
//! compiled into the service (`jiff-tzdb`) so every replica accepts the same names.

use std::sync::Arc;

use axum::Router;
use axum::extract::{FromRef, Request, State};
use axum::http::header::CACHE_CONTROL;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use contract::{ErrorBody, ErrorCode, FieldError, MessageKey, UserId};
use serde::{Deserialize, Deserializer, Serialize};
use service_kit::identity::{IdentityVerifier, UserIdentity};
use service_kit::tenant::{TenantDb, TenantDbError};
use utoipa::ToSchema;

use crate::current_session::{exchanged_refusal, json_object, own_write_refusal};
use crate::session::error_kind;
use crate::tenants::error;

/// The route, as the ingress gateway forwards it.
pub const PREFERENCES_PATH: &str = "/api/identity/v1/preferences";

/// The largest `PATCH` body: the four fields fit many times over.
pub const MAX_BODY_BYTES: usize = 1024;

/// The longest time zone name stored, as the database checks it; the longest IANA name is far
/// shorter.
pub const MAX_TIME_ZONE_LEN: usize = 64;

/// The time zone (and language) value that follows the browser, and the AI language value that
/// answers in the language of the question.
pub const AUTO: &str = "auto";

/// `/language` is not one of [`Language`].
pub const LANGUAGE_INVALID: MessageKey = MessageKey::new("preferences.language_invalid");

/// `/ai_language` is not one of [`Language`].
pub const AI_LANGUAGE_INVALID: MessageKey = MessageKey::new("preferences.ai_language_invalid");

/// `/time_zone` is neither `auto` nor an IANA time zone name.
pub const TIME_ZONE_INVALID: MessageKey = MessageKey::new("preferences.time_zone_invalid");

/// `/theme` is not one of [`Theme`].
pub const THEME_INVALID: MessageKey = MessageKey::new("preferences.theme_invalid");

/// The tzdb's placeholder for "time zone unknown": a name in the database, but no place's time,
/// and browsers refuse it.
const FACTORY_ZONE: &str = "Factory";

/// SQLSTATE `foreign_key_violation`: the user of the token no longer exists.
const FOREIGN_KEY_VIOLATION: &str = "23503";

/// The stored preferences of `$1`; no row means the defaults.
const SELECT_PREFERENCES: &str = "SELECT language, ai_language, time_zone, theme
FROM identity.preferences WHERE user_id = $1::uuid";

/// The user's row with the column defaults, unless it exists. The user must exist (foreign key).
const CREATE_PREFERENCES: &str = "INSERT INTO identity.preferences (user_id) VALUES ($1::uuid)
ON CONFLICT (user_id) DO NOTHING";

/// Changes the given settings of `$1`: `$2` to `$5` are the language, the AI language, the time
/// zone and the theme, NULL to keep one. Concurrent changes of one user take the row's lock in
/// turn, and each keeps what the other changed.
const UPDATE_PREFERENCES: &str = "UPDATE identity.preferences SET
    language = COALESCE($2, language),
    ai_language = COALESCE($3, ai_language),
    time_zone = COALESCE($4, time_zone),
    theme = COALESCE($5, theme)
WHERE user_id = $1::uuid
RETURNING language, ai_language, time_zone, theme";

/// A language the interface or AI answers may use.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
pub enum Language {
    /// The interface follows the browser's languages; AI answers are in the language of the
    /// question.
    #[serde(rename = "auto")]
    Auto,
    /// Traditional Chinese (Taiwan).
    #[serde(rename = "zh-TW")]
    ZhTw,
    /// English.
    #[serde(rename = "en")]
    En,
}

impl Language {
    /// Every allowed value.
    pub const ALL: [Self; 3] = [Self::Auto, Self::ZhTw, Self::En];

    /// The value as the API and the database spell it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => AUTO,
            Self::ZhTw => "zh-TW",
            Self::En => "en",
        }
    }

    /// The language spelled exactly `value`; `None` for anything else (`zh-tw` included).
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|language| language.as_str() == value)
    }
}

/// The interface's colors.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Theme {
    /// Follows the operating system's light or dark setting.
    System,
    /// Light.
    Light,
    /// Dark.
    Dark,
}

impl Theme {
    /// Every allowed value.
    pub const ALL: [Self; 3] = [Self::System, Self::Light, Self::Dark];

    /// The value as the API and the database spell it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Light => "light",
            Self::Dark => "dark",
        }
    }

    /// The theme spelled exactly `value`; `None` for anything else.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|theme| theme.as_str() == value)
    }
}

/// A time zone preference that passed the allow list: [`AUTO`], or a name of the IANA Time
/// Zone Database spelled as the database spells it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TimeZone(String);

impl TimeZone {
    /// Checks a requested time zone; `None` when it is not [`AUTO`] or a tzdb name. Case
    /// matters, so one zone is stored one way; links such as `Asia/Calcutta` are tzdb names too.
    /// `Factory` is refused: it names no place's time.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        let known = value == AUTO
            || (value.len() <= MAX_TIME_ZONE_LEN
                && value != FACTORY_ZONE
                && jiff_tzdb::available().any(|name| name == value));
        known.then(|| Self(value.to_owned()))
    }

    /// The value as stored.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A user's preferences.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Preferences {
    /// The interface's language; `auto` follows the browser.
    pub language: Language,
    /// The language AI answers are written in; `auto` answers in the language of the question.
    pub ai_language: Language,
    /// An IANA time zone name such as `Asia/Taipei`, or `auto` to follow the browser.
    #[schema(example = "Asia/Taipei")]
    pub time_zone: String,
    /// The interface's colors.
    pub theme: Theme,
}

impl Default for Preferences {
    /// What a user who never changed a preference has: everything follows the browser or the
    /// operating system, and AI answers in the language of the question. The migration's
    /// column defaults are the same (`0007_preferences_and_tenant_switch.sql`).
    fn default() -> Self {
        Self {
            language: Language::Auto,
            ai_language: Language::Auto,
            time_zone: AUTO.to_owned(),
            theme: Theme::System,
        }
    }
}

/// The body of `PATCH /api/identity/v1/preferences`: the settings to change. A field left out
/// keeps its value; `null` and unknown fields are refused.
#[derive(Debug, Default, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdatePreferences {
    /// The interface's language.
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = Language)]
    pub language: Option<String>,
    /// The language AI answers are written in.
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = Language)]
    pub ai_language: Option<String>,
    /// `auto` or an IANA time zone name, spelled as the tzdb spells it.
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = String, example = "Asia/Taipei")]
    pub time_zone: Option<String>,
    /// The interface's colors.
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = Theme)]
    pub theme: Option<String>,
}

/// A field that is there must be a string: `null` is refused rather than read as "keep".
fn present<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<String>, D::Error> {
    String::deserialize(deserializer).map(Some)
}

/// The settings a `PATCH` changes, each checked against its allow list.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Changes {
    /// The new interface language, if it changes.
    pub language: Option<Language>,
    /// The new AI answer language, if it changes.
    pub ai_language: Option<Language>,
    /// The new time zone, if it changes.
    pub time_zone: Option<TimeZone>,
    /// The new theme, if it changes.
    pub theme: Option<Theme>,
}

impl Changes {
    /// Checks every given value; the error names each one outside its allow list.
    ///
    /// # Errors
    ///
    /// A 422 `validation_failed` body with one field error per refused value.
    pub fn check(request: &UpdatePreferences) -> Result<Self, ErrorBody> {
        let mut refused = Vec::new();
        let language = checked(
            request.language.as_deref(),
            Language::parse,
            "/language",
            LANGUAGE_INVALID,
            &mut refused,
        );
        let ai_language = checked(
            request.ai_language.as_deref(),
            Language::parse,
            "/ai_language",
            AI_LANGUAGE_INVALID,
            &mut refused,
        );
        let time_zone = checked(
            request.time_zone.as_deref(),
            TimeZone::parse,
            "/time_zone",
            TIME_ZONE_INVALID,
            &mut refused,
        );
        let theme = checked(
            request.theme.as_deref(),
            Theme::parse,
            "/theme",
            THEME_INVALID,
            &mut refused,
        );
        if refused.is_empty() {
            Ok(Self {
                language,
                ai_language,
                time_zone,
                theme,
            })
        } else {
            Err(refused.into_iter().fold(
                ErrorBody::new(ErrorCode::ValidationFailed),
                ErrorBody::with_field_error,
            ))
        }
    }

    /// Whether the request changes nothing (`{}`).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// `parse(value)` when there is a value; a field error in `refused` when it does not parse.
fn checked<T>(
    value: Option<&str>,
    parse: impl Fn(&str) -> Option<T>,
    field: &str,
    key: MessageKey,
    refused: &mut Vec<FieldError>,
) -> Option<T> {
    let value = value?;
    let parsed = parse(value);
    if parsed.is_none() {
        refused.push(FieldError::new(field, key));
    }
    parsed
}

/// Why the preferences could not be read or changed.
#[derive(Debug, thiserror::Error)]
pub enum PreferencesError {
    /// The token's user no longer exists, so there is no one to keep a preference for.
    #[error("the user does not exist")]
    NoUser,
    /// The database failed.
    #[error(transparent)]
    Database(#[from] TenantDbError),
    /// A stored value is outside the allow lists, which the database's checks forbid.
    #[error("the stored preferences are inconsistent: {0}")]
    Inconsistent(&'static str),
}

impl From<sqlx::Error> for PreferencesError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(TenantDbError::Database(error))
    }
}

impl PreferencesError {
    /// A short description for the log: the SQLSTATE or the kind, never data.
    fn log_kind(&self) -> String {
        match self {
            Self::Database(TenantDbError::Database(error)) => error_kind(error),
            other => other.to_string(),
        }
    }
}

/// One stored row: language, AI language, time zone, theme.
type Row = (String, String, String, String);

/// The preferences in Postgres, read and written with the service role. They are not tenant
/// data, so the queries run outside a tenant transaction, always for the user of the request's
/// token.
#[derive(Clone, Debug)]
pub struct PreferencesStore {
    db: TenantDb,
}

impl PreferencesStore {
    /// A store on the service role's pool `db` (the tenant API's, shared with the current session
    /// API).
    #[must_use]
    pub fn new(db: TenantDb) -> Self {
        Self { db }
    }

    /// The preferences of `user`: [`Preferences::default`] when they never changed one.
    ///
    /// # Errors
    ///
    /// When the database fails or holds a value outside the allow lists.
    pub async fn read(&self, user: UserId) -> Result<Preferences, PreferencesError> {
        let row: Option<Row> = sqlx::query_as(SELECT_PREFERENCES)
            .bind(user.to_string())
            .fetch_optional(self.db.pool())
            .await?;
        row.map_or_else(|| Ok(Preferences::default()), from_row)
    }

    /// Applies `changes` to the preferences of `user` and returns them all; in one transaction
    /// that first gives the user a row with the defaults.
    ///
    /// # Errors
    ///
    /// [`PreferencesError::NoUser`] when the user no longer exists; [`PreferencesError::Inconsistent`]
    /// when the change would touch any row but the user's own; otherwise when the database fails.
    /// Nothing changed then.
    pub async fn update(
        &self,
        user: UserId,
        changes: &Changes,
    ) -> Result<Preferences, PreferencesError> {
        let user = user.to_string();
        let mut tx = self.db.pool().begin().await?;
        let created = sqlx::query(CREATE_PREFERENCES)
            .bind(&user)
            .execute(&mut *tx)
            .await;
        if let Err(sqlx::Error::Database(database)) = &created
            && database.code().as_deref() == Some(FOREIGN_KEY_VIOLATION)
        {
            return Err(PreferencesError::NoUser);
        }
        created?;
        let rows: Vec<Row> = sqlx::query_as(UPDATE_PREFERENCES)
            .bind(&user)
            .bind(changes.language.map(Language::as_str))
            .bind(changes.ai_language.map(Language::as_str))
            .bind(changes.time_zone.as_ref().map(TimeZone::as_str))
            .bind(changes.theme.map(Theme::as_str))
            .fetch_all(&mut *tx)
            .await?;
        // The user's own row and no other, or nothing is committed: the transaction rolls back
        // when it is dropped on the way out.
        let [row] = <[Row; 1]>::try_from(rows)
            .map_err(|_| PreferencesError::Inconsistent("rows changed"))?;
        let preferences = from_row(row)?;
        tx.commit().await?;
        Ok(preferences)
    }
}

/// A stored row as [`Preferences`].
fn from_row(
    (language, ai_language, time_zone, theme): Row,
) -> Result<Preferences, PreferencesError> {
    Ok(Preferences {
        language: Language::parse(&language).ok_or(PreferencesError::Inconsistent("language"))?,
        ai_language: Language::parse(&ai_language)
            .ok_or(PreferencesError::Inconsistent("ai_language"))?,
        time_zone,
        theme: Theme::parse(&theme).ok_or(PreferencesError::Inconsistent("theme"))?,
    })
}

/// The state of the preferences routes.
#[derive(Clone)]
pub struct PreferencesRoutes {
    verifier: IdentityVerifier,
    store: Arc<PreferencesStore>,
}

impl FromRef<PreferencesRoutes> for IdentityVerifier {
    fn from_ref(state: &PreferencesRoutes) -> Self {
        state.verifier.clone()
    }
}

/// The preferences routes: `GET` and `PATCH` [`PREFERENCES_PATH`].
pub fn routes(verifier: IdentityVerifier, store: PreferencesStore) -> Router {
    Router::new()
        .route(
            PREFERENCES_PATH,
            get(get_preferences).patch(update_preferences),
        )
        .with_state(PreferencesRoutes {
            verifier,
            store: Arc::new(store),
        })
}

/// 200 with `preferences`, never cached.
fn answer(preferences: &Preferences) -> Response {
    ([(CACHE_CONTROL, "no-store")], axum::Json(preferences)).into_response()
}

/// Reads the user's own preferences.
#[utoipa::path(
    get,
    path = "/api/identity/v1/preferences",
    tag = "identity",
    operation_id = "getPreferences",
    responses(
        (status = 200, description = "The user's preferences; the defaults (`auto`, `auto`, `auto`, `system`) for a user who never changed one.", body = Preferences),
        (status = 401, description = "No valid session (`unauthenticated`).", body = ErrorBody),
        (status = 403, description = "A token another service exchanged for a call on the user's behalf (`permission_denied`): only the user's own requests read their preferences.", body = ErrorBody),
        (status = 503, description = "The database failed (`unavailable`).", body = ErrorBody),
    )
)]
async fn get_preferences(State(state): State<PreferencesRoutes>, user: UserIdentity) -> Response {
    if let Some(refusal) = exchanged_refusal(&user, "refused to read preferences") {
        return refusal;
    }
    match state.store.read(user.user_id()).await {
        Ok(preferences) => answer(&preferences),
        Err(failure) => {
            tracing::error!(
                user_id = %user.user_id(),
                error_kind = failure.log_kind(),
                "could not read preferences"
            );
            error(ErrorBody::new(ErrorCode::Unavailable))
        }
    }
}

/// Changes the user's own preferences: the fields given, each from its allow list.
#[utoipa::path(
    patch,
    path = "/api/identity/v1/preferences",
    tag = "identity",
    operation_id = "updatePreferences",
    request_body = UpdatePreferences,
    responses(
        (status = 200, description = "The preferences after the change; fields left out kept their value.", body = Preferences),
        (status = 400, description = "The `Content-Type` is not `application/json`, the body is over 1024 bytes, or it is not a JSON object of the four fields with string values; unknown fields and `null` are refused (`invalid_request`).", body = ErrorBody),
        (status = 401, description = "No valid session, or the user no longer exists (`unauthenticated`).", body = ErrorBody),
        (status = 403, description = "A cross-site request (`permission_denied`, message key `security.cross_site_request`), or a token another service exchanged for a call on the user's behalf (`permission_denied`); nothing changed.", body = ErrorBody),
        (status = 422, description = "A value outside its allow list (`validation_failed`; `/language` `preferences.language_invalid`, `/ai_language` `preferences.ai_language_invalid`, `/time_zone` `preferences.time_zone_invalid`, `/theme` `preferences.theme_invalid`); nothing changed.", body = ErrorBody),
        (status = 503, description = "The database failed; nothing changed (`unavailable`).", body = ErrorBody),
    )
)]
async fn update_preferences(
    State(state): State<PreferencesRoutes>,
    user: UserIdentity,
    request: Request,
) -> Response {
    let (parts, body) = request.into_parts();
    if let Some(refusal) = own_write_refusal(&parts, &user, "refused to change preferences") {
        return refusal;
    }
    let Ok(body) = axum::body::to_bytes(body, MAX_BODY_BYTES).await else {
        return error(ErrorBody::new(ErrorCode::InvalidRequest));
    };
    let Some(request) = json_object::<UpdatePreferences>(&body) else {
        return error(ErrorBody::new(ErrorCode::InvalidRequest));
    };
    let changes = match Changes::check(&request) {
        Ok(changes) => changes,
        Err(refused) => return error(refused),
    };
    let outcome = if changes.is_empty() {
        state.store.read(user.user_id()).await
    } else {
        state.store.update(user.user_id(), &changes).await
    };
    match outcome {
        Ok(preferences) => {
            if !changes.is_empty() {
                // Which settings changed, not their values.
                tracing::info!(
                    user_id = %user.user_id(),
                    language = changes.language.is_some(),
                    ai_language = changes.ai_language.is_some(),
                    time_zone = changes.time_zone.is_some(),
                    theme = changes.theme.is_some(),
                    "changed preferences"
                );
            }
            answer(&preferences)
        }
        Err(PreferencesError::NoUser) => {
            tracing::info!(
                user_id = %user.user_id(),
                reason = "no_user",
                "refused to change preferences"
            );
            error(ErrorBody::new(ErrorCode::Unauthenticated))
        }
        Err(failure) => {
            tracing::error!(
                user_id = %user.user_id(),
                error_kind = failure.log_kind(),
                "could not change preferences"
            );
            error(ErrorBody::new(ErrorCode::Unavailable))
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// The migration that creates the table, whose defaults and checks must match this file.
    const MIGRATION: &str =
        include_str!("../migrations/postgres/0007_preferences_and_tenant_switch.sql");

    /// The pattern the migration's check holds every stored time zone to.
    const STORED_PATTERN: &str = "time_zone ~ '^[A-Za-z0-9_+-]+(/[A-Za-z0-9_+-]+)*$'";

    /// What [`STORED_PATTERN`] accepts: segments of ASCII letters, digits, `_`, `+` and `-`,
    /// separated by single slashes.
    fn fits_the_stored_pattern(name: &str) -> bool {
        name.split('/').all(|segment| {
            !segment.is_empty()
                && segment
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"_+-".contains(&byte))
        })
    }

    /// The body as the handler reads it.
    fn update(body: serde_json::Value) -> Option<UpdatePreferences> {
        json_object(body.to_string().as_bytes())
    }

    #[test]
    fn languages_and_themes_are_spelled_exactly() {
        assert_eq!(Language::parse("auto"), Some(Language::Auto));
        assert_eq!(Language::parse("zh-TW"), Some(Language::ZhTw));
        assert_eq!(Language::parse("en"), Some(Language::En));
        for refused in [
            "",
            "zh-tw",
            "ZH-TW",
            "zh_TW",
            "zh",
            "zh-Hant-TW",
            "en-US",
            "EN",
            "fr",
            "ja",
            " en",
            "en ",
            "Auto",
        ] {
            assert_eq!(Language::parse(refused), None, "{refused:?}");
        }
        assert_eq!(Theme::parse("system"), Some(Theme::System));
        assert_eq!(Theme::parse("light"), Some(Theme::Light));
        assert_eq!(Theme::parse("dark"), Some(Theme::Dark));
        for refused in ["", "Dark", "DARK", "blue", "auto", "high-contrast", " dark"] {
            assert_eq!(Theme::parse(refused), None, "{refused:?}");
        }
        // The API spells them as the database does.
        for language in Language::ALL {
            assert_eq!(json!(language), json!(language.as_str()));
        }
        for theme in Theme::ALL {
            assert_eq!(json!(theme), json!(theme.as_str()));
        }
    }

    #[test]
    fn time_zones_are_auto_or_tzdb_names_as_the_tzdb_spells_them() {
        for allowed in [
            "auto",
            "Asia/Taipei",
            "UTC",
            "Etc/GMT+8",
            "Etc/GMT-14",
            "America/Argentina/Buenos_Aires",
            "America/Port-au-Prince",
            // A backward-compatible link is a tzdb name too.
            "Asia/Calcutta",
        ] {
            assert_eq!(
                TimeZone::parse(allowed).as_ref().map(TimeZone::as_str),
                Some(allowed),
                "{allowed:?}"
            );
        }
        let too_long = format!("Asia/{}", "x".repeat(MAX_TIME_ZONE_LEN));
        for refused in [
            "",
            "AUTO",
            "asia/taipei",
            "ASIA/TAIPEI",
            "Asia/Taipei ",
            " Asia/Taipei",
            "Asia//Taipei",
            "/Asia/Taipei",
            "Asia/Taipei/",
            "Asia/Taipei\0",
            "Mars/Olympus_Mons",
            "../etc/passwd",
            "posixrules/../../etc/passwd",
            "+08:00",
            "GMT+8",
            "Factory",
            too_long.as_str(),
        ] {
            assert_eq!(TimeZone::parse(refused), None, "{refused:?}");
        }
    }

    /// Every name the service accepts is one the database's check accepts, so a valid request
    /// never fails as a database error; the check in the migration is the one tested here.
    #[test]
    fn every_accepted_time_zone_fits_the_database_check() {
        assert!(MIGRATION.contains(STORED_PATTERN), "{STORED_PATTERN}");
        assert!(MIGRATION.contains(&format!(
            "octet_length(time_zone) BETWEEN 1 AND {MAX_TIME_ZONE_LEN}"
        )));
        let mut names = 0;
        for name in jiff_tzdb::available() {
            let Some(zone) = TimeZone::parse(name) else {
                assert_eq!(name, FACTORY_ZONE, "only Factory is refused");
                continue;
            };
            names += 1;
            assert!(
                zone.as_str().len() <= MAX_TIME_ZONE_LEN && fits_the_stored_pattern(zone.as_str()),
                "{name:?} does not fit the database's check"
            );
        }
        // The tzdb has some 600 names; an empty list would make the check above vacuous.
        assert!(names > 400, "{names} time zone names");
        assert!(fits_the_stored_pattern("Asia/Taipei"));
        for outside in ["", "Asia//Taipei", "../etc", "Asia/Taipei ", "Asia/Taipei/"] {
            assert!(!fits_the_stored_pattern(outside), "{outside:?}");
        }
    }

    /// The service's defaults and allow lists are the migration's column defaults and checks.
    #[test]
    fn the_defaults_and_allow_lists_match_the_migration() {
        let defaults = Preferences::default();
        for (column, value) in [
            ("language", defaults.language.as_str()),
            ("ai_language", defaults.ai_language.as_str()),
            ("time_zone", defaults.time_zone.as_str()),
            ("theme", defaults.theme.as_str()),
        ] {
            let declared = format!("{column} text NOT NULL DEFAULT '{value}'");
            assert!(MIGRATION.contains(&declared), "{declared}");
        }
        let quoted = |values: Vec<&str>| {
            values
                .iter()
                .map(|value| format!("'{value}'"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let languages = quoted(Language::ALL.map(Language::as_str).to_vec());
        for column in ["language", "ai_language"] {
            let check = format!("CHECK ({column} IN ({languages}))");
            assert!(MIGRATION.contains(&check), "{check}");
        }
        let themes = quoted(Theme::ALL.map(Theme::as_str).to_vec());
        let check = format!("CHECK (theme IN ({themes}))");
        assert!(MIGRATION.contains(&check), "{check}");
        assert!(MIGRATION.contains(&format!("time_zone = '{AUTO}'")));
        assert_eq!(
            serde_json::to_value(&defaults).expect("serialize"),
            json!({"language": "auto", "ai_language": "auto", "time_zone": "auto", "theme": "system"})
        );
    }

    #[test]
    fn a_change_names_strings_of_the_four_fields_only() {
        let empty = update(json!({})).expect("an empty change");
        assert_eq!(Changes::check(&empty).expect("valid"), Changes::default());
        assert!(Changes::check(&empty).expect("valid").is_empty());
        let partial = update(json!({"theme": "dark"})).expect("one field");
        assert_eq!(
            Changes::check(&partial).expect("valid"),
            Changes {
                theme: Some(Theme::Dark),
                ..Changes::default()
            }
        );
        for refused in [
            json!({"language": null}),
            json!({"time_zone": null}),
            json!({"theme": 1}),
            json!({"language": ["en"]}),
            json!({"ai_language": {"value": "en"}}),
            json!({"user_id": "01928f6e-7a3c-7d4e-8f00-0123456789ab"}),
            json!({"language": "en", "locale": "en"}),
            // serde would read an array as the fields in order.
            json!([]),
            json!(["en", "en", "UTC", "dark"]),
            json!("en"),
            json!(null),
        ] {
            assert!(update(refused.clone()).is_none(), "{refused}");
        }
        let twice = br#"{"theme": "dark", "theme": "light"}"#;
        assert!(json_object::<UpdatePreferences>(twice).is_none());
    }

    #[test]
    fn every_value_outside_its_allow_list_is_named() {
        let request = update(json!({
            "language": "fr",
            "ai_language": "zh-tw",
            "time_zone": "Mars/Olympus_Mons",
            "theme": "blue",
        }))
        .expect("four strings");
        let refused = Changes::check(&request).expect_err("all four refused");
        assert_eq!(
            serde_json::to_value(&refused).expect("serialize"),
            json!({
                "code": "validation_failed",
                "message_key": "error.validation_failed",
                "field_errors": [
                    {"field": "/language", "message_key": "preferences.language_invalid"},
                    {"field": "/ai_language", "message_key": "preferences.ai_language_invalid"},
                    {"field": "/time_zone", "message_key": "preferences.time_zone_invalid"},
                    {"field": "/theme", "message_key": "preferences.theme_invalid"},
                ]
            })
        );
        // One bad value refuses the whole change, the valid ones included.
        let mixed = update(json!({"language": "en", "theme": "Dark"})).expect("two strings");
        let refused = Changes::check(&mixed).expect_err("the theme is refused");
        assert_eq!(
            serde_json::to_value(&refused).expect("serialize")["field_errors"],
            json!([{"field": "/theme", "message_key": "preferences.theme_invalid"}])
        );
        let valid = update(json!({
            "language": "zh-TW",
            "ai_language": "en",
            "time_zone": "Asia/Taipei",
            "theme": "light",
        }))
        .expect("four strings");
        assert_eq!(
            Changes::check(&valid).expect("valid"),
            Changes {
                language: Some(Language::ZhTw),
                ai_language: Some(Language::En),
                time_zone: TimeZone::parse("Asia/Taipei"),
                theme: Some(Theme::Light),
            }
        );
    }

    #[test]
    fn stored_rows_outside_the_allow_lists_are_inconsistent() {
        let row = |language: &str, theme: &str| {
            (
                language.to_owned(),
                "auto".to_owned(),
                "Asia/Taipei".to_owned(),
                theme.to_owned(),
            )
        };
        assert_eq!(
            from_row(row("en", "dark")).expect("valid"),
            Preferences {
                language: Language::En,
                ai_language: Language::Auto,
                time_zone: "Asia/Taipei".to_owned(),
                theme: Theme::Dark,
            }
        );
        assert!(matches!(
            from_row(row("fr", "dark")),
            Err(PreferencesError::Inconsistent("language"))
        ));
        assert!(matches!(
            from_row(row("en", "blue")),
            Err(PreferencesError::Inconsistent("theme"))
        ));
    }
}
