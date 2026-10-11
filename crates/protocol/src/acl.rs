//! Broker access control for gateways (Mosquitto dynamic security).
//!
//! Mosquitto takes the gateway's username from its client certificate's CN
//! (`use_identity_as_username`) and replaces `%u` in ACL topics with it. Every gateway client
//! gets the role [`GATEWAY_ROLE`] with the rules from [`gateway_role_acls`], so a gateway can
//! publish only to the `up` channels of `trustiot/v1/gw/{its CN}/` (one exact rule per channel,
//! no wildcard) and subscribe only under `trustiot/v1/gw/{its CN}/down/`. A publish anywhere
//! else, including a topic under its own `up/` that is not a protocol channel, gets PUBACK
//! reason code 0x87 (not authorized) under MQTT 5, which is why gateways must use MQTT 5:
//! MQTT 3.1.1 acknowledges the publish and silently drops it.
//!
//! Dynamic security cannot authorize by the retain flag: its ACL check looks only at the ACL
//! type, the topic and the client. A gateway may therefore set retain on any of its `up`
//! channels, but the exact rules bound what it can park on the broker to one retained message
//! per `up` channel; only [`crate::topic::Channel::Status`] is retained by protocol. The README's
//! ACL section records this decision and what consumers do with a stray retained message.
//!
//! The roles of the cloud services, the broker's default ACL access, the broker-wide limits and
//! how services authenticate are deployment decisions (Laysi/trustiot-v2#84);
//! [`crate::topic::Channel`] documents which channels each side publishes and subscribes.

use serde::Serialize;

use crate::topic::{Channel, Direction, TOPIC_PREFIX};

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

/// One dynamic-security ACL entry. Serializes to the JSON object that the `createRole`,
/// `modifyRole` (in `acls`) and `addRoleACL` commands of `$CONTROL/dynamic-security/v1` expect.
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

/// The ACL filter of one direction of the gateway's whole tree: `trustiot/v1/gw/%u/{up|down}/#`.
/// [`gateway_role_acls`] uses it only for the `down` direction (subscribing and receiving);
/// publishing is limited to [`gateway_channel_acl_topic`] of each `up` channel, because a
/// publish rule with a wildcard lets a gateway create any topic under its `up/`.
pub fn gateway_acl_topic(direction: Direction) -> String {
    format!("{TOPIC_PREFIX}/%u/{}/#", direction.as_str())
}

/// The exact ACL topic of one channel in the gateway's own tree, without wildcards:
/// `trustiot/v1/gw/%u/{up|down}/{channel}`.
pub fn gateway_channel_acl_topic(channel: Channel) -> String {
    format!(
        "{TOPIC_PREFIX}/%u/{}/{}",
        channel.direction().as_str(),
        channel.name()
    )
}

/// The rules of [`GATEWAY_ROLE`]: one exact `publishClientSend` rule per `up` channel, in
/// [`Channel::ALL`] order, then subscribing, receiving and unsubscribing under the gateway's own
/// `down/`. Everything else is denied by the broker's default ACL access for publishing and
/// subscribing, which is Mosquitto's default.
pub fn gateway_role_acls() -> Vec<AclRule> {
    let down = gateway_acl_topic(Direction::Down);
    let up_rules = Channel::ALL
        .into_iter()
        .filter(|channel| channel.direction() == Direction::Up)
        .map(|channel| AclRule {
            acltype: AclType::PublishClientSend,
            topic: gateway_channel_acl_topic(channel),
            priority: 0,
            allow: true,
        });
    let down_rules = [
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
    ];
    up_rules.chain(down_rules).collect()
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
        for topic in NON_PROTOCOL_UP_TOPICS {
            assert!(!may_publish("gw-a", topic), "{topic}");
        }
    }

    /// Topics in `gw-a`'s own `up/` subtree that are not protocol channels: the wildcard rule
    /// before Laysi/trustiot-v2#268 allowed every one of them, retained or not.
    const NON_PROTOCOL_UP_TOPICS: [&str; 10] = [
        "trustiot/v1/gw/gw-a/up",
        "trustiot/v1/gw/gw-a/up/",
        "trustiot/v1/gw/gw-a/up/spam",
        "trustiot/v1/gw/gw-a/up/spam/1",
        "trustiot/v1/gw/gw-a/up/telemetry/extra",
        "trustiot/v1/gw/gw-a/up/telemetry/",
        "trustiot/v1/gw/gw-a/up/Telemetry",
        "trustiot/v1/gw/gw-a/up/telemetry-ack",
        "trustiot/v1/gw/gw-a/up/config",
        "trustiot/v1/gw/gw-a/up//telemetry",
    ];

    #[test]
    fn publish_rules_are_exact_one_per_up_channel() {
        let rules = gateway_role_acls();
        let publish: Vec<&AclRule> = rules
            .iter()
            .filter(|rule| rule.acltype == AclType::PublishClientSend)
            .collect();
        let up: Vec<Channel> = Channel::ALL
            .into_iter()
            .filter(|channel| channel.direction() == Direction::Up)
            .collect();
        assert_eq!(publish.len(), up.len());
        for (rule, channel) in publish.iter().zip(&up) {
            assert_eq!(
                rule.topic,
                format!("trustiot/v1/gw/%u/up/{}", channel.name())
            );
            assert!(!rule.topic.contains(['+', '#']), "{}", rule.topic);
            assert!(rule.allow && rule.priority == 0, "{rule:?}");
        }
        // No other rule grants publishing, and no rule denies anything (deny is the default).
        assert!(rules.iter().all(|rule| rule.allow));
        let me = GatewayId::new("gw-a").unwrap();
        for rule in publish {
            let pattern = substitute(rule, me.as_str()).unwrap();
            for topic in NON_PROTOCOL_UP_TOPICS {
                assert!(!filter_matches(&pattern, topic), "{pattern} {topic}");
                assert!(!filter_covers(&pattern, topic), "{pattern} {topic}");
            }
        }
    }

    #[test]
    fn the_wildcard_rule_before_the_fix_fails_the_same_check() {
        // The rule Laysi/trustiot-v2#268 replaced: the same assertions as above must fail for it.
        let legacy = gateway_acl_topic(Direction::Up).replace("%u", "gw-a");
        for topic in NON_PROTOCOL_UP_TOPICS {
            assert!(filter_matches(&legacy, topic), "{topic}");
            assert!(filter_covers(&legacy, topic), "{topic}");
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
        let rule = |acltype: &str, topic: &str| serde_json::json!({ "acltype": acltype, "topic": topic, "priority": 0, "allow": true });
        assert_eq!(
            json,
            serde_json::json!([
                rule("publishClientSend", "trustiot/v1/gw/%u/up/telemetry"),
                rule("publishClientSend", "trustiot/v1/gw/%u/up/config-report"),
                rule("publishClientSend", "trustiot/v1/gw/%u/up/status"),
                rule("publishClientSend", "trustiot/v1/gw/%u/up/heartbeat"),
                rule("publishClientSend", "trustiot/v1/gw/%u/up/live"),
                rule("publishClientSend", "trustiot/v1/gw/%u/up/log"),
                rule("publishClientSend", "trustiot/v1/gw/%u/up/log-dump"),
                rule("subscribePattern", "trustiot/v1/gw/%u/down/#"),
                rule("publishClientReceive", "trustiot/v1/gw/%u/down/#"),
                rule("unsubscribePattern", "trustiot/v1/gw/%u/down/#"),
            ])
        );
    }
}
