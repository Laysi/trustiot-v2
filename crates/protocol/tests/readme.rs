//! The crate README is the human specification: it must list every topic, every message type
//! and the broker ACL. This test fails when a channel or an ACL rule is added without
//! documenting it.

#![allow(
    clippy::unwrap_used,
    reason = "test helpers unwrap like the tests themselves (clippy.toml allows it in tests)"
)]

use protocol::acl::{GATEWAY_ALLOW_WILDCARD_SUBS, GATEWAY_ROLE, gateway_role_acls};
use protocol::topic::{Channel, TOPIC_PREFIX};

fn readme() -> String {
    std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/README.md")).unwrap()
}

/// The README table row of `channel`, by its `Channel` variant name in the first column.
fn channel_row(readme: &str, channel: Channel) -> &str {
    let marker = format!("| `{channel:?}` |");
    readme
        .lines()
        .find(|line| line.starts_with(&marker))
        .unwrap_or_else(|| panic!("README has no topic table row for {channel:?}"))
}

/// The message type that travels on `channel`, as the README names it.
fn message_type(channel: Channel) -> &'static str {
    match channel {
        Channel::Telemetry => "TelemetryBatch",
        Channel::TelemetryAck => "TelemetryAck",
        Channel::Config => "DesiredConfig",
        Channel::ConfigReport => "ConfigReport",
        Channel::Status => "Status",
        Channel::Heartbeat => "Heartbeat",
        Channel::Live => "LiveSamples",
        Channel::LiveControl => "LiveControl",
        Channel::Log => "LogBatch",
        Channel::LogRequest => "LogRequest",
        Channel::LogDump => "LogDump",
    }
}

fn kib(bytes: usize) -> String {
    if bytes >= 1024 * 1024 {
        format!("{} MiB", bytes / (1024 * 1024))
    } else {
        format!("{} KiB", bytes / 1024)
    }
}

#[test]
fn the_readme_lists_every_topic_with_its_message_and_delivery() {
    let readme = readme();
    assert!(readme.contains(&format!(
        "`{TOPIC_PREFIX}/{{gateway}}/{{up|down}}/{{channel}}`"
    )));
    for channel in Channel::ALL {
        let row = channel_row(&readme, channel);
        let delivery = channel.delivery();
        let topic = format!("`{}/{}`", channel.direction().as_str(), channel.name());
        assert!(
            row.contains(&topic),
            "{channel:?}: {topic} missing in {row}"
        );
        assert!(
            row.contains(&format!("`{}`", message_type(channel))),
            "{channel:?}: message type missing in {row}"
        );
        let cells: Vec<&str> = row.split('|').map(str::trim).collect();
        // | Channel | Topic | Message | Sender | QoS | retain | expiry | limit |
        assert_eq!(cells[5], delivery.qos.level().to_string(), "{row}");
        assert_eq!(cells[6], if delivery.retain { "是" } else { "否" }, "{row}");
        let expiry = delivery.message_expiry.map_or("無".to_owned(), |expiry| {
            format!("{} 秒", expiry.as_secs())
        });
        assert_eq!(cells[7], expiry, "{row}");
        assert_eq!(cells[8], kib(channel.max_payload_bytes()), "{row}");
    }
}

/// The rows of the ACL table: the table that follows the `| acltype | topic | allow |` header.
fn acl_table_rows(readme: &str) -> Vec<&str> {
    let mut lines = readme
        .lines()
        .skip_while(|line| *line != "| acltype | topic | allow |");
    assert!(lines.next().is_some(), "README has no ACL table");
    assert_eq!(lines.next(), Some("| --- | --- | --- |"));
    lines.take_while(|line| line.starts_with('|')).collect()
}

#[test]
fn the_readme_lists_the_gateway_acl() {
    let readme = readme();
    assert!(readme.contains(&format!("`{GATEWAY_ROLE}`")));
    let generated: Vec<String> = gateway_role_acls()
        .into_iter()
        .map(|rule| {
            let acltype = serde_json::to_value(rule.acltype).unwrap();
            format!(
                "| `{}` | `{}` | {} |",
                acltype.as_str().unwrap(),
                rule.topic,
                if rule.allow { "是" } else { "否" }
            )
        })
        .collect();
    // Exactly the generated rules, in order: a rule in the README that the code does not
    // generate (such as a stale wildcard rule) fails as well as a missing one.
    assert_eq!(acl_table_rows(&readme), generated);
}

#[test]
fn the_readme_records_the_wildcard_subscription_setting() {
    let setting = format!("`allowwildcardsubs: {GATEWAY_ALLOW_WILDCARD_SUBS}`");
    assert!(readme().contains(&setting), "README lacks {setting}");
}
