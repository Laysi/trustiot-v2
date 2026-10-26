//! The service's configuration (README.md, 「設定」): where the browser and the sandbox's files
//! are, where the browser may write, and how many checks may wait.

use std::path::{Path, PathBuf};
use std::time::Duration;

use contract::api::dashboard::check::CHECK_TIME_LIMIT;
use serde::Deserialize;
use service_kit::config::{Invalid, KitConfig, Validate};

/// The most checks that may wait for the one running.
pub const MAX_WAITING_LIMIT: usize = 8;

/// The longest a check may wait for the one running.
pub const MAX_WAIT_LIMIT: Duration = Duration::from_secs(60);

/// How long the cleanup after a check may take: the browser's processes are killed and reaped,
/// and its files removed. Part of the time budget `http.request_timeout` must cover.
pub const CLEANUP_LIMIT: Duration = Duration::from_secs(5);

/// The service's own settings, under the `TRUSTIOT__` prefix.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// The browser executable: Debian's `chromium-headless-shell` binary itself, not its shell
    /// wrapper (`TRUSTIOT__CHROMIUM`).
    pub chromium: PathBuf,
    /// The frontend build that holds `sandbox/` (`TRUSTIOT__SITE_DIR`); the image puts it at
    /// `/srv/www`.
    pub site_dir: PathBuf,
    /// The only directory the browser writes to; each check gets a fresh directory inside it
    /// (`TRUSTIOT__WORK_DIR`). The chart mounts a memory-backed volume here. When the service
    /// runs as the container's PID 1, everything in it is removed before and after every check.
    pub work_dir: PathBuf,
    /// The wait queue.
    pub queue: QueueConfig,
}

/// `queue.*`: checks that arrive while one runs.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct QueueConfig {
    /// How many checks may wait (`TRUSTIOT__QUEUE__MAX_WAITING`); one more is refused at once
    /// with 503 `unavailable`. Default 2.
    pub max_waiting: usize,
    /// How long a check may wait before it is refused with 503 `unavailable`
    /// (`TRUSTIOT__QUEUE__MAX_WAIT`). Default 10s.
    #[serde(deserialize_with = "service_kit::config::duration")]
    pub max_wait: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            chromium: PathBuf::from("/usr/lib/chromium/chromium-headless-shell"),
            site_dir: PathBuf::from("/srv/www"),
            work_dir: PathBuf::from("/tmp"),
            queue: QueueConfig::default(),
        }
    }
}

impl Default for QueueConfig {
    fn default() -> Self {
        Self {
            max_waiting: 2,
            max_wait: Duration::from_secs(10),
        }
    }
}

impl Validate for Config {
    fn validate(&self) -> Result<(), Invalid> {
        absolute("chromium", &self.chromium)?;
        absolute("site_dir", &self.site_dir)?;
        absolute("work_dir", &self.work_dir)?;
        if self.work_dir == Path::new("/") {
            return Err(Invalid::new("work_dir", "must not be the root directory"));
        }
        if self.queue.max_waiting > MAX_WAITING_LIMIT {
            return Err(Invalid::new(
                "queue.max_waiting",
                format!("must be at most {MAX_WAITING_LIMIT}"),
            ));
        }
        if self.queue.max_wait.is_zero() || self.queue.max_wait > MAX_WAIT_LIMIT {
            return Err(Invalid::new(
                "queue.max_wait",
                "must be longer than zero and at most 60s",
            ));
        }
        Ok(())
    }
}

impl Config {
    /// The longest a request can take: waiting in the queue, the check itself, and the cleanup.
    #[must_use]
    pub fn longest_request(&self) -> Duration {
        self.queue.max_wait + CHECK_TIME_LIMIT + CLEANUP_LIMIT
    }
}

/// Refuses settings of the kit that would make the service send something out of its pod, which
/// has no egress at all: the OTLP export of logs and traces.
///
/// # Errors
///
/// When `telemetry.otlp_endpoint` is set.
pub fn refuse_egress(kit: &KitConfig) -> Result<(), Invalid> {
    if kit.telemetry.otlp_endpoint.is_some() {
        return Err(Invalid::new(
            "telemetry.otlp_endpoint",
            "must be unset: dashboard-check has no network egress, so it logs to stdout only",
        ));
    }
    Ok(())
}

fn absolute(key: &str, path: &Path) -> Result<(), Invalid> {
    if path.is_absolute() {
        Ok(())
    } else {
        Err(Invalid::new(key, "must be an absolute path"))
    }
}

#[cfg(test)]
mod tests {
    use service_kit::config::{Settings, load_from};

    use super::*;

    fn load(vars: &[(&str, &str)]) -> Result<Settings<Config>, String> {
        load_from::<Config, _>(
            "TRUSTIOT",
            vars.iter()
                .map(|(key, value)| ((*key).into(), (*value).into())),
        )
        .map_err(|error| error.to_string())
    }

    #[test]
    fn defaults_fit_the_image() {
        let settings = load(&[]).expect("defaults load");
        let config = settings.app;
        assert_eq!(
            config.chromium,
            Path::new("/usr/lib/chromium/chromium-headless-shell")
        );
        assert_eq!(config.site_dir, Path::new("/srv/www"));
        assert_eq!(config.work_dir, Path::new("/tmp"));
        assert_eq!(config.queue.max_waiting, 2);
        assert_eq!(config.queue.max_wait, Duration::from_secs(10));
        // The default request timeout covers the longest request with the defaults.
        assert!(config.longest_request() <= settings.kit.http.request_timeout);
        refuse_egress(&settings.kit).expect("no export by default");
    }

    #[test]
    fn settings_come_from_the_environment() {
        let settings = load(&[
            ("TRUSTIOT__CHROMIUM", "/opt/chromium/headless_shell"),
            ("TRUSTIOT__SITE_DIR", "/srv/site"),
            ("TRUSTIOT__WORK_DIR", "/run/checks"),
            ("TRUSTIOT__QUEUE__MAX_WAITING", "0"),
            ("TRUSTIOT__QUEUE__MAX_WAIT", "3s"),
        ])
        .expect("loads");
        let config = settings.app;
        assert_eq!(config.chromium, Path::new("/opt/chromium/headless_shell"));
        assert_eq!(config.site_dir, Path::new("/srv/site"));
        assert_eq!(config.work_dir, Path::new("/run/checks"));
        assert_eq!(config.queue.max_waiting, 0);
        assert_eq!(config.queue.max_wait, Duration::from_secs(3));
    }

    #[test]
    fn invalid_settings_are_refused() {
        for (key, value, named) in [
            ("TRUSTIOT__CHROMIUM", "chromium", "chromium"),
            ("TRUSTIOT__SITE_DIR", "www", "site_dir"),
            ("TRUSTIOT__WORK_DIR", "tmp", "work_dir"),
            ("TRUSTIOT__WORK_DIR", "/", "work_dir"),
            ("TRUSTIOT__QUEUE__MAX_WAITING", "9", "queue.max_waiting"),
            ("TRUSTIOT__QUEUE__MAX_WAIT", "0s", "queue.max_wait"),
            ("TRUSTIOT__QUEUE__MAX_WAIT", "61s", "queue.max_wait"),
            ("TRUSTIOT__BROWSER", "/x", "browser"),
        ] {
            let error = load(&[(key, value)]).expect_err(key);
            assert!(error.contains(named), "{key}={value}: {error}");
        }
    }

    #[test]
    fn otlp_export_is_refused() {
        let settings = load(&[(
            "TRUSTIOT__TELEMETRY__OTLP_ENDPOINT",
            "http://otel-collector:4318",
        )])
        .expect("the kit accepts it");
        let error = refuse_egress(&settings.kit).expect_err("refused");
        assert_eq!(error.key, "telemetry.otlp_endpoint");
    }
}
