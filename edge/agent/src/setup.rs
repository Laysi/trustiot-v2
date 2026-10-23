//! Everything the agent checks before it reports ready to systemd: the configuration, both TLS
//! layers, the client certificate and its key (from the TPM in production). A failure here
//! refuses to start; nothing in it needs the network, so a gateway without a connection still
//! starts (and keeps trying to connect).

use std::time::{Duration, SystemTime};

use protocol::{AgentVersion, GatewayId, IdError, UnixMillis};
use rustls_pki_types::UnixTime;

use crate::config::Config;
use crate::identity::{self, IdentityError, KeySource};
use crate::link::LinkSettings;
use crate::tls::{self, TlsError, Validity};
use crate::transport::WssConnector;

/// This agent's release, from the crate version.
///
/// # Errors
///
/// Never for a crate version, which is a semantic version.
pub fn agent_version() -> Result<AgentVersion, IdError> {
    AgentVersion::new(env!("CARGO_PKG_VERSION"))
}

/// Why the agent cannot start.
#[derive(Debug, thiserror::Error)]
pub enum SetupError {
    /// The certificate or key.
    #[error("identity.certificate / identity.key: {0}")]
    Identity(#[from] IdentityError),
    /// The outer TLS trust.
    #[error("cloud.trust: {0}")]
    OuterTls(TlsError),
    /// The inner TLS trust.
    #[error("broker.ca: {0}")]
    InnerTls(TlsError),
    /// The agent's own version string.
    #[error("agent version: {0}")]
    Version(#[from] IdError),
}

/// The agent, ready to connect.
#[derive(Debug)]
pub struct Setup {
    /// The gateway, from the certificate's CN.
    pub gateway: GatewayId,
    /// Opens connections to the broker.
    pub connector: WssConnector,
    /// The link's settings.
    pub link: LinkSettings,
    /// How often to send a heartbeat.
    pub heartbeat_interval: Duration,
    /// When the client certificate becomes valid.
    pub certificate_not_before: UnixMillis,
    /// When the client certificate expires (reported in heartbeats).
    pub certificate_not_after: UnixMillis,
    /// Whether the clock's time lies in the certificate's validity. The agent starts either
    /// way: the gateway's clock may be wrong (its hardware RTC never set, or its time lost), and
    /// the broker has the last word.
    pub certificate_validity: Validity,
    /// Whether the key comes from a file (development and tests) instead of the TPM.
    pub file_key: bool,
}

impl Setup {
    /// Loads and checks everything `config` names.
    ///
    /// # Errors
    ///
    /// See [`SetupError`].
    pub fn new(config: &Config) -> Result<Self, SetupError> {
        let identity = identity::load(&config.certificate, &config.key)?;
        let outer = tls::outer_config(&config.outer_trust).map_err(SetupError::OuterTls)?;
        let inner =
            tls::inner_config(&config.broker_ca, &identity).map_err(SetupError::InnerTls)?;
        let certificate_validity =
            tls::check_client_certificate(&config.broker_ca, &identity, clock_now())
                .map_err(SetupError::InnerTls)?;
        let mut link = LinkSettings::new(identity.gateway.clone(), agent_version()?);
        link.inflight_limit = config.inflight_limit;
        link.reconnect = config.reconnect;
        Ok(Self {
            gateway: identity.gateway,
            connector: WssConnector::new(
                config.url.clone(),
                outer,
                inner,
                config.broker_name.clone(),
            ),
            link,
            heartbeat_interval: config.heartbeat_interval,
            certificate_not_before: identity.not_before,
            certificate_not_after: identity.not_after,
            certificate_validity,
            file_key: matches!(config.key, KeySource::File { .. }),
        })
    }
}

/// The clock's time for the certificate check. `UnixTime::now` panics on a clock before 1970,
/// which a gateway whose RTC lost its time can have; such a time is simply "too early".
fn clock_now() -> UnixTime {
    UnixTime::since_unix_epoch(
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_crate_version_is_a_valid_agent_version() {
        assert_eq!(agent_version().unwrap().as_str(), env!("CARGO_PKG_VERSION"));
    }
}
