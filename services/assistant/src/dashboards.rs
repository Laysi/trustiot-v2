//! Dashboards (Laysi/trustiot-v2#150, decision 33; docs/architecture/ai-assistant.md,
//! 「儀表板」): a page and the datasets it shows, written by the `dashboard` tool as a
//! conversation's draft, published by a user into the tenant's saved dashboards, and opened in
//! the browser's sandbox, which runs each dataset as the user who opened it.
//!
//! - [`store`]: the dashboards and their versions in Postgres, and who sees and may change
//!   what.
//! - [`runner`]: dataset runs at telemetry-query as the caller, within the caps of [`caps`],
//!   flattened by [`flatten`] for semantic sources.
//! - The public API over both: [`crate::api`].
//!
//! The `dashboard` tool (Laysi/trustiot-v2#150, S5) writes drafts through [`store`] in a
//! transaction of the question's tenant and runs datasets through the same
//! [`runner::DatasetRunner`] as the API, so the caps hold for both.

pub mod caps;
pub mod flatten;
pub mod runner;
pub mod store;
