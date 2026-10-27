//! Events the certs service produces.
//!
//! Factory registration (Laysi/trustiot-v2#79, rebuild checklist D1, decision 12): every new
//! registration writes one [`GatewayRegistered`] in the transaction that records it. A gateway
//! belongs to no tenant when it leaves the factory, so the event is platform-wide: the envelope
//! has no `tenant_id`, and the certs service writes it as its platform role. The envelope's
//! `actor` is the platform administrator who signed in on the workstation. The aggregate is
//! [`GATEWAY_REGISTRATION_AGGREGATE`] with the hardware ID, which the platform assigns and never
//! reuses. The identity service's event log records it (the operation log, K2); the assets
//! service puts the gateway in its device pool from it (Laysi/trustiot-v2#101).

use protocol::GatewayId;
use serde::{Deserialize, Serialize};

use crate::event::Event;
use crate::service::ServiceName;

/// The aggregate kind of factory registration events; the aggregate ID is the hardware ID.
pub const GATEWAY_REGISTRATION_AGGREGATE: &str = "gateway_registration";

/// A production workstation registered a gateway at the factory. A repeated registration of the
/// same gateway returns the existing record and writes no event.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GatewayRegistered {
    /// The gateway's hardware ID: the common name of its certificates and its MQTT identity.
    pub hardware_id: GatewayId,
    /// The SHA-256 of the registered public key (DER SubjectPublicKeyInfo), lower-case hex. The
    /// key itself never changes for this hardware ID.
    pub public_key_sha256: String,
    /// The SHA-256 of the TPM's endorsement key (DER SubjectPublicKeyInfo), lower-case hex.
    pub endorsement_key_sha256: String,
    /// The serial number of the initial certificate, lower-case hex.
    pub certificate_serial: String,
    /// The name of the workstation that registered it (its certificate's
    /// `workstation:<name>`).
    pub workstation: String,
}

impl Event for GatewayRegistered {
    const TYPE: &'static str = "certs.gateway_registered";
    const VERSION: u32 = 1;
    const PRODUCER: ServiceName = ServiceName::Certs;
}
