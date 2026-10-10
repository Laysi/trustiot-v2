//! Service configuration: environment variables and an optional TOML file, checked at startup.
//!
//! Every key has a dotted path such as `http.listen_addr`. A key is set by the environment
//! variable `<PREFIX>__HTTP__LISTEN_ADDR` (path segments upper-cased and joined by `__`) or in the
//! TOML file named by `<PREFIX>_CONFIG_FILE`; the environment wins. An empty variable counts as
//! unset. The top-level sections in [`KIT_SECTIONS`] belong to the kit, every other top-level key
//! to the service's own configuration type. Unknown keys, missing required keys, and invalid
//! values all stop the service before it serves traffic, and no error message ever contains a
//! configured value.
//!
//! The deployment's platform settings (deploy/chart/README.md) arrive as `<PREFIX>_<NAME>`
//! variables with a single underscore, such as `TRUSTIOT_POSTGRES_HOST`, and every service gets
//! all of them. They appear as flat keys of the [`PLATFORM_SECTION`] table,
//! `platform.postgres_host`, to a configuration type that has a `platform` field, and are left
//! out for one that has not, so such a type can still reject unknown keys. Only the deployment
//! sets them: `<PREFIX>__PLATFORM__*` variables are rejected.

mod de;

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt;
use std::io;
use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;
use serde::de::DeserializeOwned;

use crate::http::HttpConfig;
use crate::shutdown::ShutdownConfig;
use crate::telemetry::TelemetryConfig;

use self::de::{DeError, DeErrorKind, ValueDe};

/// The environment-variable prefix of TrustIoT services.
pub const DEFAULT_ENV_PREFIX: &str = "TRUSTIOT";

/// Top-level configuration sections owned by the kit. A service's own configuration type must
/// not use these names.
pub const KIT_SECTIONS: [&str; 3] = ["http", "shutdown", "telemetry"];

/// The top-level key of the platform settings. A service that reads them declares a field of
/// this name whose type lists the settings it needs; that type must not deny unknown fields,
/// because every service gets every platform setting.
pub const PLATFORM_SECTION: &str = "platform";

/// The loaded configuration: the kit's sections and the service's own settings.
#[derive(Debug)]
pub struct Settings<T> {
    /// Settings of the kit itself (HTTP server, shutdown, telemetry).
    pub kit: KitConfig,
    /// The service's own settings.
    pub app: T,
}

/// The kit's configuration sections. Every key has a default.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct KitConfig {
    /// `http.*`: the HTTP server.
    pub http: HttpConfig,
    /// `shutdown.*`: draining on SIGTERM.
    pub shutdown: ShutdownConfig,
    /// `telemetry.*`: logs and traces.
    pub telemetry: TelemetryConfig,
}

impl Validate for KitConfig {
    fn validate(&self) -> Result<(), Invalid> {
        self.http.validate()?;
        self.shutdown.validate()?;
        self.telemetry.validate()
    }
}

/// Checks that run on a configuration after it deserialized, before the service starts.
pub trait Validate {
    /// Returns the first setting whose value is unacceptable.
    fn validate(&self) -> Result<(), Invalid> {
        Ok(())
    }
}

/// A setting with an unacceptable value. The message must not contain the value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invalid {
    /// The dotted key path, for example `http.request_timeout`.
    pub key: String,
    /// Why the value is unacceptable, without the value itself.
    pub message: String,
}

impl Invalid {
    /// Creates a validation failure for `key`.
    pub fn new(key: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            message: message.into(),
        }
    }
}

/// The configuration of a service that has no settings beyond the kit's.
#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NoConfig {}

impl Validate for NoConfig {}

/// Where a configured value came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Origin {
    /// The named environment variable.
    Env(String),
    /// The configuration file.
    File(PathBuf),
}

impl fmt::Display for Origin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Env(name) => write!(f, "set by environment variable {name}"),
            Self::File(path) => write!(f, "set in {}", path.display()),
        }
    }
}

/// Why the configuration could not be loaded. Messages name keys, variables, and files, never
/// configured values.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The configuration file could not be read.
    #[error("cannot read configuration file {path}: {source}")]
    ReadFile {
        /// The file named by `<PREFIX>_CONFIG_FILE`.
        path: PathBuf,
        /// The I/O error.
        source: io::Error,
    },
    /// The configuration file is not valid TOML.
    #[error(
        "configuration file {path} is not valid TOML (line {line}, column {column}): {message}"
    )]
    ParseFile {
        /// The file named by `<PREFIX>_CONFIG_FILE`.
        path: PathBuf,
        /// 1-based line of the error.
        line: usize,
        /// 1-based column of the error.
        column: usize,
        /// The parser's message.
        message: String,
    },
    /// A variable with the configuration prefix is not valid UTF-8.
    #[error("environment variable {name} is not valid UTF-8")]
    NotUnicode {
        /// The variable name, lossily decoded.
        name: String,
    },
    /// A variable with the configuration prefix does not name a usable key.
    #[error("environment variable {name} cannot be used: {reason}")]
    BadVariable {
        /// The variable name.
        name: String,
        /// Why it cannot be used.
        reason: String,
    },
    /// A required key has no value.
    #[error(
        "configuration key `{key}` is required: set environment variable {env} or `{key}` in \
         the configuration file"
    )]
    Missing {
        /// The dotted key path.
        key: String,
        /// The environment variable that sets it.
        env: String,
    },
    /// A key has a value of the wrong type or an unacceptable value.
    #[error("configuration key `{key}`{} is invalid: {message}", origin.as_ref().map(|o| format!(" ({o})")).unwrap_or_default())]
    Invalid {
        /// The dotted key path.
        key: String,
        /// Where the value came from, when it was configured rather than defaulted.
        origin: Option<Origin>,
        /// Why it is invalid, without the value.
        message: String,
    },
}

/// The environment variable that sets the dotted key `key`: `<PREFIX>__A__B` for `a.b`, and
/// `<PREFIX>_NAME` for the platform setting `platform.name`.
pub fn env_var_for(prefix: &str, key: &str) -> String {
    if let Some(setting) = key
        .strip_prefix(PLATFORM_SECTION)
        .and_then(|rest| rest.strip_prefix('.'))
    {
        return format!(
            "{prefix}_{}",
            setting.replace('.', "_").to_ascii_uppercase()
        );
    }
    let mut name = prefix.to_owned();
    for segment in key.split('.') {
        name.push_str("__");
        name.push_str(&segment.to_ascii_uppercase());
    }
    name
}

/// Loads the configuration from the process environment (and the file it names).
pub fn load<T: DeserializeOwned + Validate>(env_prefix: &str) -> Result<Settings<T>, ConfigError> {
    load_from(env_prefix, std::env::vars_os())
}

/// Loads the configuration from the given environment, so tests need not touch the process
/// environment.
pub fn load_from<T, I>(env_prefix: &str, vars: I) -> Result<Settings<T>, ConfigError>
where
    T: DeserializeOwned + Validate,
    I: IntoIterator<Item = (OsString, OsString)>,
{
    let mut file = None;
    let mut overrides = Vec::new();
    for (name, value) in vars {
        let lossy_name = name.to_string_lossy();
        let Some(variable) = Variable::classify(&lossy_name, env_prefix) else {
            continue;
        };
        let (Some(name), Ok(value)) = (name.to_str(), value.into_string()) else {
            return Err(ConfigError::NotUnicode {
                name: lossy_name.into_owned(),
            });
        };
        if value.is_empty() {
            continue;
        }
        match variable {
            Variable::File => file = Some(PathBuf::from(value)),
            Variable::Key(rest) => {
                overrides.push((
                    name.to_owned(),
                    key_segments(env_prefix, name, &rest)?,
                    value,
                ));
            }
            Variable::Platform(rest) => {
                // A name that cannot be a key is no platform setting of this kit.
                if rest
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
                {
                    let segments = vec![PLATFORM_SECTION.to_owned(), rest.to_ascii_lowercase()];
                    overrides.push((name.to_owned(), segments, value));
                }
            }
        }
    }
    // Environment order is arbitrary; sorting makes conflicting variables fail the same way
    // every time.
    overrides.sort();

    let mut origins = BTreeMap::new();
    let mut root = match &file {
        Some(path) => {
            let table = read_file(path)?;
            record_leaves(&table, "", &Origin::File(path.clone()), &mut origins);
            table
        }
        None => toml::Table::new(),
    };
    for (name, segments, value) in overrides {
        insert(&mut root, &segments, value, &name)?;
        origins.insert(segments.join("."), Origin::Env(name));
    }
    // Present even when the deployment sets no platform setting, so a missing required one is
    // reported by its own name rather than as a missing `platform` table.
    root.entry(PLATFORM_SECTION)
        .or_insert_with(|| toml::Value::Table(toml::Table::new()));

    let mut kit_table = toml::Table::new();
    for section in KIT_SECTIONS {
        if let Some(value) = root.remove(section) {
            kit_table.insert(section.to_owned(), value);
        }
    }
    let error = |e: DeError| from_de_error(e, env_prefix, &origins);
    let kit: KitConfig =
        KitConfig::deserialize(ValueDe::new(kit_table.into(), String::new())).map_err(error)?;
    let app = T::deserialize(ValueDe::new(root.into(), String::new())).map_err(error)?;

    let invalid = |e: Invalid| ConfigError::Invalid {
        origin: lookup_origin(&origins, &e.key),
        key: e.key,
        message: e.message,
    };
    kit.validate().map_err(invalid)?;
    app.validate().map_err(invalid)?;
    Ok(Settings { kit, app })
}

/// What a variable with the configuration prefix sets.
enum Variable {
    /// `<PREFIX>_CONFIG_FILE`: the configuration file.
    File,
    /// `<PREFIX>__A__B`: the key `a.b`; holds `A__B`.
    Key(String),
    /// `<PREFIX>_NAME`: the platform setting `platform.name`; holds `NAME`.
    Platform(String),
}

impl Variable {
    /// `None` for a variable that is not the kit's: another prefix, or `<PREFIX>_` followed by
    /// something other than a letter, a digit, or a second `_` (the same names the Helm chart
    /// reserves for its platform settings).
    fn classify(name: &str, env_prefix: &str) -> Option<Self> {
        let rest = name.strip_prefix(env_prefix)?.strip_prefix('_')?;
        if rest == "CONFIG_FILE" {
            Some(Self::File)
        } else if let Some(key) = rest.strip_prefix('_') {
            Some(Self::Key(key.to_owned()))
        } else if rest.starts_with(|c: char| c.is_ascii_alphanumeric()) {
            Some(Self::Platform(rest.to_owned()))
        } else {
            None
        }
    }
}

/// Splits the part of a variable name after `<PREFIX>__` into lower-case key segments.
fn key_segments(env_prefix: &str, name: &str, rest: &str) -> Result<Vec<String>, ConfigError> {
    let bad = |reason: &str| ConfigError::BadVariable {
        name: name.to_owned(),
        reason: reason.to_owned(),
    };
    let segments: Vec<String> = rest.split("__").map(str::to_ascii_lowercase).collect();
    if segments.iter().any(String::is_empty) {
        return Err(bad(
            "a key segment is empty; segments are separated by `__`",
        ));
    }
    if segments.first().map(String::as_str) == Some(PLATFORM_SECTION) {
        return Err(bad(&format!(
            "platform settings come only from the deployment, as {env_prefix}_<NAME> variables"
        )));
    }
    let valid = |segment: &String| {
        segment
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    };
    if !segments.iter().all(valid) {
        return Err(bad(
            "key segments may contain only letters, digits, and `_`",
        ));
    }
    Ok(segments)
}

fn read_file(path: &PathBuf) -> Result<toml::Table, ConfigError> {
    let text = std::fs::read_to_string(path).map_err(|source| ConfigError::ReadFile {
        path: path.clone(),
        source,
    })?;
    text.parse::<toml::Table>().map_err(|error| {
        let offset = error.span().map_or(0, |span| span.start);
        let (line, column) = line_and_column(&text, offset);
        ConfigError::ParseFile {
            path: path.clone(),
            line,
            column,
            message: error.message().to_owned(),
        }
    })
}

/// 1-based line and column (in characters) of a byte offset.
fn line_and_column(text: &str, offset: usize) -> (usize, usize) {
    let before = text.get(..offset).unwrap_or(text);
    let line = before.matches('\n').count() + 1;
    let line_start = before.rfind('\n').map_or(0, |index| index + 1);
    let column = before
        .get(line_start..)
        .map_or(0, |rest| rest.chars().count())
        + 1;
    (line, column)
}

fn record_leaves(
    table: &toml::Table,
    path: &str,
    origin: &Origin,
    origins: &mut BTreeMap<String, Origin>,
) {
    for (key, value) in table {
        let child = if path.is_empty() {
            key.clone()
        } else {
            format!("{path}.{key}")
        };
        match value {
            toml::Value::Table(inner) => record_leaves(inner, &child, origin, origins),
            _ => {
                origins.insert(child, origin.clone());
            }
        }
    }
}

/// Sets `segments` in `root` to the string `value`, creating tables on the way.
fn insert(
    root: &mut toml::Table,
    segments: &[String],
    value: String,
    name: &str,
) -> Result<(), ConfigError> {
    let conflict = |reason: String| ConfigError::BadVariable {
        name: name.to_owned(),
        reason,
    };
    let Some((last, parents)) = segments.split_last() else {
        return Err(conflict("it names no key".to_owned()));
    };
    let mut table = root;
    for (depth, segment) in parents.iter().enumerate() {
        let entry = table
            .entry(segment.clone())
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        table = match entry {
            toml::Value::Table(inner) => inner,
            _ => {
                return Err(conflict(format!(
                    "`{}` is already set to a value that is not a table",
                    segments[..=depth].join(".")
                )));
            }
        };
    }
    if matches!(table.get(last), Some(toml::Value::Table(_))) {
        return Err(conflict(format!(
            "`{}` is a table; set its keys instead",
            segments.join(".")
        )));
    }
    table.insert(last.clone(), toml::Value::String(value));
    Ok(())
}

/// The origin of `key`, or of the list or table it belongs to.
fn lookup_origin(origins: &BTreeMap<String, Origin>, key: &str) -> Option<Origin> {
    let mut candidate = key;
    loop {
        if let Some(origin) = origins.get(candidate) {
            return Some(origin.clone());
        }
        let cut = candidate.rfind(['.', '['])?;
        candidate = candidate.get(..cut)?;
    }
}

fn from_de_error(error: DeError, prefix: &str, origins: &BTreeMap<String, Origin>) -> ConfigError {
    let key = error.path.unwrap_or_default();
    match error.kind {
        DeErrorKind::Missing => ConfigError::Missing {
            env: env_var_for(prefix, &key),
            key,
        },
        DeErrorKind::Message(message) => ConfigError::Invalid {
            origin: lookup_origin(origins, &key),
            key,
            message,
        },
    }
}

/// Deserializes a [`Duration`] from a string with units, such as `"5s"`, `"500ms"`, or
/// `"1m 30s"`. Use with `#[serde(deserialize_with = "service_kit::config::duration")]`.
pub fn duration<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Duration, D::Error> {
    struct DurationVisitor;

    impl serde::de::Visitor<'_> for DurationVisitor {
        type Value = Duration;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a duration with a unit, such as \"5s\" or \"500ms\"")
        }

        fn visit_str<E: serde::de::Error>(self, text: &str) -> Result<Duration, E> {
            // humantime's errors quote the input; report the expected form instead.
            humantime::parse_duration(text.trim()).map_err(|_| {
                E::custom("expected a duration with a unit, such as \"5s\" or \"500ms\"")
            })
        }
    }

    deserializer.deserialize_str(DurationVisitor)
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use super::*;

    #[derive(Debug, Deserialize, PartialEq)]
    #[serde(deny_unknown_fields)]
    struct App {
        greeting: String,
        #[serde(default)]
        retries: u8,
        #[serde(default)]
        verbose: bool,
        #[serde(default)]
        origins: Vec<String>,
        #[serde(default)]
        database: Option<Database>,
    }

    #[derive(Debug, Deserialize, PartialEq)]
    #[serde(deny_unknown_fields)]
    struct Database {
        url: String,
        #[serde(default = "default_pool")]
        pool: u32,
    }

    fn default_pool() -> u32 {
        5
    }

    impl Validate for App {
        fn validate(&self) -> Result<(), Invalid> {
            if self.greeting.trim().is_empty() {
                return Err(Invalid::new("greeting", "must not be blank"));
            }
            Ok(())
        }
    }

    fn env(pairs: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
        pairs
            .iter()
            .map(|(name, value)| (OsString::from(name), OsString::from(value)))
            .collect()
    }

    fn load_env<T: DeserializeOwned + Validate>(
        pairs: &[(&str, &str)],
    ) -> Result<Settings<T>, ConfigError> {
        load_from("TEST", env(pairs))
    }

    /// A configuration file that is deleted when the test ends.
    struct Fixture(PathBuf);

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn write_file(name: &str, contents: &str) -> Fixture {
        let path =
            std::env::temp_dir().join(format!("service-kit-config-{}-{name}", std::process::id()));
        std::fs::write(&path, contents).expect("write the fixture file");
        Fixture(path)
    }

    #[test]
    fn kit_defaults_apply_without_any_configuration() {
        let settings: Settings<NoConfig> = load_env(&[]).expect("defaults are valid");
        assert_eq!(
            settings.kit.http.listen_addr,
            "0.0.0.0:8080".parse::<SocketAddr>().expect("valid address")
        );
        assert_eq!(settings.kit.shutdown.drain_delay, Duration::from_secs(5));
        assert_eq!(settings.kit.shutdown.drain_timeout, Duration::from_secs(20));
        assert_eq!(settings.kit.telemetry.log_filter, "info");
        assert_eq!(settings.kit.telemetry.otlp_endpoint, None);
    }

    #[test]
    fn environment_strings_become_typed_values() {
        let settings: Settings<App> = load_env(&[
            ("TEST__GREETING", "0123"),
            ("TEST__RETRIES", " 7 "),
            ("TEST__VERBOSE", "TRUE"),
            ("TEST__ORIGINS", "a.example, b.example,,"),
            ("TEST__DATABASE__URL", "postgres://db/app"),
            ("TEST__HTTP__LISTEN_ADDR", "127.0.0.1:9000"),
            ("TEST__SHUTDOWN__DRAIN_DELAY", "250ms"),
            ("UNRELATED", "ignored"),
        ])
        .expect("valid configuration");
        assert_eq!(
            settings.app,
            App {
                // A string stays a string even when it looks like a number.
                greeting: "0123".to_owned(),
                retries: 7,
                verbose: true,
                origins: vec!["a.example".to_owned(), "b.example".to_owned()],
                database: Some(Database {
                    url: "postgres://db/app".to_owned(),
                    pool: 5,
                }),
            }
        );
        assert_eq!(
            settings.kit.http.listen_addr,
            "127.0.0.1:9000"
                .parse::<SocketAddr>()
                .expect("valid address")
        );
        assert_eq!(
            settings.kit.shutdown.drain_delay,
            Duration::from_millis(250)
        );
    }

    #[test]
    fn empty_variables_count_as_unset() {
        let settings: Settings<App> = load_env(&[
            ("TEST__GREETING", "hi"),
            ("TEST__RETRIES", ""),
            ("TEST__TELEMETRY__OTLP_ENDPOINT", ""),
        ])
        .expect("valid configuration");
        assert_eq!(settings.app.retries, 0);
        assert_eq!(settings.kit.telemetry.otlp_endpoint, None);
    }

    #[test]
    fn missing_required_keys_name_the_variable() {
        let error = load_env::<App>(&[]).expect_err("greeting is required");
        assert_eq!(
            error.to_string(),
            "configuration key `greeting` is required: set environment variable \
             TEST__GREETING or `greeting` in the configuration file"
        );

        let error = load_env::<App>(&[("TEST__GREETING", "hi"), ("TEST__DATABASE__POOL", "3")])
            .expect_err("database.url is required once database is set");
        assert!(
            matches!(&error, ConfigError::Missing { key, env } if key == "database.url" && env == "TEST__DATABASE__URL"),
            "{error}"
        );
    }

    #[test]
    fn errors_never_contain_the_configured_value() {
        // Invalid in every position below, including as a log filter directive.
        let secret = "s3cr3t=value-that-must-not-leak";
        let cases: Vec<Vec<(&str, &str)>> = vec![
            vec![("TEST__GREETING", "hi"), ("TEST__RETRIES", secret)],
            vec![("TEST__GREETING", "hi"), ("TEST__RETRIES", "9999")],
            vec![("TEST__GREETING", "hi"), ("TEST__VERBOSE", secret)],
            vec![
                ("TEST__HTTP__LISTEN_ADDR", secret),
                ("TEST__GREETING", "hi"),
            ],
            vec![
                ("TEST__SHUTDOWN__DRAIN_DELAY", secret),
                ("TEST__GREETING", "hi"),
            ],
            vec![("TEST__GREETING", "hi"), ("TEST__DATABASE", secret)],
            vec![
                ("TEST__TELEMETRY__OTLP_ENDPOINT", secret),
                ("TEST__GREETING", "hi"),
            ],
            vec![
                ("TEST__TELEMETRY__LOG_FILTER", secret),
                ("TEST__GREETING", "hi"),
            ],
        ];
        for case in cases {
            let error = load_env::<App>(&case).expect_err("invalid configuration");
            let message = error.to_string();
            assert!(!message.contains(secret), "{message}");
            assert!(!message.contains("9999"), "{message}");
            assert!(message.contains("TEST__"), "names the variable: {message}");
        }
    }

    #[test]
    fn invalid_values_name_the_key_and_variable() {
        let error = load_env::<App>(&[("TEST__GREETING", "hi"), ("TEST__RETRIES", "many")])
            .expect_err("retries must be a number");
        assert_eq!(
            error.to_string(),
            "configuration key `retries` (set by environment variable TEST__RETRIES) is \
             invalid: expected a non-negative integer, found a string that is not one"
        );

        let error = load_env::<App>(&[("TEST__GREETING", " ")]).expect_err("blank greeting");
        assert_eq!(
            error.to_string(),
            "configuration key `greeting` (set by environment variable TEST__GREETING) is \
             invalid: must not be blank"
        );
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let error = load_env::<App>(&[("TEST__GREETING", "hi"), ("TEST__GRETING", "typo")])
            .expect_err("typo in a service key");
        assert!(
            matches!(&error, ConfigError::Invalid { key, origin: Some(Origin::Env(name)), .. } if key == "greting" && name == "TEST__GRETING"),
            "{error}"
        );

        let error = load_env::<NoConfig>(&[("TEST__HTTP__LISTEN_ADRR", "127.0.0.1:1")])
            .expect_err("typo in a kit key");
        let message = error.to_string();
        assert!(message.contains("`http.listen_adrr`"), "{message}");
        assert!(
            message.contains("`listen_addr`"),
            "lists the valid keys: {message}"
        );
    }

    #[test]
    fn malformed_variable_names_are_rejected() {
        for name in [
            "TEST__HTTP____LISTEN_ADDR",
            "TEST__HTTP__LISTEN-ADDR",
            "TEST__",
        ] {
            let error = load_env::<NoConfig>(&[(name, "x")]).expect_err("bad variable name");
            assert!(
                matches!(&error, ConfigError::BadVariable { name: n, .. } if n == name),
                "{name}: {error}"
            );
        }
        let error = load_env::<NoConfig>(&[
            ("TEST__HTTP", "x"),
            ("TEST__HTTP__LISTEN_ADDR", "127.0.0.1:1"),
        ])
        .expect_err("a value cannot also be a table");
        assert!(matches!(error, ConfigError::BadVariable { .. }), "{error}");
    }

    #[test]
    fn non_unicode_variables_are_rejected() {
        use std::os::unix::ffi::OsStringExt;
        let vars = vec![(
            OsString::from("TEST__GREETING"),
            OsString::from_vec(vec![0x66, 0xff, 0x6f]),
        )];
        let error = load_from::<App, _>("TEST", vars).expect_err("not UTF-8");
        assert!(matches!(error, ConfigError::NotUnicode { .. }), "{error}");
    }

    #[test]
    fn file_values_load_and_environment_overrides_them() {
        let path = write_file(
            "layered.toml",
            "greeting = \"from file\"\nretries = 2\n\n[database]\nurl = \"postgres://file\"\n\n\
             [http]\nlisten_addr = \"127.0.0.1:7000\"\n",
        );
        let path_text = path.0.to_str().expect("UTF-8 path");
        let settings: Settings<App> = load_env(&[
            ("TEST_CONFIG_FILE", path_text),
            ("TEST__RETRIES", "4"),
            ("TEST__DATABASE__POOL", "9"),
        ])
        .expect("valid configuration");
        assert_eq!(settings.app.greeting, "from file");
        assert_eq!(settings.app.retries, 4);
        assert_eq!(
            settings.app.database,
            Some(Database {
                url: "postgres://file".to_owned(),
                pool: 9,
            })
        );
        assert_eq!(
            settings.kit.http.listen_addr,
            "127.0.0.1:7000"
                .parse::<SocketAddr>()
                .expect("valid address")
        );
    }

    #[test]
    fn file_errors_point_at_the_file_without_its_contents() {
        let path = write_file(
            "broken.toml",
            "greeting = \"hi\"\npassword = \"hunter2\nother = 1\n",
        );
        let error = load_env::<App>(&[("TEST_CONFIG_FILE", path.0.to_str().expect("UTF-8"))])
            .expect_err("unterminated string");
        let message = error.to_string();
        assert!(
            matches!(&error, ConfigError::ParseFile { line: 2, .. }),
            "{message}"
        );
        assert!(!message.contains("hunter2"), "{message}");

        let path = write_file("typed.toml", "greeting = \"hi\"\nretries = \"lots\"\n");
        let error = load_env::<App>(&[("TEST_CONFIG_FILE", path.0.to_str().expect("UTF-8"))])
            .expect_err("retries must be a number");
        assert!(
            matches!(&error, ConfigError::Invalid { key, origin: Some(Origin::File(p)), .. } if key == "retries" && *p == path.0),
            "{error}"
        );
        assert!(!error.to_string().contains("lots"), "{error}");

        let missing = std::env::temp_dir().join(format!(
            "service-kit-config-{}-missing.toml",
            std::process::id()
        ));
        let error = load_env::<App>(&[("TEST_CONFIG_FILE", missing.to_str().expect("UTF-8"))])
            .expect_err("the named file must exist");
        assert!(matches!(error, ConfigError::ReadFile { .. }), "{error}");
    }

    #[test]
    fn env_var_names_follow_the_key_path() {
        assert_eq!(
            env_var_for("TRUSTIOT", "http.listen_addr"),
            "TRUSTIOT__HTTP__LISTEN_ADDR"
        );
        assert_eq!(env_var_for("TRUSTIOT", "greeting"), "TRUSTIOT__GREETING");
        assert_eq!(
            env_var_for("TRUSTIOT", "platform.postgres_host"),
            "TRUSTIOT_POSTGRES_HOST"
        );
    }

    /// Platform settings a service reads. No `deny_unknown_fields`: every service gets every
    /// platform setting.
    #[derive(Debug, Deserialize, PartialEq)]
    struct Platform {
        deployment_mode: String,
        #[serde(default)]
        postgres_port: Option<u16>,
    }

    #[derive(Debug, Deserialize, PartialEq)]
    #[serde(deny_unknown_fields)]
    struct WithPlatform {
        platform: Platform,
        #[serde(default)]
        greeting: Option<String>,
    }

    impl Validate for WithPlatform {}

    #[test]
    fn platform_settings_reach_a_configuration_that_asks_for_them() {
        let settings: Settings<WithPlatform> = load_env(&[
            ("TEST_DEPLOYMENT_MODE", "saas"),
            ("TEST_POSTGRES_PORT", "5432"),
            // Not asked for, so ignored.
            ("TEST_APP_HOST", "app.example.com"),
            // Cannot be a key, so no platform setting of the kit.
            ("TEST_ODD-NAME", "ignored"),
            ("TEST__GREETING", "hi"),
        ])
        .expect("valid configuration");
        assert_eq!(
            settings.app,
            WithPlatform {
                platform: Platform {
                    deployment_mode: "saas".to_owned(),
                    postgres_port: Some(5432),
                },
                greeting: Some("hi".to_owned()),
            }
        );
    }

    #[test]
    fn configurations_without_platform_settings_ignore_them() {
        let platform = [
            ("TEST_DEPLOYMENT_MODE", "saas"),
            ("TEST_APP_HOST", "app.example.com"),
        ];
        let mut vars = platform.to_vec();
        vars.push(("TEST__GREETING", "hi"));
        // `App` rejects unknown keys, yet the platform settings are no unknown keys to it.
        let settings: Settings<App> = load_env(&vars).expect("valid configuration");
        assert_eq!(settings.app.greeting, "hi");
        load_env::<NoConfig>(&platform).expect("valid configuration");
    }

    #[test]
    fn platform_setting_errors_name_the_platform_variable() {
        let error = load_env::<WithPlatform>(&[]).expect_err("deployment_mode is required");
        assert!(
            matches!(&error, ConfigError::Missing { key, env } if key == "platform.deployment_mode" && env == "TEST_DEPLOYMENT_MODE"),
            "{error}"
        );

        let secret = "s3cr3t-port";
        let error = load_env::<WithPlatform>(&[
            ("TEST_DEPLOYMENT_MODE", "saas"),
            ("TEST_POSTGRES_PORT", secret),
        ])
        .expect_err("not a port number");
        let message = error.to_string();
        assert!(
            message.contains(
                "`platform.postgres_port` (set by environment variable TEST_POSTGRES_PORT)"
            ),
            "{message}"
        );
        assert!(!message.contains(secret), "{message}");
    }

    #[test]
    fn platform_settings_cannot_be_set_as_service_keys() {
        let error = load_env::<WithPlatform>(&[
            ("TEST_DEPLOYMENT_MODE", "saas"),
            ("TEST__PLATFORM__DEPLOYMENT_MODE", "onprem"),
        ])
        .expect_err("only the deployment sets platform settings");
        assert!(
            matches!(&error, ConfigError::BadVariable { name, .. } if name == "TEST__PLATFORM__DEPLOYMENT_MODE"),
            "{error}"
        );
    }

    #[test]
    fn platform_settings_in_the_file_yield_to_the_environment() {
        let path = write_file(
            "platform.toml",
            "[platform]\ndeployment_mode = \"onprem\"\npostgres_port = 6432\n",
        );
        let settings: Settings<WithPlatform> = load_env(&[
            ("TEST_CONFIG_FILE", path.0.to_str().expect("UTF-8 path")),
            ("TEST_DEPLOYMENT_MODE", "saas"),
        ])
        .expect("valid configuration");
        assert_eq!(settings.app.platform.deployment_mode, "saas");
        assert_eq!(settings.app.platform.postgres_port, Some(6432));
    }

    #[test]
    fn variables_are_classified_by_what_follows_the_prefix() {
        let kind = |name: &str| match Variable::classify(name, "TEST") {
            Some(Variable::File) => "file".to_owned(),
            Some(Variable::Key(rest)) => format!("key {rest}"),
            Some(Variable::Platform(rest)) => format!("platform {rest}"),
            None => "none".to_owned(),
        };
        assert_eq!(kind("TEST_CONFIG_FILE"), "file");
        assert_eq!(kind("TEST__HTTP__LISTEN_ADDR"), "key HTTP__LISTEN_ADDR");
        assert_eq!(kind("TEST_POSTGRES_HOST"), "platform POSTGRES_HOST");
        assert_eq!(kind("TEST_3RD_PARTY"), "platform 3RD_PARTY");
        for name in ["TEST_-X", "TEST_", "TEST", "TESTX_Y", "OTHER__X"] {
            assert_eq!(kind(name), "none", "{name}");
        }
    }

    #[test]
    fn line_and_column_count_characters() {
        let text = "a = 1\nb = \"漢字\" x\n";
        let offset = text.find('x').expect("fixture has an x");
        assert_eq!(line_and_column(text, offset), (2, 10));
        assert_eq!(line_and_column(text, 0), (1, 1));
    }
}
