//! Broker access control for gateways (Mosquitto dynamic security).
//!
//! Mosquitto takes the gateway's username from its client certificate's CN
//! (`use_identity_as_username`) and replaces `%u` in ACL topics with it. Every gateway client
//! gets the role [`GATEWAY_ROLE`] with the rules from [`gateway_role_acls`], so a gateway can
//! publish only under `trustiot/v1/gw/{its CN}/up/` and subscribe only under
//! `trustiot/v1/gw/{its CN}/down/`. A publish anywhere else gets PUBACK reason code 0x87 (not
//! authorized) under MQTT 5, which is why gateways must use MQTT 5: MQTT 3.1.1 acknowledges the
//! publish and silently drops it.
//!
//! The roles of the cloud services, the broker's default ACL access and how services
//! authenticate are deployment decisions (Laysi/trustiot-v2#84); [`crate::topic::Channel`]
//! documents which channels each side publishes and subscribes.

use serde::Serialize;

use crate::topic::{Direction, TOPIC_PREFIX};

/// The dynamic-security role every gateway client is bound to at provisioning.
pub const GATEWAY_ROLE: &str = "trustiot-gateway";

/// Mosquitto dynamic-security ACL types used by the gateway role.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum AclType {
    /// The client publishing to a topic.
    PublishClientSend,
    /// The broker delivering a message on a topic to the client.
    PublishClientReceive,
    /// The client subscribing with a filter; the filter must stay within the ACL topic.
    SubscribePattern,
    /// The client unsubscribing with a filter.
    UnsubscribePattern,
}

/// One dynamic-security ACL entry. Serializes to the JSON object that the `createRole` and
/// `addRoleACL` commands of `$CONTROL/dynamic-security/v1` expect.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct AclRule {
    /// What the rule controls.
    pub acltype: AclType,
    /// The topic or filter; `%u` is the client's username, that is its certificate CN.
    pub topic: String,
    /// Higher priorities are checked first.
    pub priority: i32,
    /// Allow or deny.
    pub allow: bool,
}

/// The ACL topic of one direction of the gateway's own tree: `trustiot/v1/gw/%u/{up|down}/#`.
pub fn gateway_acl_topic(direction: Direction) -> String {
    format!("{TOPIC_PREFIX}/%u/{}/#", direction.as_str())
}

/// The rules of [`GATEWAY_ROLE`]. Everything else is denied by the broker's default ACL access
/// for publishing and subscribing, which is Mosquitto's default.
pub fn gateway_role_acls() -> Vec<AclRule> {
    let up = gateway_acl_topic(Direction::Up);
    let down = gateway_acl_topic(Direction::Down);
    vec![
        AclRule {
            acltype: AclType::PublishClientSend,
            topic: up,
            priority: 0,
            allow: true,
        },
        AclRule {
            acltype: AclType::SubscribePattern,
            topic: down.clone(),
            priority: 0,
            allow: true,
        },
        AclRule {
            acltype: AclType::PublishClientReceive,
            topic: down.clone(),
            priority: 0,
            allow: true,
        },
        AclRule {
            acltype: AclType::UnsubscribePattern,
            topic: down,
            priority: 0,
            allow: true,
        },
    ]
}

/// Whether `topic` matches the MQTT topic filter `filter` (MQTT 5.0 section 4.7). Topics
/// starting with `$` never match a filter that starts with a wildcard.
pub fn filter_matches(filter: &str, topic: &str) -> bool {
    if topic.starts_with('$') && (filter.starts_with('+') || filter.starts_with('#')) {
        return false;
    }
    let mut topic_levels = topic.split('/');
    for filter_level in filter.split('/') {
        match filter_level {
            "#" => return true,
            "+" => {
                if topic_levels.next().is_none() {
                    return false;
                }
            }
            literal => {
                if topic_levels.next() != Some(literal) {
                    return false;
                }
            }
        }
    }
    topic_levels.next().is_none()
}

/// Whether every topic the subscription filter `subscription` can match is also matched by
/// `acl` — how Mosquitto checks a `subscribePattern` rule. A `+` or `#` in the subscription is
/// covered only by a `+` or `#` at the same place in the ACL topic.
pub fn filter_covers(acl: &str, subscription: &str) -> bool {
    let mut subscription_levels = subscription.split('/');
    for acl_level in acl.split('/') {
        match (acl_level, subscription_levels.next()) {
            ("#", _) => return true,
            (_, None) => return false,
            ("+", Some(level)) => {
                if level == "#" {
                    return false;
                }
            }
            (literal, Some(level)) => {
                if level != literal {
                    return false;
                }
            }
        }
    }
    subscription_levels.next().is_none()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::GatewayId;
    use crate::topic::{Channel, gateway_subscriptions};

    /// The rule topic with `%u` replaced, or `None` when Mosquitto would not substitute the
    /// username: it refuses pattern substitution for usernames containing `/`, `+` or `#` (the
    /// 2026-10-09 PoC confirmed that certificates with CN `+` or `#` can read nothing).
    fn substitute(rule: &AclRule, username: &str) -> Option<String> {
        (!username.contains(['/', '+', '#'])).then(|| rule.topic.replace("%u", username))
    }

    /// Whether `username` may use `topic` under the gateway role for `acltype`, evaluated like
    /// Mosquitto: first matching rule wins, default deny.
    fn allowed(acltype: AclType, username: &str, topic: &str) -> bool {
        gateway_role_acls()
            .iter()
            .filter(|rule| rule.acltype == acltype)
            .find(|rule| {
                substitute(rule, username).is_some_and(|pattern| match acltype {
                    AclType::SubscribePattern | AclType::UnsubscribePattern => {
                        filter_covers(&pattern, topic)
                    }
                    AclType::PublishClientSend | AclType::PublishClientReceive => {
                        filter_matches(&pattern, topic)
                    }
                })
            })
            .is_some_and(|rule| rule.allow)
    }

    fn may_publish(username: &str, topic: &str) -> bool {
        allowed(AclType::PublishClientSend, username, topic)
    }

    fn may_subscribe(username: &str, subscription: &str) -> bool {
        allowed(AclType::SubscribePattern, username, subscription)
    }

    fn may_receive(username: &str, topic: &str) -> bool {
        allowed(AclType::PublishClientReceive, username, topic)
    }

    #[test]
    fn gateway_publishes_only_its_own_up_channels() {
        let me = GatewayId::new("gw-a").unwrap();
        let other = GatewayId::new("gw-b").unwrap();
        for channel in Channel::ALL {
            let mine = channel.topic(&me);
            let theirs = channel.topic(&other);
            assert_eq!(
                may_publish(me.as_str(), &mine),
                channel.direction() == Direction::Up,
                "{mine}"
            );
            assert!(!may_publish(me.as_str(), &theirs), "{theirs}");
        }
        for topic in [
            "trustiot/v1/gw/gw-a",
            "trustiot/v1/gw/gw-ab/up/telemetry",
            "trustiot/v2/gw/gw-a/up/telemetry",
            "$CONTROL/dynamic-security/v1",
            "trustiot/v1/gw/+/up/telemetry",
        ] {
            assert!(!may_publish("gw-a", topic), "{topic}");
        }
    }

    #[test]
    fn gateway_subscribes_only_its_own_down_channels() {
        let me = GatewayId::new("gw-a").unwrap();
        for topic in gateway_subscriptions(&me) {
            assert!(may_subscribe("gw-a", &topic), "{topic}");
            assert!(may_receive("gw-a", &topic), "{topic}");
            assert!(!may_subscribe("gw-b", &topic), "{topic}");
        }
        assert!(may_subscribe("gw-a", "trustiot/v1/gw/gw-a/down/#"));
        for filter in [
            "#",
            "+/#",
            "trustiot/#",
            "trustiot/v1/gw/#",
            "trustiot/v1/gw/+/down/config",
            "trustiot/v1/gw/+/#",
            "trustiot/v1/gw/gw-a/#",
            "trustiot/v1/gw/gw-a/+/config",
            "trustiot/v1/gw/gw-a/up/telemetry",
            "trustiot/v1/gw/gw-b/down/config",
        ] {
            assert!(!may_subscribe("gw-a", filter), "{filter}");
        }
    }

    #[test]
    fn wildcard_usernames_gain_nothing() {
        // The CA issues only CNs that are valid gateway IDs, which exclude wildcards; Mosquitto
        // additionally refuses to substitute such a username, so the rules match nothing.
        for username in ["+", "#", "a/b", "gw-a/#"] {
            assert!(GatewayId::new(username).is_err());
            let topic = Channel::Telemetry.topic(&GatewayId::new("gw-a").unwrap());
            assert!(!may_publish(username, &topic), "{username}");
            assert!(
                !may_subscribe(username, "trustiot/v1/gw/gw-a/down/config"),
                "{username}"
            );
        }
    }

    #[test]
    fn filter_matching_follows_mqtt() {
        assert!(filter_matches("a/+/c", "a/b/c"));
        assert!(filter_matches("a/#", "a"));
        assert!(filter_matches("a/#", "a/b/c"));
        assert!(filter_matches("#", "a/b"));
        assert!(!filter_matches("#", "$SYS/x"));
        assert!(!filter_matches("+/x", "$SYS/x"));
        assert!(filter_matches("$SYS/x", "$SYS/x"));
        assert!(!filter_matches("a/+", "a/b/c"));
        assert!(!filter_matches("a/b", "a/b/c"));
        assert!(!filter_matches("a/b/c", "a/b"));
        assert!(filter_matches("a/+/+", "a//"));
        assert!(filter_covers("a/+/c", "a/b/c"));
        assert!(filter_covers("a/+/c", "a/+/c"));
        assert!(!filter_covers("a/b/c", "a/+/c"));
        assert!(!filter_covers("a/+", "a/#"));
        assert!(filter_covers("a/#", "a/b/#"));
        assert!(!filter_covers("a/b", "a/b/c"));
    }

    #[test]
    fn rules_serialize_for_dynamic_security() {
        let json = serde_json::to_value(gateway_role_acls()).unwrap();
        assert_eq!(
            json[0],
            serde_json::json!({
                "acltype": "publishClientSend",
                "topic": "trustiot/v1/gw/%u/up/#",
                "priority": 0,
                "allow": true
            })
        );
        assert_eq!(json[1]["acltype"], "subscribePattern");
        assert_eq!(json[2]["acltype"], "publishClientReceive");
        assert_eq!(json[3]["acltype"], "unsubscribePattern");
    }
}
