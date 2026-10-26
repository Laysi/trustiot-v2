//! Dashboards (Laysi/trustiot-v2#150, decision 33; docs/architecture/ai-assistant.md,
//! 「儀表板」): a page and the datasets it shows, written by the `dashboard` tool as a
//! conversation's draft, published by a user into the tenant's saved dashboards, and opened in
//! the browser's sandbox, which runs each dataset as the user who opened it.
//!
//! - [`store`]: the dashboards and their versions in Postgres, and who sees and may change
//!   what.
//! - [`runner`]: dataset runs at telemetry-query as the caller, within the caps of [`caps`],
//!   flattened by [`flatten`] for semantic sources.
//! - [`checker`]: the dashboard check, which passes a page before the tool writes it.
//! - The public API over both: [`crate::api`].
//!
//! The `dashboard` tool ([`crate::tools::dashboard`]) writes drafts through [`store`] in a
//! transaction of the question's tenant, after it ran the datasets through the same
//! [`runner::DatasetRunner`] as the API (so the caps hold for both) and [`checker`] passed the
//! page.

pub mod caps;
pub mod checker;
pub mod flatten;
pub mod runner;
pub mod store;
