//! The web service's configuration (services/web/README.md, "設定").

use std::path::PathBuf;

use serde::Deserialize;
use service_kit::config::{Invalid, Validate};

/// Where the image puts the SPA's build output (deploy/images/web/image.toml, `[static]`).
pub const DEFAULT_ROOT_DIR: &str = "/srv/www";

/// The service's settings.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Platform settings, set only by the deployment. The service reads none of them.
    #[serde(default)]
    pub platform: Platform,
    /// `TRUSTIOT__ROOT_DIR`: the directory with the site, loaded once at startup. Default
    /// [`DEFAULT_ROOT_DIR`].
    #[serde(default = "default_root_dir")]
    pub root_dir: PathBuf,
}

/// The platform settings, all ignored. No `deny_unknown_fields`: every service gets every
/// platform setting.
#[derive(Debug, Default, Deserialize)]
pub struct Platform {}

fn default_root_dir() -> PathBuf {
    PathBuf::from(DEFAULT_ROOT_DIR)
}

impl Validate for Config {
    fn validate(&self) -> Result<(), Invalid> {
        if !self.root_dir.is_absolute() {
            return Err(Invalid::new("root_dir", "must be an absolute path"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use service_kit::config::{ConfigError, load_from};

    use super::*;

    fn load(vars: &[(&str, &str)]) -> Result<Config, ConfigError> {
        let vars = vars
            .iter()
            .map(|(name, value)| (OsString::from(name), OsString::from(value)));
        load_from::<Config, _>("TRUSTIOT", vars).map(|settings| settings.app)
    }

    #[test]
    fn the_root_defaults_to_the_image_directory() {
        let config = load(&[]).expect("defaults");
        assert_eq!(config.root_dir, PathBuf::from("/srv/www"));
    }

    #[test]
    fn the_root_can_be_set_and_must_be_absolute() {
        let config = load(&[("TRUSTIOT__ROOT_DIR", "/tmp/site")]).expect("absolute root");
        assert_eq!(config.root_dir, PathBuf::from("/tmp/site"));
        let error = load(&[("TRUSTIOT__ROOT_DIR", "site")]).expect_err("relative root");
        assert!(
            error.to_string().contains("must be an absolute path"),
            "{error}"
        );
    }

    #[test]
    fn platform_settings_are_accepted_and_unknown_keys_are_not() {
        load(&[
            ("TRUSTIOT_POSTGRES_HOST", "postgres"),
            ("TRUSTIOT_INTERNAL_IDENTITY_KEYS_FILE", "/keys/jwks.json"),
        ])
        .expect("platform settings");
        let error = load(&[("TRUSTIOT__ROOT", "/srv/www")]).expect_err("unknown key");
        assert!(error.to_string().contains("root"), "{error}");
    }
}
