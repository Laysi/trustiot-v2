//! Receiving and applying the desired configuration (rebuild checklist E1,
//! Laysi/trustiot-v2#94): the devices to poll, which the cloud publishes as one retained
//! `down/config` message per gateway ([`protocol::config`]).
//!
//! - **Delivery.** The link subscribes to `down/config` on every connection, and every connection
//!   starts clean, so the broker sends the retained configuration again after each reconnect. A
//!   failed subscription is a failed connection that the link retries with backoff, so a
//!   gateway that is connected has its subscription. Deliveries go through an [`Inbox`]: it
//!   keeps only the latest, so the link's dispatcher never waits for an apply, and a burst of
//!   changes is applied once, as its last version.
//! - **Versions** ([`Applier::receive`], protocol README 「設定下發」). A higher version than the
//!   active one is validated, planned and applied whole, or rejected with its issues while the
//!   active version stays; the same version is not applied again but reported as applied again,
//!   so a report lost with a connection heals at the next delivery; a lower version is never
//!   applied and is answered with `stale_version`, so an outdated retained message cannot roll
//!   the gateway back (Laysi/trustiot#139). A payload that does not decode is rejected for the
//!   version [`DesiredConfig::peek_header`] finds (also when that is the active version; an
//!   older one is answered as stale, which tells the cloud what to exceed); one without a
//!   readable version, and an empty
//!   one (the retained message was cleared), cannot be answered and only logged, and the active
//!   version stays.
//! - **Apply** ([`fleet`]): unchanged devices keep polling untouched, changed ones restart with
//!   the new settings, removed ones stop before the report is sent and their state is cleared.
//! - **Persistence** ([`store`]): an applied configuration is saved on the data partition before
//!   it takes effect. At start the agent polls with the saved configuration at once, before and
//!   without a connection, and moves to the cloud's version once it arrives.
//! - **Report.** Every answer is a [`ConfigReport`] on `up/config-report`. One that cannot be
//!   sent (not connected, or the connection ended before the PUBACK) is sent again when the link
//!   is next connected, unless a newer answer replaced it.

pub mod fleet;
pub mod store;

use std::fmt::Write as _;

use protocol::config::{ConfigReport, DesiredConfig};
use protocol::topic::Channel;
use protocol::{ConfigVersion, DecodeError, Issue};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use self::fleet::{Fleet, Plan};
use self::store::Store;
use crate::link::{Link, LinkState, PublishError};

/// Hands `down/config` payloads from the link's dispatcher to [`run`]. Only the latest payload
/// is kept, so [`Inbox::deliver`] never waits.
#[derive(Debug)]
pub struct Inbox(watch::Sender<Option<Vec<u8>>>);

/// The receiving side of an [`Inbox`].
#[derive(Debug)]
pub struct Deliveries(watch::Receiver<Option<Vec<u8>>>);

/// A new inbox and its receiving side.
pub fn inbox() -> (Inbox, Deliveries) {
    let (sender, receiver) = watch::channel(None);
    (Inbox(sender), Deliveries(receiver))
}

impl Inbox {
    /// Delivers a `down/config` payload, replacing one not taken yet.
    pub fn deliver(&self, payload: Vec<u8>) {
        self.0.send_replace(Some(payload));
    }
}

impl Deliveries {
    /// The next payload; `None` once the inbox is dropped.
    async fn next(&mut self) -> Option<Vec<u8>> {
        loop {
            self.0.changed().await.ok()?;
            if let Some(payload) = self.0.borrow_and_update().clone() {
                return Some(payload);
            }
        }
    }
}

/// The configuration in effect.
#[derive(Debug)]
struct Active {
    version: ConfigVersion,
    /// The payload as received, compared with deliveries of the same version.
    payload: Vec<u8>,
    /// Whether the payload is on the disk; a failed save is tried again at the next delivery.
    saved: bool,
}

/// Decides what to do with each delivered configuration and applies it.
#[derive(Debug)]
pub struct Applier {
    fleet: Fleet,
    store: Store,
    active: Option<Active>,
}

impl Applier {
    /// Nothing applied yet; [`Applier::restore`] applies the saved configuration.
    pub fn new(fleet: Fleet, store: Store) -> Self {
        Self {
            fleet,
            store,
            active: None,
        }
    }

    /// The version in effect, if any.
    pub fn active_version(&self) -> Option<ConfigVersion> {
        self.active.as_ref().map(|active| active.version)
    }

    /// The devices being polled.
    pub fn fleet(&self) -> &Fleet {
        &self.fleet
    }

    /// Applies the configuration saved by an earlier run, so that the gateway polls before it
    /// reaches the cloud (or without it). A missing file means none was ever applied; an
    /// unreadable or invalid one is logged and ignored, and the gateway waits for the cloud's.
    /// Returns the version applied.
    pub async fn restore(&mut self) -> Option<ConfigVersion> {
        let store = self.store.clone();
        let loaded = tokio::task::spawn_blocking(move || store.load())
            .await
            .unwrap_or_else(|error| Err(std::io::Error::other(error.to_string())));
        let payload = match loaded {
            Ok(Some(payload)) => payload,
            Ok(None) => {
                tracing::info!(
                    "no configuration saved yet; polling nothing until the cloud's configuration arrives"
                );
                return None;
            }
            Err(error) => {
                tracing::error!(
                    path = %self.store.path().display(),
                    %error,
                    "cannot read the saved configuration; polling nothing until the cloud's configuration arrives"
                );
                return None;
            }
        };
        let planned = DesiredConfig::decode(&payload)
            .map_err(|error| error.to_issues())
            .and_then(|config| Plan::new(&config).map(|plan| (config.version, plan)));
        match planned {
            Ok((version, plan)) => {
                let changes = self.fleet.apply(plan).await;
                tracing::info!(
                    %version,
                    devices = self.fleet.len(),
                    "polling with the saved configuration until the cloud's arrives"
                );
                tracing::debug!(%changes, "restored");
                self.active = Some(Active {
                    version,
                    payload,
                    saved: true,
                });
                Some(version)
            }
            Err(issues) => {
                tracing::error!(
                    path = %self.store.path().display(),
                    issues = %summary(&issues),
                    "the saved configuration is not usable; polling nothing until the cloud's configuration arrives"
                );
                None
            }
        }
    }

    /// Handles one `down/config` payload and returns the report to send, if one can be sent
    /// (see the module documentation).
    pub async fn receive(&mut self, payload: Vec<u8>) -> Option<ConfigReport> {
        let decoded = DesiredConfig::decode(&payload);
        let version = match &decoded {
            Ok(config) => config.version,
            Err(DecodeError::Empty) => {
                tracing::warn!(
                    active = ?self.active_version(),
                    "the retained configuration was cleared; polling on with the active one"
                );
                return None;
            }
            Err(error) => match DesiredConfig::peek_header(&payload) {
                Ok(header) => header.version,
                Err(_) => {
                    tracing::error!(
                        %error,
                        active = ?self.active_version(),
                        "a configuration without a readable version cannot be answered; keeping the active one"
                    );
                    return None;
                }
            },
        };
        let active = self.active_version();
        if let Some(active) = active {
            if version < active {
                tracing::warn!(
                    %version,
                    %active,
                    "refused an older configuration than the active one"
                );
                return Some(ConfigReport::stale(version, active));
            }
            // Only a payload that decodes counts as the active version arriving again; one that
            // does not is rejected below like any other, and the active version stays.
            if version == active && decoded.is_ok() {
                self.same_version_again(&payload).await;
                return Some(ConfigReport::applied(version));
            }
        }
        let planned = decoded
            .map_err(|error| error.to_issues())
            .and_then(|config| Plan::new(&config));
        let plan = match planned {
            Ok(plan) => plan,
            Err(issues) => {
                tracing::warn!(
                    %version,
                    active = ?active,
                    issues = %summary(&issues),
                    "rejected the configuration; keeping the active one"
                );
                return Some(ConfigReport::rejected(version, active, issues));
            }
        };
        // Saved before it takes effect: after a power cut the gateway polls with what it last
        // reported, or with the version before when the save did not finish.
        let saved = self.save(&payload).await;
        let changes = self.fleet.apply(plan).await;
        tracing::info!(
            %version,
            previous = ?active,
            devices = self.fleet.len(),
            %changes,
            "applied the configuration"
        );
        self.active = Some(Active {
            version,
            payload,
            saved,
        });
        Some(ConfigReport::applied(version))
    }

    /// The active version arrived again (every reconnect delivers the retained message).
    async fn same_version_again(&mut self, payload: &[u8]) {
        let Some(active) = &self.active else { return };
        if active.payload != payload {
            // Versions increase with every change, so this is the cloud's mistake; applying it
            // would make the version mean two configurations.
            tracing::warn!(
                version = %active.version,
                "the active version arrived with other content; keeping the content applied first"
            );
        }
        if !active.saved {
            let payload = active.payload.clone();
            let saved = self.save(&payload).await;
            if let Some(active) = &mut self.active {
                active.saved = saved;
            }
        }
    }

    /// Saves `payload`; a failure is logged and the configuration is applied anyway: polling
    /// matters more than surviving the next restart, and the next delivery tries again.
    async fn save(&self, payload: &[u8]) -> bool {
        let store = self.store.clone();
        let owned = payload.to_vec();
        let saved = tokio::task::spawn_blocking(move || store.save(&owned))
            .await
            .unwrap_or_else(|error| Err(std::io::Error::other(error.to_string())));
        match saved {
            Ok(()) => true,
            Err(error) => {
                tracing::error!(
                    path = %self.store.path().display(),
                    %error,
                    "cannot save the configuration; applying it anyway, but a restart before the next delivery polls with the previous one"
                );
                false
            }
        }
    }

    /// Stops polling every device, for shutdown.
    pub async fn stop(&mut self) {
        self.fleet.stop().await;
    }
}

/// The first issues as one line for the log.
fn summary(issues: &[Issue]) -> String {
    const SHOWN: usize = 5;
    let mut line = String::new();
    for issue in issues.iter().take(SHOWN) {
        if !line.is_empty() {
            line.push_str("; ");
        }
        let _ = write!(
            line,
            "{} at {:?}: {}",
            issue.code, issue.path, issue.message
        );
    }
    if issues.len() > SHOWN {
        let _ = write!(line, "; {} more", issues.len() - SHOWN);
    }
    line
}

/// Applies the saved configuration, then every delivered one, and sends each report, until
/// `shutdown` is cancelled; then stops polling.
pub async fn run(
    mut applier: Applier,
    link: Link,
    mut deliveries: Deliveries,
    shutdown: CancellationToken,
) {
    applier.restore().await;
    let mut state = link.state();
    let mut pending: Option<ConfigReport> = None;
    loop {
        if let Some(report) = pending.take() {
            let connected = matches!(&*state.borrow_and_update(), LinkState::Connected { .. });
            if connected {
                tokio::select! {
                    () = shutdown.cancelled() => break,
                    sent = send_report(&link, &report) => match sent {
                        Sent::Done => {}
                        Sent::Later => pending = Some(report),
                        Sent::Stopped => break,
                    },
                }
            } else {
                pending = Some(report);
            }
        }
        tokio::select! {
            () = shutdown.cancelled() => break,
            payload = deliveries.next() => match payload {
                Some(payload) => {
                    if let Some(report) = applier.receive(payload).await {
                        pending = Some(report);
                    }
                }
                None => break,
            },
            changed = state.changed(), if pending.is_some() => {
                if changed.is_err() {
                    break;
                }
            }
        }
    }
    applier.stop().await;
}

enum Sent {
    /// Sent, or not worth sending again.
    Done,
    /// Send it when the link is next connected.
    Later,
    /// The link has stopped.
    Stopped,
}

async fn send_report(link: &Link, report: &ConfigReport) -> Sent {
    let payload = match report.encode() {
        Ok(payload) => payload,
        Err(error) => {
            tracing::error!(%error, version = %report.version, "a configuration report does not encode");
            return Sent::Done;
        }
    };
    let published = match link.reserve(Channel::ConfigReport).await {
        Ok(permit) => permit.publish(payload).await,
        Err(error) => Err(error),
    };
    match published {
        Ok(_) => {
            tracing::debug!(version = %report.version, outcome = %report.outcome, "sent the configuration report");
            Sent::Done
        }
        Err(PublishError::NotConnected | PublishError::ConnectionLost) => {
            tracing::debug!(version = %report.version, "the configuration report waits for the next connection");
            Sent::Later
        }
        Err(PublishError::Stopped) => Sent::Stopped,
        Err(error) => {
            tracing::error!(%error, version = %report.version, "the configuration report was not accepted");
            Sent::Done
        }
    }
}
