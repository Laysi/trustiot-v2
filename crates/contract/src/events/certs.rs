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
//!
//! Certificate renewal (Laysi/trustiot-v2#81, rebuild checklist D2, decision 12;
//! services/certs/README.md, "換發"):
//!
//! - Every renewal writes one [`GatewayCertificateRenewed`] in the transaction that records the
//!   new certificate in the ledger. It is a tenant event: the tenant the gateway is bound to when
//!   the certificate is signed, so the tenant's administrators see it in their operation log. Its
//!   aggregate is [`GATEWAY_CERTIFICATE_AGGREGATE`] with [`gateway_certificate_aggregate_id`],
//!   the tenant and the hardware ID: an aggregate belongs to the tenant of its first event
//!   (crates/outbox), so a gateway bound to another tenant later renews in an aggregate of that
//!   tenant.
//! - Refused renewals of a registered hardware ID are counted per time window, and each window
//!   writes one [`GatewayRenewalRefused`] once it has ended. The endpoint is public, so a
//!   refused request proves nothing about who sent it: the event is platform-wide (written as the
//!   platform role, no `tenant_id`) in the registration's aggregate, and a hardware ID that is not
//!   registered writes nothing, so unauthenticated traffic cannot grow the permanent event log
//!   (decision 34) beyond one event per registered gateway and window.
//!
//! Both have the certs service itself as the `actor`: the gateway is not a user.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use protocol::GatewayId;
use serde::{Deserialize, Serialize};

use crate::event::Event;
use crate::id::{GatewayBindingId, TenantId};
use crate::service::ServiceName;

/// The aggregate kind of factory registration events, and of the refused renewals of the
/// registered gateway ([`GatewayRenewalRefused`]); the aggregate ID is the hardware ID.
pub const GATEWAY_REGISTRATION_AGGREGATE: &str = "gateway_registration";

/// The aggregate kind of a gateway's certificate renewals in one tenant; the aggregate ID is
/// [`gateway_certificate_aggregate_id`].
pub const GATEWAY_CERTIFICATE_AGGREGATE: &str = "gateway_certificate";

/// The aggregate ID of the certificate renewals of `hardware_id` while bound to `tenant`:
/// `<tenant ID>:<hardware ID>`. The tenant is part of it because an aggregate belongs to the
/// tenant of its first event, and a gateway may be bound to another tenant later.
#[must_use]
pub fn gateway_certificate_aggregate_id(tenant: TenantId, hardware_id: &GatewayId) -> String {
    format!("{tenant}:{hardware_id}")
}

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

/// A gateway renewed its certificate through the HTTPS renewal endpoint: it signed the
/// service's challenge with its registered TPM key, its registration is active, it is bound to
/// the envelope's tenant and that tenant is not suspended. The previous certificates stay valid
/// until they expire.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GatewayCertificateRenewed {
    /// The gateway's hardware ID: the certificate's common name.
    pub hardware_id: GatewayId,
    /// The binding (assets service) under which it was signed.
    pub binding_id: GatewayBindingId,
    /// The new certificate's serial number, lower-case hex.
    pub certificate_serial: String,
    /// The issuing CA that signed it: the SHA-256 of its public key, lower-case hex.
    pub ca_key_id: String,
    /// When the certificate becomes valid.
    pub not_before: DateTime<Utc>,
    /// When it expires.
    pub not_after: DateTime<Utc>,
}

impl Event for GatewayCertificateRenewed {
    const TYPE: &'static str = "certs.gateway_certificate_renewed";
    const VERSION: u32 = 1;
    const PRODUCER: ServiceName = ServiceName::Certs;
}

/// Renewals of a registered gateway were refused during one time window (the certs service's
/// `renewal.refusal_window`); written once the window has ended, one per hardware ID and window.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GatewayRenewalRefused {
    /// The hardware ID the refused requests named.
    pub hardware_id: GatewayId,
    /// The window's start, inclusive.
    pub window_start: DateTime<Utc>,
    /// The window's end, exclusive.
    pub window_end: DateTime<Utc>,
    /// How many requests were refused in the window.
    pub refusals: u64,
    /// The refusals by reason (services/certs/README.md, "換發"; for example
    /// `signature_invalid`, `challenge_unknown`, `not_bound`); the counts add up to `refusals`.
    /// A later version of the service may add reasons.
    pub reasons: BTreeMap<String, u64>,
}

impl Event for GatewayRenewalRefused {
    const TYPE: &'static str = "certs.gateway_renewal_refused";
    const VERSION: u32 = 1;
    const PRODUCER: ServiceName = ServiceName::Certs;
}
