//! The gateway role of `protocol::acl` against a real Mosquitto 2.1 with dynamic security
//! (Laysi/trustiot-v2#268). The role is registered from the crate's own `createRole` JSON; the
//! tests then check what the broker does:
//!
//! - every `up` channel still reaches a cloud subscriber with its QoS and retain flag, and the
//!   last will on `up/status` is delivered;
//! - every `down` channel still reaches the gateway;
//! - a publish to a topic under the gateway's own `up/` that is not a protocol channel is
//!   refused with PUBACK 0x87, never reaches the cloud and leaves no retained message;
//! - the `up/#` wildcard rule this crate generated before the fix fails that same check.
//!
//! The broker runs in a throwaway container (podman or docker), listens only on the
//! container's loopback interface and is removed afterwards; the MQTT clients are the
//! `mosquitto_pub`, `mosquitto_sub` and `mosquitto_rr` binaries of the same image, run with
//! `exec`. Without a container runtime the tests skip, unless `CI` is set, where they fail.

#![allow(
    clippy::unwrap_used,
    reason = "test helpers unwrap like the tests themselves (clippy.toml allows it in tests)"
)]

use std::collections::BTreeSet;
use std::hash::{BuildHasher, RandomState};
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use protocol::GatewayId;
use protocol::acl::{AclRule, AclType, GATEWAY_ROLE, gateway_acl_topic, gateway_role_acls};
use protocol::topic::{Channel, Direction, gateway_subscriptions};
use serde_json::{Value as Json, json};

/// eclipse-mosquitto:2.1.2-alpine, pinned to the multi-arch index digest. The local dev
/// environment (`scripts/lib/stack.ts`) runs the same digest.
const MOSQUITTO_IMAGE: &str = "docker.io/library/eclipse-mosquitto@sha256:38c0da4f2ef84284d47b3b3eeea1cb3bdeabe81ee10caf0cd5c5ff61ee3ea408";

/// PUBACK reason code 0x87: not authorized.
const NOT_AUTHORIZED: u8 = 0x87;

/// How long to wait for one message on a subscription.
const RECEIVE_TIMEOUT: Duration = Duration::from_secs(15);

/// Starts the broker inside the container: dynamic security initialised with a random admin
/// password (from the environment, never the command line) and a listener on the container's
/// loopback interface only, so nothing outside the container can connect.
const START_SCRIPT: &str = r#"set -eu
mkdir -p /run/trustiot
cat > /run/trustiot/mosquitto.conf <<CONF
listener 1883 127.0.0.1
allow_anonymous false
plugin /usr/lib/mosquitto_dynamic_security.so
plugin_opt_config_file /run/trustiot/dynamic-security.json
CONF
mosquitto_ctrl dynsec init /run/trustiot/dynamic-security.json admin "$ADMIN_PASSWORD" > /dev/null
chown -R mosquitto:mosquitto /run/trustiot
exec mosquitto -c /run/trustiot/mosquitto.conf
"#;

/// Runs `mosquitto_sub` with the arguments after the pid file, writing its pid to that file
/// first so the test can kill it without a DISCONNECT (which fires the last will).
const SUBSCRIBE_SCRIPT: &str = r#"echo $$ > "$0"; exec mosquitto_sub "$@""#;

/// Fails the test in CI, where every prerequisite must be present; skips it elsewhere.
#[allow(
    clippy::print_stderr,
    reason = "tells the developer why a test did not run"
)]
fn unavailable(what: &str) {
    assert!(
        std::env::var_os("CI").is_none(),
        "{what} is required when CI is set"
    );
    eprintln!("skipped: {what} is not available (set CI to make this a failure)");
}

/// `podman` or `docker`, whichever works.
fn container_runtime() -> Option<&'static str> {
    ["podman", "docker"].into_iter().find(|runtime| {
        Command::new(runtime)
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    })
}

/// A random lowercase hex string, for container names, client IDs and throwaway passwords.
fn random_hex() -> String {
    format!(
        "{:016x}{:016x}",
        RandomState::new().hash_one(1_u8),
        RandomState::new().hash_one(2_u8)
    )
}

fn describe(output: &Output) -> String {
    format!(
        "status {}\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// A dynamic-security client: its username (the gateway's CN for a gateway) and password.
struct Account {
    username: String,
    password: String,
}

/// A throwaway Mosquitto container, removed when dropped.
struct Broker {
    runtime: &'static str,
    name: String,
    admin_password: String,
}

impl Drop for Broker {
    fn drop(&mut self) {
        let _ = Command::new(self.runtime)
            // `--volumes`: the image's anonymous volumes go with the container (Laysi/trustiot-v2#334).
            .args(["rm", "--force", "--volumes", &self.name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

impl Broker {
    fn start() -> Option<Self> {
        let Some(runtime) = container_runtime() else {
            unavailable("podman or docker");
            return None;
        };
        let broker = Self {
            runtime,
            name: format!("protocol-test-mosquitto-{}", random_hex()),
            admin_password: random_hex(),
        };
        let output = Command::new(runtime)
            .args([
                "run",
                "--detach",
                "--name",
                &broker.name,
                "--memory",
                "128m",
                "--env",
                "ADMIN_PASSWORD",
                "--entrypoint",
                "/bin/sh",
                MOSQUITTO_IMAGE,
                "-c",
                START_SCRIPT,
            ])
            .env("ADMIN_PASSWORD", &broker.admin_password)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "start Mosquitto: {}",
            describe(&output)
        );
        let started = Instant::now();
        while broker
            .try_control(&json!([{ "command": "getDefaultACLAccess" }]))
            .is_none()
        {
            assert!(
                started.elapsed() < Duration::from_secs(60),
                "Mosquitto did not answer dynamic-security commands in time"
            );
            std::thread::sleep(Duration::from_millis(300));
        }
        Some(broker)
    }

    fn exec(&self, args: &[&str]) -> Output {
        Command::new(self.runtime)
            .args(["exec", &self.name])
            .args(args)
            .output()
            .unwrap()
    }

    /// Sends dynamic-security commands; `None` when the broker did not answer.
    fn try_control(&self, commands: &Json) -> Option<Vec<Json>> {
        let request = json!({ "commands": commands }).to_string();
        let output = self.exec(&[
            "mosquitto_rr",
            "-h",
            "127.0.0.1",
            "-V",
            "5",
            "-u",
            "admin",
            "-P",
            &self.admin_password,
            "-t",
            "$CONTROL/dynamic-security/v1",
            "-e",
            "$CONTROL/dynamic-security/v1/response",
            "-W",
            "5",
            "-m",
            &request,
        ]);
        if !output.status.success() {
            return None;
        }
        let response: Json = serde_json::from_slice(&output.stdout).ok()?;
        response["responses"].as_array().cloned()
    }

    /// Sends dynamic-security commands and requires every one to succeed.
    fn control(&self, commands: &Json) -> Vec<Json> {
        let responses = self
            .try_control(commands)
            .unwrap_or_else(|| panic!("no answer to {commands}"));
        for response in &responses {
            assert!(response.get("error").is_none(), "{response}");
        }
        responses
    }

    fn create_client(&self, username: &str, role: &str) -> Account {
        let password = random_hex();
        self.control(&json!([{
            "command": "createClient",
            "username": username,
            "password": password,
            "roles": [{ "rolename": role }]
        }]));
        Account {
            username: username.to_owned(),
            password,
        }
    }

    /// Requires the broker to store exactly `acls` for `role`.
    fn assert_role(&self, role: &str, acls: &[AclRule]) {
        let stored = self.control(&json!([{ "command": "getRole", "rolename": role }]));
        let sorted = |rules: &Json| {
            let mut rules: Vec<String> = rules
                .as_array()
                .unwrap()
                .iter()
                .map(Json::to_string)
                .collect();
            rules.sort();
            rules
        };
        assert_eq!(
            sorted(&stored[0]["data"]["role"]["acls"]),
            sorted(&serde_json::to_value(acls).unwrap()),
            "the broker stores exactly the generated rules for {role}"
        );
    }

    /// Publishes one message and returns the PUBACK reason code (`None` for QoS 0, which has no
    /// acknowledgement). An empty payload with retain clears the topic's retained message.
    fn publish(
        &self,
        account: &Account,
        topic: &str,
        qos: u8,
        retain: bool,
        payload: &str,
    ) -> Option<u8> {
        let client_id = format!("{}-pub-{}", account.username, random_hex());
        let qos = qos.to_string();
        let mut args = vec![
            "mosquitto_pub",
            "-h",
            "127.0.0.1",
            "-V",
            "5",
            "-d",
            "-u",
            &account.username,
            "-P",
            &account.password,
            "-i",
            &client_id,
            "-q",
            &qos,
            "-t",
            topic,
        ];
        if payload.is_empty() {
            args.push("-n");
        } else {
            args.extend(["-m", payload]);
        }
        if retain {
            args.push("-r");
        }
        let output = self.exec(&args);
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && stdout.contains("received CONNACK (0)"),
            "publish to {topic}: {}",
            describe(&output)
        );
        if qos == "0" {
            return None;
        }
        let code = stdout
            .lines()
            .find_map(|line| line.split_once("received PUBACK ")?.1.split_once("RC:"))
            .map(|(_, rest)| rest.trim_end_matches(')').parse::<u8>().unwrap());
        assert!(
            code.is_some(),
            "no PUBACK for {topic}: {}",
            describe(&output)
        );
        code
    }

    /// Subscribes `account` to `filters` in the background. `will` sets a retained QoS 1 last
    /// will on that topic.
    fn subscribe(&self, account: &Account, filters: &[String], will: Option<&str>) -> Subscription {
        let client_id = format!("{}-sub-{}", account.username, random_hex());
        let pid_file = format!("/tmp/{client_id}.pid");
        let mut args: Vec<&str> = vec![
            "exec",
            // A terminal makes mosquitto_sub flush every line; through a pipe it buffers them.
            "--tty",
            &self.name,
            "/bin/sh",
            "-c",
            SUBSCRIBE_SCRIPT,
            &pid_file,
            "-h",
            "127.0.0.1",
            "-V",
            "5",
            "-d",
            "-u",
            &account.username,
            "-P",
            &account.password,
            "-i",
            &client_id,
            "-q",
            "1",
            // The broker forwards the publisher's retain flag instead of clearing it.
            "--retain-as-published",
            "-F",
            "MSG %t %r %q %l",
        ];
        for filter in filters {
            args.extend(["-t", filter]);
        }
        if let Some(topic) = will {
            args.extend([
                "--will-topic",
                topic,
                "--will-payload",
                "lwt",
                "--will-qos",
                "1",
                "--will-retain",
            ]);
        }
        let mut child = Command::new(self.runtime)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (sender, lines) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if sender.send(line.trim_end_matches('\r').to_owned()).is_err() {
                    break;
                }
            }
        });
        let mut subscription = Subscription {
            child,
            lines,
            pid_file,
            granted: Vec::new(),
        };
        // `mosquitto_sub -d` prints `Subscribed (mid: 1): 1, 1, 135` with one reason code per
        // filter once the SUBACK arrives.
        let line = subscription
            .next_line(|line| line.starts_with("Subscribed (mid:"))
            .unwrap_or_else(|| panic!("no SUBACK for {filters:?}"));
        subscription.granted = line
            .split_once("):")
            .unwrap()
            .1
            .split(',')
            .map(|code| code.trim().parse::<u8>().unwrap())
            .collect();
        subscription
    }

    /// Ends a subscription without DISCONNECT, as a crashed or cut-off client would.
    fn kill(&self, subscription: &mut Subscription) {
        let output = self.exec(&[
            "/bin/sh",
            "-c",
            r#"kill -9 "$(cat "$0")""#,
            &subscription.pid_file,
        ]);
        assert!(output.status.success(), "kill: {}", describe(&output));
        let _ = subscription.child.wait();
    }

    /// The topics under `filter` that hold a retained message, as `cloud` sees them.
    fn retained_topics(&self, cloud: &Account, filter: &str) -> BTreeSet<String> {
        let client_id = format!("{}-retained-{}", cloud.username, random_hex());
        let output = self.exec(&[
            "mosquitto_sub",
            "-h",
            "127.0.0.1",
            "-V",
            "5",
            "-u",
            &cloud.username,
            "-P",
            &cloud.password,
            "-i",
            &client_id,
            "-t",
            filter,
            "--retained-only",
            "-W",
            "2",
            "-F",
            "%t",
        ]);
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|line| !line.is_empty() && *line != "Timed out")
            .map(str::to_owned)
            .collect()
    }
}

/// One delivered message: topic, retain flag, QoS and payload length.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Delivered {
    topic: String,
    retain: bool,
    qos: u8,
    len: usize,
}

/// A background `mosquitto_sub`; killed when dropped.
struct Subscription {
    child: Child,
    lines: Receiver<String>,
    pid_file: String,
    granted: Vec<u8>,
}

impl Drop for Subscription {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Subscription {
    fn next_line(&self, wanted: impl Fn(&str) -> bool) -> Option<String> {
        let deadline = Instant::now() + RECEIVE_TIMEOUT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.lines.recv_timeout(left) {
                Ok(line) if wanted(&line) => return Some(line),
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => return None,
            }
        }
    }

    fn next_message(&self) -> Option<Delivered> {
        let line = self.next_line(|line| line.starts_with("MSG "))?;
        let fields: Vec<&str> = line.split(' ').collect();
        assert_eq!(fields.len(), 5, "{line}");
        Some(Delivered {
            topic: fields[1].to_owned(),
            retain: fields[2] == "1",
            qos: fields[3].parse().unwrap(),
            len: fields[4].parse().unwrap(),
        })
    }

    /// Every message received before `marker` arrives.
    fn until(&self, marker: &str) -> Vec<Delivered> {
        let mut received = Vec::new();
        loop {
            let message = self
                .next_message()
                .unwrap_or_else(|| panic!("marker {marker} not received; got {received:?}"));
            if message.topic == marker {
                return received;
            }
            received.push(message);
        }
    }
}

/// A broker with `role` holding `acls`, a gateway client `gw-a` bound to it, and a cloud
/// client allowed everything under `trustiot/v1/gw/`.
struct Setup {
    broker: Broker,
    gateway: Account,
    cloud: Account,
}

const GATEWAY: &str = "gw-a";
/// The topic the cloud publishes to mark the end of a test step on its own subscription.
const MARKER: &str = "trustiot/v1/gw/marker/up/telemetry";

impl Setup {
    fn start(role: &str, acls: &[AclRule]) -> Option<Self> {
        let broker = Broker::start()?;
        let create = json!({ "command": "createRole", "rolename": role, "acls": acls });
        broker.control(&json!([create]));
        broker.assert_role(role, acls);
        let all = "trustiot/v1/gw/#";
        broker.control(&json!([{
            "command": "createRole",
            "rolename": "test-cloud",
            "acls": [
                { "acltype": "publishClientSend", "topic": all, "allow": true },
                { "acltype": "publishClientReceive", "topic": all, "allow": true },
                { "acltype": "subscribePattern", "topic": all, "allow": true },
            ]
        }]));
        let gateway = broker.create_client(GATEWAY, role);
        let cloud = broker.create_client("test-cloud", "test-cloud");
        Some(Self {
            broker,
            gateway,
            cloud,
        })
    }

    /// The cloud's subscription to every gateway's `up/` tree.
    fn watch_up(&self) -> Subscription {
        let subscription =
            self.broker
                .subscribe(&self.cloud, &["trustiot/v1/gw/+/up/#".to_owned()], None);
        assert_eq!(subscription.granted, [1]);
        subscription
    }

    fn mark(&self) {
        self.broker.publish(&self.cloud, MARKER, 1, false, "end");
    }
}

fn gateway_topic(channel: Channel) -> String {
    channel.topic(&GatewayId::new(GATEWAY).unwrap())
}

/// Topics under the gateway's own `up/` that are not protocol channels.
fn non_protocol_up_topics() -> Vec<String> {
    [
        "",
        "/",
        "/spam",
        "/spam/1",
        "/telemetry/extra",
        "/telemetry/",
        "/Telemetry",
        "/telemetry-ack",
        "/config",
        "//status",
    ]
    .into_iter()
    .map(|suffix| format!("trustiot/v1/gw/{GATEWAY}/up{suffix}"))
    .collect()
}

/// The check this issue is about: the gateway publishes a retained 1 KiB message at QoS 1 to
/// every non-protocol topic under its own `up/`. Returns every way the broker let one through:
/// a PUBACK other than 0x87, delivery to the cloud subscription, or a stored retained message.
fn non_protocol_publish_violations(setup: &Setup, watch: &Subscription) -> Vec<String> {
    let payload = "x".repeat(1024);
    let topics = non_protocol_up_topics();
    let mut violations = Vec::new();
    for topic in &topics {
        let code = setup
            .broker
            .publish(&setup.gateway, topic, 1, true, &payload);
        if code != Some(NOT_AUTHORIZED) {
            violations.push(format!("{topic}: PUBACK {code:?}, expected 0x87"));
        }
    }
    setup.mark();
    for message in watch.until(MARKER) {
        if topics.contains(&message.topic) {
            violations.push(format!("{}: delivered to the cloud", message.topic));
        }
    }
    let retained = setup
        .broker
        .retained_topics(&setup.cloud, &format!("trustiot/v1/gw/{GATEWAY}/up/#"));
    for topic in retained.intersection(&topics.iter().cloned().collect()) {
        violations.push(format!("{topic}: stored as a retained message"));
    }
    violations
}

#[test]
fn the_gateway_role_allows_exactly_the_protocol_channels() {
    let Some(setup) = Setup::start(GATEWAY_ROLE, &gateway_role_acls()) else {
        return;
    };
    let broker = &setup.broker;
    let watch = setup.watch_up();

    // Every up channel, sent as the protocol says, reaches the cloud unchanged.
    let up: Vec<Channel> = Channel::ALL
        .into_iter()
        .filter(|channel| channel.direction() == Direction::Up)
        .collect();
    for &channel in &up {
        let delivery = channel.delivery();
        let qos = delivery.qos.level();
        let code = broker.publish(
            &setup.gateway,
            &gateway_topic(channel),
            qos,
            delivery.retain,
            channel.name(),
        );
        assert!(code.is_none_or(|code| code < 0x80), "{channel:?}: {code:?}");
    }
    setup.mark();
    let mut received = watch.until(MARKER);
    received.sort();
    let mut expected: Vec<Delivered> = up
        .iter()
        .map(|&channel| Delivered {
            topic: gateway_topic(channel),
            retain: channel.delivery().retain,
            qos: channel.delivery().qos.level(),
            len: channel.name().len(),
        })
        .collect();
    expected.sort();
    assert_eq!(received, expected);

    // Nothing outside the protocol channels gets through.
    assert_eq!(
        non_protocol_publish_violations(&setup, &watch),
        Vec::<String>::new()
    );

    // Dynamic security cannot refuse the retain flag on an allowed topic (README, ACL
    // section). If this fails, the broker now enforces it: update the README's decision.
    let telemetry = gateway_topic(Channel::Telemetry);
    let code = broker.publish(&setup.gateway, &telemetry, 1, true, "stray");
    assert!(code.is_some_and(|code| code < 0x80), "{code:?}");
    assert!(
        broker
            .retained_topics(&setup.cloud, &telemetry)
            .contains(&telemetry)
    );
    broker.publish(&setup.gateway, &telemetry, 1, true, "");
    assert_eq!(
        broker.retained_topics(&setup.cloud, &format!("trustiot/v1/gw/{GATEWAY}/up/#")),
        BTreeSet::from([gateway_topic(Channel::Status)]),
        "only up/status holds a retained message"
    );
    setup.mark();
    watch.until(MARKER);

    // The gateway receives every down channel, and its last will on up/status is delivered
    // when the connection drops.
    let mut gateway = broker.subscribe(
        &setup.gateway,
        &gateway_subscriptions(&GatewayId::new(GATEWAY).unwrap()),
        Some(&gateway_topic(Channel::Status)),
    );
    assert!(
        gateway.granted.iter().all(|&code| code < 0x80),
        "{:?}",
        gateway.granted
    );
    let down: Vec<Channel> = Channel::ALL
        .into_iter()
        .filter(|channel| channel.direction() == Direction::Down)
        .collect();
    for &channel in &down {
        let delivery = channel.delivery();
        let code = broker.publish(
            &setup.cloud,
            &gateway_topic(channel),
            delivery.qos.level(),
            delivery.retain,
            channel.name(),
        );
        assert!(code.is_none_or(|code| code < 0x80), "{channel:?}: {code:?}");
    }
    let mut got: Vec<String> = (0..down.len())
        .map(|_| gateway.next_message().unwrap().topic)
        .collect();
    got.sort();
    let mut want: Vec<String> = down.iter().map(|&channel| gateway_topic(channel)).collect();
    want.sort();
    assert_eq!(got, want);
    broker.kill(&mut gateway);
    setup.mark();
    let will = watch.until(MARKER);
    assert_eq!(
        will,
        [Delivered {
            topic: gateway_topic(Channel::Status),
            retain: true,
            qos: 1,
            len: 3,
        }]
    );

    // A last will on a non-protocol topic is dropped like any other publish there.
    let spam = format!("trustiot/v1/gw/{GATEWAY}/up/spam");
    let mut gateway = broker.subscribe(
        &setup.gateway,
        &gateway_subscriptions(&GatewayId::new(GATEWAY).unwrap()),
        Some(&spam),
    );
    broker.kill(&mut gateway);
    setup.mark();
    assert_eq!(watch.until(MARKER), []);
    assert!(!broker.retained_topics(&setup.cloud, &spam).contains(&spam));
}

#[test]
fn the_wildcard_rule_before_the_fix_fails_the_same_check() {
    // The rules this crate generated before Laysi/trustiot-v2#268.
    let mut legacy = vec![AclRule {
        acltype: AclType::PublishClientSend,
        topic: gateway_acl_topic(Direction::Up),
        priority: 0,
        allow: true,
    }];
    legacy.extend(
        gateway_role_acls()
            .into_iter()
            .filter(|rule| rule.acltype != AclType::PublishClientSend),
    );
    assert_eq!(legacy[0].topic, "trustiot/v1/gw/%u/up/#");
    let Some(setup) = Setup::start("legacy-gateway", &legacy) else {
        return;
    };
    let watch = setup.watch_up();
    let violations = non_protocol_publish_violations(&setup, &watch);
    // Every non-protocol topic is accepted (PUBACK 0x00), delivered and stored as retained.
    for topic in non_protocol_up_topics() {
        for how in [
            "PUBACK Some(0), expected 0x87",
            "delivered to the cloud",
            "stored as a retained message",
        ] {
            let violation = format!("{topic}: {how}");
            assert!(
                violations.contains(&violation),
                "{violation} missing in {violations:#?}"
            );
        }
    }
    assert_eq!(violations.len(), 3 * non_protocol_up_topics().len());

    // Migrating a role registered with the old rules: `modifyRole` with `acls` replaces the
    // role's whole ACL list, so the wildcard rule is gone (`addRoleACL` would only add rules
    // next to it), and the stray retained messages are cleared with empty retained payloads.
    setup.broker.control(&json!([{
        "command": "modifyRole",
        "rolename": "legacy-gateway",
        "acls": gateway_role_acls()
    }]));
    setup
        .broker
        .assert_role("legacy-gateway", &gateway_role_acls());
    for topic in non_protocol_up_topics() {
        setup.broker.publish(&setup.cloud, &topic, 1, true, "");
    }
    setup.mark();
    watch.until(MARKER);
    assert_eq!(
        non_protocol_publish_violations(&setup, &watch),
        Vec::<String>::new()
    );
}
