//! Dashboards of the assistant (Laysi/trustiot-v2#150; checklist J4, decision 33): the types
//! the assistant, the check service (`services/dashboard-check`) and the frontend's sandbox
//! (`frontend/sandbox/`) share. The decisions behind them are in
//! `docs/architecture/ai-assistant.md`, 「儀表板」.
//!
//! - [`spec`]: a version's datasets, computations, constants and component list, and their
//!   validation.
//! - [`component`]: the platform components (time series, bar, KPI, table, status) and the
//!   custom page's item, and the checks of their columns against a result.
//! - [`names`]: the names of datasets, computations, constants and columns, and the
//!   references a page marks its values with.
//! - [`formula`]: the computation language.
//! - [`result`]: one run of a dataset as the page receives it, and the flattening of semantic
//!   answers.
//! - [`public`]: the bodies and message keys of the public dashboard API.
//! - [`check`]: the check API between the assistant and `dashboard-check`.
//! - [`sandbox`]: the sandbox's files and response headers.
//!
//! `tests/fixtures/dashboard/` holds JSON written from these types and the shared formula and
//! reference vectors; `tests/dashboard.rs` fails when they drift, and the frontend's protocol
//! test reads the same files.

pub mod check;
pub mod component;
pub mod formula;
pub mod names;
pub mod public;
pub mod result;
pub mod sandbox;
pub mod spec;

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize};
use utoipa::ToSchema;

/// The version of the sandbox runtime a dashboard version was checked with and runs with: the
/// page API (`window.tq`), the formula language, d3 and the check rules
/// (`frontend/sandbox/README.md`). Each version's files are kept unchanged, so a dashboard keeps
/// working when a newer runtime ships. Serialized as a number, `1` for the first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, ToSchema)]
#[serde(transparent)]
#[schema(as = DashboardRuntime, value_type = u16, minimum = 1)]
pub struct RuntimeVersion(u16);

impl RuntimeVersion {
    /// The first runtime.
    pub const V1: Self = Self(1);

    /// The runtime new versions are written with.
    pub const CURRENT: Self = Self::V1;

    /// Every runtime the build ships, oldest first.
    pub const SHIPPED: [Self; 1] = [Self::V1];

    /// The number.
    #[must_use]
    pub const fn get(self) -> u16 {
        self.0
    }

    /// The runtime numbered `number`, when it is one the build ships.
    #[must_use]
    pub fn shipped(number: u16) -> Option<Self> {
        Self::SHIPPED
            .into_iter()
            .find(|runtime| runtime.0 == number)
    }
}

impl fmt::Display for RuntimeVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Accepts only runtimes the build ships: a stored version names one it was checked with, and a
/// check request one the service has.
impl<'de> Deserialize<'de> for RuntimeVersion {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let number = u16::deserialize(deserializer)?;
        Self::shipped(number)
            .ok_or_else(|| serde::de::Error::custom("not a runtime version this build ships"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_versions_are_numbers_the_build_ships() {
        assert_eq!(
            serde_json::to_string(&RuntimeVersion::V1).expect("json"),
            "1"
        );
        assert_eq!(
            serde_json::from_str::<RuntimeVersion>("1").expect("json"),
            RuntimeVersion::V1
        );
        for text in ["0", "2", "-1", "\"1\"", "1.0", "null"] {
            assert!(
                serde_json::from_str::<RuntimeVersion>(text).is_err(),
                "{text}"
            );
        }
        assert_eq!(
            RuntimeVersion::CURRENT,
            *RuntimeVersion::SHIPPED.last().expect("one")
        );
    }
}
