//! MQTT 5 reason codes of rumqttc's packet types as the numbers of the specification
//! (OASIS MQTT 5.0, section 2.4). rumqttc keeps its own conversions private.

use rumqttc::v5::mqttbytes::QoS;
use rumqttc::v5::mqttbytes::v5::{ConnectReturnCode, PubAckReason, SubscribeReasonCode};

/// The CONNACK reason code.
pub(crate) fn connect(code: ConnectReturnCode) -> u8 {
    match code {
        ConnectReturnCode::Success => 0x00,
        ConnectReturnCode::UnspecifiedError => 0x80,
        ConnectReturnCode::MalformedPacket => 0x81,
        ConnectReturnCode::ProtocolError => 0x82,
        ConnectReturnCode::ImplementationSpecificError => 0x83,
        // The MQTT 3.1.1 refusals have no MQTT 5 number of their own; rumqttc never decodes
        // them from an MQTT 5 CONNACK. Their MQTT 5 counterparts:
        ConnectReturnCode::UnsupportedProtocolVersion
        | ConnectReturnCode::RefusedProtocolVersion => 0x84,
        ConnectReturnCode::ClientIdentifierNotValid | ConnectReturnCode::BadClientId => 0x85,
        ConnectReturnCode::BadUserNamePassword => 0x86,
        ConnectReturnCode::NotAuthorized => 0x87,
        ConnectReturnCode::ServerUnavailable | ConnectReturnCode::ServiceUnavailable => 0x88,
        ConnectReturnCode::ServerBusy => 0x89,
        ConnectReturnCode::Banned => 0x8A,
        ConnectReturnCode::BadAuthenticationMethod => 0x8C,
        ConnectReturnCode::TopicNameInvalid => 0x90,
        ConnectReturnCode::PacketTooLarge => 0x95,
        ConnectReturnCode::QuotaExceeded => 0x97,
        ConnectReturnCode::PayloadFormatInvalid => 0x99,
        ConnectReturnCode::RetainNotSupported => 0x9A,
        ConnectReturnCode::QoSNotSupported => 0x9B,
        ConnectReturnCode::UseAnotherServer => 0x9C,
        ConnectReturnCode::ServerMoved => 0x9D,
        ConnectReturnCode::ConnectionRateExceeded => 0x9F,
    }
}

/// The PUBACK reason code.
pub(crate) fn puback(reason: PubAckReason) -> u8 {
    match reason {
        PubAckReason::Success => 0x00,
        PubAckReason::NoMatchingSubscribers => 0x10,
        PubAckReason::UnspecifiedError => 0x80,
        PubAckReason::ImplementationSpecificError => 0x83,
        PubAckReason::NotAuthorized => 0x87,
        PubAckReason::TopicNameInvalid => 0x90,
        PubAckReason::PacketIdentifierInUse => 0x91,
        PubAckReason::QuotaExceeded => 0x97,
        PubAckReason::PayloadFormatInvalid => 0x99,
    }
}

/// The SUBACK reason code of one topic filter: the granted QoS (0 to 2) or a refusal (0x80 and
/// above).
pub(crate) fn suback(reason: &SubscribeReasonCode) -> u8 {
    match reason {
        SubscribeReasonCode::Success(QoS::AtMostOnce) => 0x00,
        SubscribeReasonCode::Success(QoS::AtLeastOnce) => 0x01,
        SubscribeReasonCode::Success(QoS::ExactlyOnce) => 0x02,
        SubscribeReasonCode::Failure | SubscribeReasonCode::Unspecified => 0x80,
        SubscribeReasonCode::ImplementationSpecific => 0x83,
        SubscribeReasonCode::NotAuthorized => 0x87,
        SubscribeReasonCode::TopicFilterInvalid => 0x8F,
        SubscribeReasonCode::PkidInUse => 0x91,
        SubscribeReasonCode::QuotaExceeded => 0x97,
        SubscribeReasonCode::SharedSubscriptionsNotSupported => 0x9E,
        SubscribeReasonCode::SubscriptionIdNotSupported => 0xA1,
        SubscribeReasonCode::WildcardSubscriptionsNotSupported => 0xA2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refusals_have_the_numbers_of_the_specification() {
        assert_eq!(connect(ConnectReturnCode::BadUserNamePassword), 0x86);
        assert_eq!(connect(ConnectReturnCode::NotAuthorized), 0x87);
        assert_eq!(puback(PubAckReason::NotAuthorized), 0x87);
        assert_eq!(puback(PubAckReason::NoMatchingSubscribers), 0x10);
        assert_eq!(suback(&SubscribeReasonCode::Success(QoS::AtLeastOnce)), 1);
        assert_eq!(suback(&SubscribeReasonCode::NotAuthorized), 0x87);
    }
}
