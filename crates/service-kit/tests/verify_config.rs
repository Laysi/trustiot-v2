//! Independent verification of Laysi/trustiot-v2#51: configuration comes only from the
//! environment and the file, is validated at startup, treats unknown and malformed keys safely,
//! has a clear precedence, and never echoes a configured value in an error.
//!
//! Runs against the public `load_from`, with the process environment out of the picture, and
//! against the real `example-service` for the "no other source" and exit-code claims.

mod support;

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use serde::Deserialize;
use service_kit::config::{ConfigError, Invalid, Validate, load_from};
use support::EXAMPLE_SERVICE;

const SECRET: &str = "s3cr3t-canary-d4f9";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct App {
    name: String,
    #[serde(default)]
    level: Level,
    #[serde(default)]
    retries: u8,
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    ports: Vec<u16>,
    #[serde(default)]
    ratio: Option<f32>,
    #[serde(default)]
    nested: Nested,
    #[serde(default)]
    pair: Option<(u8, u8)>,
    #[serde(default)]
    tagged: Option<Tagged>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Level {
    #[default]
    Low,
    High,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Nested {
    #[serde(default)]
    host: Option<std::net::IpAddr>,
    #[serde(default)]
    id: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
#[allow(dead_code, reason = "only deserialization behaviour is under test")]
enum Tagged {
    Plain(String),
    Num(u32),
}

impl Validate for App {
    fn validate(&self) -> Result<(), Invalid> {
        if self.name.len() > 32 {
            return Err(Invalid::new("name", "too long"));
        }
        let _ = (
            &self.level,
            self.retries,
            self.enabled,
            &self.ports,
            self.ratio,
        );
        let _ = (&self.nested.host, self.nested.id, self.pair, &self.tagged);
        Ok(())
    }
}

fn env(pairs: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
    pairs
        .iter()
        .map(|(name, value)| (OsString::from(name), OsString::from(value)))
        .collect()
}

fn load(pairs: &[(&str, &str)]) -> Result<service_kit::config::Settings<App>, ConfigError> {
    load_from("V", env(pairs))
}

fn file(name: &str, content: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    std::fs::create_dir_all(&dir).expect("tmp dir");
    let path = dir.join(format!("verify-config-{}-{name}.toml", std::process::id()));
    std::fs::write(&path, content).expect("write config");
    path
}

#[test]
fn no_error_ever_contains_a_configured_value() {
    // One malformed or rejected value per shape, each carrying the canary; plus the same through
    // a file. The Display text of the error must never contain it.
    let cases: &[(&str, &str)] = &[
        ("V__NAME", &format!("{SECRET}{SECRET}{SECRET}")), // validation failure
        ("V__LEVEL", SECRET),                              // unknown enum variant
        ("V__RETRIES", SECRET),                            // not an integer
        ("V__RETRIES", "99999999"),                        // out of range
        ("V__ENABLED", SECRET),                            // not a bool
        ("V__PORTS", &format!("1,{SECRET}")),              // bad list element
        ("V__RATIO", SECRET),
        ("V__NESTED__HOST", SECRET), // IpAddr parse
        ("V__NESTED__ID", SECRET),
        ("V__PAIR", &format!("1,{SECRET}")),
        ("V__TAGGED", SECRET), // newtype enum from a bare string
        ("V__HTTP__LISTEN_ADDR", SECRET),
        ("V__HTTP__REQUEST_TIMEOUT", SECRET),
        ("V__HTTP__REQUEST_TIMEOUT", &format!("5{SECRET}")),
        ("V__SHUTDOWN__DRAIN_TIMEOUT", SECRET),
        ("V__SHUTDOWN__DRAIN_DELAY", &format!("-{SECRET}")),
        ("V__TELEMETRY__LOG_FILTER", &format!("{SECRET}=nonsense")),
        (
            "V__TELEMETRY__LOG_FILTER",
            &format!("info,{SECRET}[x]=bogus"),
        ),
        ("V__TELEMETRY__OTLP_ENDPOINT", SECRET),
        (
            "V__TELEMETRY__OTLP_ENDPOINT",
            &format!("https://{SECRET}@host:4318"),
        ),
        (
            "V__TELEMETRY__OTLP_ENDPOINT",
            &format!("http://user:{SECRET}@host:4318"),
        ),
        (
            "V__TELEMETRY__OTLP_ENDPOINT",
            &format!("http://host:4318/p?token={SECRET}"),
        ),
        ("V__TELEMETRY__OTLP_ENDPOINT", &format!("ftp://{SECRET}")),
        (
            "V__TELEMETRY__OTLP_ENDPOINT",
            &format!("http://{SECRET} space"),
        ),
    ];
    for (name, value) in cases {
        let base = [("V__NAME", "ok"), (name, value)];
        // The failing variable replaces the base name when it is the same key.
        let pairs: Vec<(&str, &str)> = if *name == "V__NAME" {
            vec![(name, value)]
        } else {
            base.to_vec()
        };
        let error = load(&pairs).expect_err(&format!("{name}={value} must be rejected"));
        let text = format!("{error} | {error:?}");
        assert!(!text.contains(SECRET), "{name}: the value leaked: {text}");
    }
}

#[test]
fn a_malformed_file_never_echoes_its_content() {
    let bad_files = [
        format!("name = {SECRET}"),   // bare word
        format!("name = \"{SECRET}"), // unterminated string
        format!("[{SECRET}"),         // unterminated table header
        format!("{SECRET} = = 1"),
        format!("name = ok\nname = {SECRET}"), // duplicate key
        format!("name = 1979-05-27T07:32:00 {SECRET}"),
        format!("name = \"\\u{SECRET}\""), // bad escape
        format!("name = [1, {SECRET}"),
        format!("name = 0x{SECRET}"),
        format!("name = {{ a = {SECRET} }}"),
    ];
    for (index, content) in bad_files.iter().enumerate() {
        let path = file(&format!("bad{index}"), content);
        let result = load(&[("V_CONFIG_FILE", path.to_str().expect("utf-8 path"))]);
        let _ = std::fs::remove_file(&path);
        let error = result.expect_err("a broken file is rejected");
        let text = format!("{error} | {error:?}");
        assert!(
            !text.contains(SECRET),
            "file #{index} content leaked: {text}"
        );
    }
}

#[test]
fn precedence_is_environment_over_file_over_default_and_empty_means_unset() {
    let path = file(
        "precedence",
        "name = \"from-file\"\nretries = 7\n[http]\nrequest_timeout = \"11s\"\nlisten_addr = \"127.0.0.1:1111\"\n",
    );
    let path_text = path.to_str().expect("utf-8 path");
    let settings = load(&[
        ("V_CONFIG_FILE", path_text),
        ("V__RETRIES", "9"),
        ("V__HTTP__LISTEN_ADDR", "127.0.0.1:2222"),
        // Empty counts as unset, so the file's value stands.
        ("V__HTTP__REQUEST_TIMEOUT", ""),
    ])
    .expect("valid");
    let _ = std::fs::remove_file(&path);
    assert_eq!(settings.app.name, "from-file");
    assert_eq!(settings.app.retries, 9, "environment beats the file");
    assert_eq!(settings.kit.http.listen_addr.port(), 2222);
    assert_eq!(
        settings.kit.http.request_timeout,
        Duration::from_secs(11),
        "an empty variable does not override the file"
    );
    assert_eq!(
        settings.kit.shutdown.drain_timeout,
        Duration::from_secs(20),
        "the default stands when nothing sets it"
    );
}

#[test]
fn required_settings_are_named_and_unknown_or_odd_keys_are_rejected() {
    // Missing required key: names the key and the variable that sets it.
    let error = load(&[]).expect_err("name is required");
    let text = error.to_string();
    assert!(text.contains("name") && text.contains("V__NAME"), "{text}");

    // Unknown keys: app, kit section, nested, and through the file.
    for pairs in [
        vec![("V__NAME", "x"), ("V__NOPE", "1")],
        vec![("V__NAME", "x"), ("V__HTTP__NOPE", "1")],
        vec![("V__NAME", "x"), ("V__NESTED__NOPE", "1")],
        vec![("V__NAME", "x"), ("V__TELEMETRY__NOPE", "1")],
        vec![("V__NAME", "x"), ("V__PLATFORM__ANYTHING", "1")],
        vec![("V__NAME", "x"), ("V____", "1")],
        vec![("V__NAME", "x"), ("V__NESTED__", "1")],
        vec![("V__NAME", "x"), ("V__a-b", "1")],
        // A key that is both a value and a table.
        vec![("V__NAME", "x"), ("V__NESTED", "1")],
        vec![("V__NAME", "x"), ("V__NAME__SUB", "1")],
    ] {
        assert!(load(&pairs).is_err(), "{pairs:?} must be rejected");
    }
    let path = file("unknown", "name = \"x\"\nsurprise = 1\n");
    let result = load(&[("V_CONFIG_FILE", path.to_str().expect("utf-8 path"))]);
    let _ = std::fs::remove_file(&path);
    assert!(result.is_err(), "an unknown key in the file is rejected");

    // A missing file is an error, never silently ignored.
    assert!(matches!(
        load(&[
            ("V__NAME", "x"),
            ("V_CONFIG_FILE", "/nonexistent/config.toml")
        ]),
        Err(ConfigError::ReadFile { .. })
    ));
}

#[test]
fn other_prefixes_and_unrelated_variables_are_ignored() {
    let settings = load(&[
        ("V__NAME", "x"),
        ("OTHER__NAME", "y"),
        ("VX__NAME", "y"),
        ("HOME", "/root"),
        ("VERBOSE", "1"),
        ("V_", "1"),
    ])
    .expect("valid");
    assert_eq!(settings.app.name, "x");
}

#[test]
fn the_service_reads_no_command_line_argument_or_stdin_as_configuration() {
    // `--greeting`-style arguments must not configure the service: with the required
    // environment variable missing, startup fails whatever the arguments say.
    let output = Command::new(EXAMPLE_SERVICE)
        .env_clear()
        .args(["--greeting=hi", "greeting=hi", "-c", "/dev/null"])
        .stdin(Stdio::null())
        .output()
        .expect("run");
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("greeting"), "{stdout}");
}
