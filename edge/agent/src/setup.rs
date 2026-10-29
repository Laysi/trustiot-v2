//! Everything the agent checks before it reports ready to systemd: the configuration, both TLS
//! layers, the client certificate and its key (from the TPM in production). A failure here
//! refuses to start; nothing in it needs the network, so a gateway without a connection still
//! starts (and keeps trying to connect).

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use protocol::{AgentVersion, GatewayId, IdError, UnixMillis};
use rustls_pki_types::UnixTime;

use crate::config::Config;
use crate::identity::{self, IdentityError, KeySource};
use crate::link::LinkSettings;
use crate::renewal::{Credentials, HttpsEndpoint, Renewal};
use crate::tls::{self, BrokerTrust, InnerConfig, RootCrl, TlsError, Validity};
use crate::tpm::TpmTransport;
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
    /// The root CRL.
    #[error("broker.crl: {0}")]
    RootCrl(TlsError),
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
    /// The link's settings, with the credentials' version of `renewal`.
    pub link: LinkSettings,
    /// Renews the client certificate ([`Renewal::run`]); heartbeats take the certificate's
    /// expiry from [`Renewal::certificate_not_after`].
    pub renewal: Renewal<HttpsEndpoint>,
    /// How often to send a heartbeat.
    pub heartbeat_interval: Duration,
    /// When the client certificate becomes valid.
    pub certificate_not_before: UnixMillis,
    /// When the client certificate expires, at start.
    pub certificate_not_after: UnixMillis,
    /// Whether the clock's time lies in the certificate's validity. The agent starts either
    /// way: the gateway's clock may be wrong (its hardware RTC never set, or its time lost), and
    /// the broker has the last word.
    pub certificate_validity: Validity,
    /// Whether the key comes from a file (development and tests) instead of the TPM.
    pub file_key: bool,
    /// The root CRL the inner TLS checks the broker's chain against.
    pub root_crl: RootCrl,
}

impl Setup {
    /// Loads and checks everything `config` names.
    ///
    /// # Errors
    ///
    /// See [`SetupError`].
    pub fn new(config: &Config) -> Result<Self, SetupError> {
        Self::new_with(config, &mut |device| {
            TpmTransport::Device(device.to_owned())
        })
    }

    /// [`Setup::new`], with a TPM key reached through `open_tpm(device)` instead of the device
    /// itself: tests pass a software TPM.
    ///
    /// # Errors
    ///
    /// See [`SetupError`].
    pub fn new_with(
        config: &Config,
        open_tpm: &mut dyn FnMut(&Path) -> TpmTransport,
    ) -> Result<Self, SetupError> {
        let identity = identity::load_with(&config.certificate, &config.key, open_tpm)?;
        let outer = tls::outer_config(&config.outer_trust).map_err(SetupError::OuterTls)?;
        let trust = BrokerTrust::load(&config.broker_ca).map_err(SetupError::InnerTls)?;
        let root_crl = RootCrl::load(config.broker_crl.as_deref(), &trust, clock_now())
            .map_err(SetupError::RootCrl)?;
        let inner = InnerConfig::new(
            tls::inner_config(&trust, &root_crl, &identity).map_err(SetupError::InnerTls)?,
        );
        let certificate_validity = tls::check_client_certificate(&trust, &identity, clock_now())
            .map_err(SetupError::InnerTls)?;
        let mut link = LinkSettings::new(identity.gateway.clone(), agent_version()?);
        link.inflight_limit = config.inflight_limit;
        link.reconnect = config.reconnect;
        let gateway = identity.gateway.clone();
        let (certificate_not_before, certificate_not_after) =
            (identity.not_before, identity.not_after);
        let renewal = Renewal::new(
            HttpsEndpoint::new(&config.url, Arc::clone(&outer)),
            config.renewal,
            Credentials {
                identity,
                certificate: config.certificate.clone(),
                trust,
                root_crl: root_crl.clone(),
                inner: inner.clone(),
            },
        );
        link.credentials = Some(renewal.credentials());
        Ok(Self {
            gateway,
            connector: WssConnector::new(config.url.clone(), outer, inner),
            link,
            renewal,
            heartbeat_interval: config.heartbeat_interval,
            certificate_not_before,
            certificate_not_after,
            certificate_validity,
            file_key: matches!(config.key, KeySource::File { .. }),
            root_crl,
        })
    }
}

/// The clock's time for the certificate check. `UnixTime::now` panics on a clock before 1970,
/// which a gateway whose RTC lost its time can have; such a time is simply "too early".
pub(crate) fn clock_now() -> UnixTime {
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
