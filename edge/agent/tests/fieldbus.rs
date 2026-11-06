//! The agent's side of the fieldbus allowlist (Laysi/trustiot-v2#865), end to end: the real
//! `trustiot-agent` under its real unit, with the allowlist component's socket and service
//! (Laysi/trustiot-v2#864), the default deny and the nameserver units (Laysi/trustiot-v2#879) and
//! the egress check (Laysi/trustiot-v2#878), joined as `README.md` (section "連線位址的限制")
//! tells Laysi/trustiot-v2#866 to join them.
//!
//! No gateway and no cloud are needed: the throwaway Debian 13 container of `tests/egress.rs` (the
//! same image) runs systemd as PID 1 with `tests/egress/lab.sh`'s network, started at boot by a
//! unit of its own. On the peer, the site's LAN, two Modbus TCP devices answer every read (a Perl
//! responder per connection, run by socat, that also logs each request) and a third port accepts
//! connections without being a device; the cloud's port closes every connection, so the agent
//! never connects. It polls the configuration saved in its state directory, as after a restart
//! without the cloud (rebuild checklist E1). Where the cloud would send a new configuration the
//! test saves it and restarts the agent: both go through `Fleet::apply`, whose ordering on a
//! configuration received while the agent runs `src/desired/fleet.rs` tests.
//!
//! Whether the agent ever connected to a device before its list was applied shows in the default
//! deny's own refusal counter: the agent's account sends nothing else that the default deny
//! refuses (DNS to the nameserver and the cloud's port 443 are allowed), and every load of the
//! rules file starts the counter at 0.
//!
//! The container needs podman; when systemd does not come up in it the tests say so and skip,
//! also with `CI` set, as `tests/egress.rs` does.

#![allow(
    clippy::unwrap_used,
    clippy::print_stderr,
    reason = "test code; a skipped test says why on stderr, and the evidence lines go there too"
)]

use std::collections::BTreeSet;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use agent::allowlist::{SET_V4, TABLE};
use agent::nameservers;
use protocol::config::{
    ByteOrder, DataPoint, DataType, DesiredConfig, Device, DeviceModel, FunctionCode, Host,
    Transport,
};
use protocol::{ConfigVersion, DeviceId, ModelName, PointKey, V1};
use testkit::container::{Container, container_runtime, runtime_output, unavailable, unique_name};
use testkit::mqtt::TestPki;

const AGENT_EXECUTABLE: &str = env!("CARGO_BIN_EXE_trustiot-agent");
const ALLOWLIST_EXECUTABLE: &str = env!("CARGO_BIN_EXE_trustiot-agent-allowlist");
const ALLOWLIST_INSTALLED: &str = "/usr/libexec/trustiot-agent/allowlist";
const NAMESERVERS_EXECUTABLE: &str = env!("CARGO_BIN_EXE_trustiot-agent-nameservers");
const NAMESERVERS_INSTALLED: &str = "/usr/libexec/trustiot-agent/nameservers";

const AGENT_UNIT: &str = include_str!("../systemd/trustiot-agent.service");
const EGRESS_UNIT: &str = include_str!("../systemd/trustiot-agent-egress.service");
const NFTABLES_RELOAD: &str = include_str!("../systemd/nftables-reload.conf");
const CHECK_UNIT: &str = include_str!("../systemd/trustiot-agent-egress-check.service");
const CHECK_TIMER: &str = include_str!("../systemd/trustiot-agent-egress-check.timer");
const ALLOWLIST_SOCKET: &str = include_str!("../systemd/trustiot-agent-allowlist.socket");
const ALLOWLIST_SERVICE: &str = include_str!("../systemd/trustiot-agent-allowlist@.service");
const NAMESERVERS_SERVICE: &str = include_str!("../systemd/trustiot-agent-nameservers.service");
const NAMESERVERS_PATH: &str = include_str!("../systemd/trustiot-agent-nameservers.path");
const SYSUSERS: &str = include_str!("../systemd/trustiot-agent.sysusers");
const RULES: &str = include_str!("../systemd/trustiot-agent-egress.nft");
const DEFAULT_DENY: &str = include_str!("../systemd/trustiot-agent-default-deny.nft");
const LAB: &str = include_str!("egress/lab.sh");
const CONTAINERFILE: &str = include_str!("egress/Containerfile");

const RULES_PATH: &str = "/usr/share/trustiot-agent/trustiot-agent-egress.nft";
const DEFAULT_DENY_PATH: &str = "/usr/share/trustiot-agent/trustiot-agent-default-deny.nft";
/// The line Laysi/trustiot-v2#866 adds to the egress rules.
const INCLUDE: &str = "include \"/usr/share/trustiot-agent/trustiot-agent-default-deny.nft\"\n";
const NFTABLES_DROP_IN: &str =
    "/usr/lib/systemd/system/nftables.service.d/trustiot-agent-egress.conf";
const AGENT: &str = "trustiot-agent.service";
const CHECK: &str = "trustiot-agent-egress-check.service";
const STATE: &str = "/var/lib/trustiot-agent";

/// The two devices on the peer, and a port there that accepts connections but is no device's.
const DEVICE_1: &str = "10.81.3.1";
const DEVICE_2: &str = "10.81.3.20";
const NOT_A_DEVICE_PORT: u16 = 503;
/// The site's DNS server of `lab.sh`, which `/etc/resolv.conf` names.
const NAMESERVER: &str = "10.81.3.1";
/// The gateway's identity; the cloud is never reached, but the agent checks its files at start.
const GATEWAY: &str = "gw-fieldbus-test";

/// The systemd container runs without an SELinux label, as in `tests/egress.rs`.
const SELINUX_OFF: &str = "label=disable";
/// How long systemd in the container may take to finish booting. It takes a second or two.
const BOOT_BOUND: Duration = Duration::from_secs(120);
/// How long the agent may take, once started, to poll a device: it polls every second and its
/// list takes milliseconds. Only catches a poll that never comes.
const POLL_BOUND: Duration = Duration::from_secs(120);
/// How long an automatic restart of the agent may take to come (`RestartSec=2s` for the first).
const RESTART_BOUND: Duration = Duration::from_secs(300);
/// How long the egress check may take to load flushed rules again: its timer runs every 10 s.
const CHECK_BOUND: Duration = Duration::from_secs(300);

/// The Modbus TCP device on the peer, one process per connection (socat's `fork` and `EXEC`):
/// answers every read of registers with the registers' numbers and appends
/// `<device address> <function code>` to `/run/modbus-requests` for each request. perl-base is
/// part of every Debian system.
const MODBUS_PEER: &str = r#"#!/usr/bin/perl
use strict;
use warnings;

my $device = $ENV{SOCAT_SOCKADDR} // 'unknown';
open(my $log, '>>', '/run/modbus-requests') or die "opening the request log: $!";

sub read_exactly {
    my ($wanted) = @_;
    my $buffer = '';
    while (length($buffer) < $wanted) {
        my $got = sysread(STDIN, $buffer, $wanted - length($buffer), length($buffer));
        return undef unless $got;
    }
    return $buffer;
}

while (defined(my $header = read_exactly(7))) {
    my ($transaction, $protocol, $length, $unit) = unpack('n n n C', $header);
    last if $length < 6 || $length > 254;
    my $pdu = read_exactly($length - 1);
    last unless defined $pdu;
    my ($function, $address, $count) = unpack('C n n', $pdu);
    last if $count < 1 || $count > 125;
    syswrite($log, "$device $function\n");
    my $data = join('', map { pack('n', ($address + $_) & 0xffff) } 0 .. $count - 1);
    my $answer = pack('C C', $function, length($data)) . $data;
    syswrite(STDOUT, pack('n n n C', $transaction, 0, length($answer) + 1, $unit) . $answer);
}
"#;

/// The site, at every boot before the agent: `lab network`, the second device's address, the
/// Modbus devices on port 502 and a listener on the port that is no device's.
const LAB_SITE: &str = r#"#!/bin/bash
set -euo pipefail
lab network
peer=(nsenter --net=/run/netns/peer)
"${peer[@]}" ip addr add 10.81.3.20/24 dev veth1
setsid "${peer[@]}" socat TCP4-LISTEN:502,fork,reuseaddr EXEC:/usr/local/bin/modbus-peer \
    < /dev/null > /dev/null 2>&1 &
setsid "${peer[@]}" socat TCP4-LISTEN:503,fork,reuseaddr EXEC:/bin/true \
    < /dev/null > /dev/null 2>&1 &
for i in $(seq 600); do
    "${peer[@]}" ss -H -tln 'sport = :502' | grep -q . \
        && "${peer[@]}" ss -H -tln 'sport = :503' | grep -q . && exit 0
    sleep 0.1
done
echo 'the devices did not listen within 60 s' >&2
exit 1
"#;

const LAB_SITE_UNIT: &str = "[Unit]\nDescription=The test site's network and devices\n\
    Before=trustiot-agent.service\n\n\
    [Service]\nType=oneshot\nRemainAfterExit=yes\nExecStart=/usr/local/bin/lab-site\n\n\
    [Install]\nWantedBy=multi-user.target\n";

/// The image of `tests/egress.rs`, built once per process and Containerfile content (with the
/// same tag, so the test files share it), from an empty build context.
fn image(runtime: &str) -> String {
    static BUILD: Mutex<()> = Mutex::new(());
    let _building = BUILD.lock().unwrap_or_else(PoisonError::into_inner);
    let digest = ring::digest::digest(&ring::digest::SHA256, CONTAINERFILE.as_bytes());
    let hash: String = digest
        .as_ref()
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let image = format!("localhost/agent-egress-test:{hash}");
    let exists = Command::new(runtime)
        .args(["image", "inspect", &image])
        .output()
        .is_ok_and(|output| output.status.success());
    if exists {
        return image;
    }
    let context = std::env::temp_dir().join(unique_name("fieldbus-context"));
    std::fs::create_dir_all(&context).unwrap();
    let containerfile = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/egress/Containerfile");
    let built = Command::new(runtime)
        .args(["build", "--quiet", "--file"])
        .arg(&containerfile)
        .args(["--tag", &image])
        .arg(&context)
        .output();
    let _ = std::fs::remove_dir(&context);
    let built = built.unwrap();
    assert!(
        built.status.success(),
        "building the egress test image failed: {}",
        String::from_utf8_lossy(&built.stderr)
    );
    image
}

/// What is installed besides the agent and its egress rules.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Wiring {
    /// The component, the default deny and the nameserver units, joined as
    /// Laysi/trustiot-v2#866 will.
    Joined,
    /// Main before Laysi/trustiot-v2#866: the egress rules as shipped and nothing else.
    Main,
}

/// A stand-in gateway: a throwaway container with systemd as PID 1, removed with its volumes
/// when dropped.
struct Lab {
    container: Container,
}

impl Lab {
    /// systemd as PID 1 with the units installed as the image installs them, the site up and the
    /// agent's identity in place; the agent is not started. `None` (and a printed reason) when
    /// the container cannot run systemd.
    fn new(wiring: Wiring) -> Option<Self> {
        let Some(runtime) = container_runtime() else {
            unavailable("podman or docker");
            return None;
        };
        if runtime != "podman" {
            eprintln!("skipped: systemd as PID 1 in a test container needs podman, not {runtime}");
            return None;
        }
        let image = image(runtime);
        let container = Container {
            runtime,
            name: unique_name("agent-fieldbus"),
        };
        let _id = runtime_output(
            runtime,
            &[
                "run",
                "--detach",
                "--name",
                &container.name,
                "--network",
                "none",
                "--memory",
                "512m",
                "--systemd",
                "always",
                "--cap-add",
                "NET_ADMIN,SYS_ADMIN",
                "--security-opt",
                SELINUX_OFF,
                &image,
                "/sbin/init",
            ],
        );
        let lab = Self { container };
        if !lab.booted() {
            return None;
        }
        lab.install(wiring);
        lab.ok("systemctl start lab-site.service");
        Some(lab)
    }

    /// Waits until systemd finished booting; false (and a printed reason) when it does not.
    fn booted(&self) -> bool {
        let started = Instant::now();
        loop {
            let state = self.sh("systemctl is-system-running");
            let state = String::from_utf8_lossy(&state.stdout).trim().to_owned();
            if state == "running" || state == "degraded" {
                return true;
            }
            if started.elapsed() > BOOT_BOUND {
                eprintln!(
                    "skipped: systemd in the container did not finish booting within \
                     {BOOT_BOUND:?} (state {state:?})"
                );
                return false;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    fn install(&self, wiring: Wiring) {
        self.copy(AGENT_EXECUTABLE, "/usr/bin/trustiot-agent");
        self.put("/usr/local/bin/lab", LAB);
        self.put("/usr/local/bin/lab-site", LAB_SITE);
        self.put("/usr/local/bin/modbus-peer", MODBUS_PEER);
        self.put("/etc/systemd/system/lab-site.service", LAB_SITE_UNIT);
        self.put("/usr/lib/sysusers.d/trustiot-agent.conf", SYSUSERS);
        self.put("/usr/lib/systemd/system/trustiot-agent.service", AGENT_UNIT);
        self.put(
            "/usr/lib/systemd/system/trustiot-agent-egress.service",
            EGRESS_UNIT,
        );
        self.put(
            "/usr/lib/systemd/system/trustiot-agent-egress-check.service",
            CHECK_UNIT,
        );
        self.put(
            "/usr/lib/systemd/system/trustiot-agent-egress-check.timer",
            CHECK_TIMER,
        );
        match wiring {
            Wiring::Main => {
                self.put(RULES_PATH, RULES);
                self.put(NFTABLES_DROP_IN, NFTABLES_RELOAD);
            }
            Wiring::Joined => {
                self.put(RULES_PATH, &format!("{RULES}\n{INCLUDE}"));
                self.put(DEFAULT_DENY_PATH, DEFAULT_DENY);
                self.copy(ALLOWLIST_EXECUTABLE, ALLOWLIST_INSTALLED);
                self.copy(NAMESERVERS_EXECUTABLE, NAMESERVERS_INSTALLED);
                self.put(
                    "/usr/lib/systemd/system/trustiot-agent-allowlist.socket",
                    ALLOWLIST_SOCKET,
                );
                self.put(
                    "/usr/lib/systemd/system/trustiot-agent-allowlist@.service",
                    ALLOWLIST_SERVICE,
                );
                self.put(
                    "/usr/lib/systemd/system/trustiot-agent-nameservers.service",
                    NAMESERVERS_SERVICE,
                );
                self.put(
                    "/usr/lib/systemd/system/trustiot-agent-nameservers.path",
                    NAMESERVERS_PATH,
                );
                // The drop-in as shipped, with the line Laysi/trustiot-v2#866 adds.
                self.put(
                    NFTABLES_DROP_IN,
                    &format!("{NFTABLES_RELOAD}ExecReload={NAMESERVERS_INSTALLED}\n"),
                );
                // What Laysi/trustiot-v2#866 adds to the agent's unit (README.md, "給
                // Laysi/trustiot-v2#866 的接法"), as a drop-in here: the path unit and the
                // component's socket from before the agent starts, and the nameservers after
                // every load of the rules by the agent's unit (its ExecStartPre=). After the check
                // loads flushed rules it puts the nameservers back itself.
                self.put(
                    "/etc/systemd/system/trustiot-agent.service.d/default-deny.conf",
                    &format!(
                        "[Unit]\n\
                         Wants=trustiot-agent-nameservers.path trustiot-agent-allowlist.socket\n\
                         After=trustiot-agent-nameservers.path trustiot-agent-allowlist.socket\n\
                         [Service]\n\
                         ExecStartPre=+{NAMESERVERS_INSTALLED}\n"
                    ),
                );
            }
        }
        self.ok(&format!(
            "chmod 0755 /usr/local/bin/lab /usr/local/bin/lab-site /usr/local/bin/modbus-peer \
             && groupadd --system tss && systemd-sysusers \
             && install -d -o trustiot-agent -g trustiot-agent -m 0700 {STATE} \
             && systemctl daemon-reload"
        ));
        self.identity();
    }

    /// The agent's configuration file and identity under /etc/trustiot. The cloud is the peer's
    /// port 443, which closes every connection: the agent keeps retrying and polls meanwhile.
    fn identity(&self) {
        let pki = TestPki::new();
        let leaf = pki.gateway(GATEWAY);
        self.put("/etc/trustiot/public-ca.pem", &pki.public_ca.pem);
        self.put("/etc/trustiot/broker-ca.pem", &pki.broker_ca.pem);
        self.put("/etc/trustiot/gateway.pem", &leaf.cert_pem);
        self.put("/etc/trustiot/gateway.key", &leaf.key_pem);
        self.put(
            "/etc/trustiot/agent.toml",
            "[cloud]\nurl = \"wss://cloud.test/mqtt\"\ntrust = [\"/etc/trustiot/public-ca.pem\"]\n\n\
             [broker]\nca = \"/etc/trustiot/broker-ca.pem\"\n\n\
             [identity]\ncertificate = \"/etc/trustiot/gateway.pem\"\n\
             key = { source = \"file\", path = \"/etc/trustiot/gateway.key\" }\n",
        );
        self.ok("chmod 0644 /etc/trustiot/*.pem /etc/trustiot/agent.toml \
             && chown trustiot-agent:trustiot-agent /etc/trustiot/gateway.key \
             && chmod 0600 /etc/trustiot/gateway.key");
    }

    /// Saves `devices` (name, address; Modbus TCP on port 502) as the agent's applied
    /// configuration, as the agent saves one it received.
    fn save_configuration(&self, version: u64, devices: &[(&str, &str)]) {
        let model = DeviceModel {
            name: ModelName::new("meter").unwrap(),
            points: vec![DataPoint {
                key: PointKey::new("power").unwrap(),
                function_code: FunctionCode::ReadHoldingRegisters,
                address: 0,
                data_type: DataType::U16,
                byte_order: ByteOrder::Abcd,
                length: None,
                scale: None,
                offset: None,
            }],
        };
        let config = DesiredConfig {
            v: V1,
            rev: 1,
            version: ConfigVersion::new(version).unwrap(),
            devices: devices
                .iter()
                .map(|(id, host)| Device {
                    id: DeviceId::new(*id).unwrap(),
                    model: model.name.clone(),
                    transport: Transport::ModbusTcp,
                    host: Host::new(*host).unwrap(),
                    port: 502,
                    unit_id: 1,
                    poll_interval_ms: 1000,
                })
                .collect(),
            models: vec![model],
        }
        .encode()
        .unwrap();
        let path = format!("{STATE}/desired-config.cbor");
        self.put_bytes(&path, &config);
        self.ok(&format!(
            "chown trustiot-agent:trustiot-agent {path} && chmod 0600 {path}"
        ));
    }

    fn copy(&self, built: &str, installed: &str) {
        let parent = Path::new(installed).parent().unwrap().display().to_string();
        self.ok(&format!("mkdir -p {parent}"));
        let copied = Command::new(self.container.runtime)
            .arg("cp")
            .arg(built)
            .arg(format!("{}:{installed}", self.container.name))
            .output()
            .unwrap();
        assert!(
            copied.status.success(),
            "copying {built}: {}",
            String::from_utf8_lossy(&copied.stderr)
        );
        self.ok(&format!(
            "chown root:root {installed} && chmod 0755 {installed}"
        ));
    }

    fn put(&self, path: &str, contents: &str) {
        self.put_bytes(path, contents.as_bytes());
    }

    fn put_bytes(&self, path: &str, contents: &[u8]) {
        let parent = Path::new(path).parent().unwrap().display().to_string();
        let mut child = Command::new(self.container.runtime)
            .args(["exec", "--interactive", &self.container.name, "sh", "-c"])
            .arg(format!("mkdir -p '{parent}' && cat > '{path}'"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(contents).unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "writing {path}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// Runs a bash script in the container.
    fn sh(&self, script: &str) -> Output {
        Command::new(self.container.runtime)
            .args(["exec", &self.container.name, "bash", "-c", script])
            .env("LC_ALL", "C")
            .output()
            .unwrap()
    }

    /// Runs a bash script that must succeed; its trimmed stdout.
    fn ok(&self, script: &str) -> String {
        let output = self.sh(script);
        assert!(
            output.status.success(),
            "{script} failed ({}):\n{}{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    fn property(&self, unit: &str, name: &str) -> String {
        self.ok(&format!("systemctl show --property={name} --value {unit}"))
    }

    /// Restarts the container: a reboot of the gateway, with an empty ruleset and a new /run.
    fn reboot(&self) -> bool {
        let _ = runtime_output(self.container.runtime, &["restart", &self.container.name]);
        self.booted()
    }

    /// `lab probe`: ok, refused, not-permitted, timeout or the error.
    fn probe(&self, who: &str, address: &str, port: u16) -> String {
        self.ok(&format!("lab probe {who} tcp {address} {port}"))
    }

    /// The requests the device at `address` has answered since boot.
    fn requests(&self, address: &str) -> usize {
        self.ok("cat /run/modbus-requests 2> /dev/null || true")
            .lines()
            .filter(|line| line.split_whitespace().next() == Some(address))
            .count()
    }

    /// Waits until the device at `address` answers a request it had not answered yet.
    fn await_poll(&self, address: &str, what: &str) {
        let before = self.requests(address);
        await_until(what, POLL_BOUND, || self.requests(address) > before);
    }

    /// The elements of one of the default deny's sets, as nft writes them.
    fn elements(&self, set: &str) -> BTreeSet<String> {
        elements(&self.ok(&format!("nft list set inet {TABLE} {set}")))
    }

    fn fieldbus(&self) -> BTreeSet<String> {
        self.elements(SET_V4)
    }

    /// Fails when the default deny refused anything of the agent's account since its rules were
    /// last loaded, with the rules' counters and the agent's journal.
    fn assert_nothing_refused(&self, when: &str) {
        let refused = self.refused();
        assert_eq!(
            refused,
            0,
            "{when}: the default deny refused {refused} packets of the agent's account (did it \
             connect before its list?)\n{}\n{}",
            self.ok(&format!("nft list table inet {TABLE}")),
            self.ok(&format!(
                "journalctl --boot --no-pager --unit={AGENT} | tail -n 60"
            ))
        );
    }

    /// Packets the default deny refused since its rules were last loaded.
    fn refused(&self) -> u64 {
        let chain = self.ok(&format!("nft list chain inet {TABLE} agent"));
        let line = chain
            .lines()
            .find(|line| line.contains("comment \"default deny\""))
            .unwrap_or_else(|| panic!("no default deny rule in {chain}"));
        let (_, after) = line.split_once("packets ").unwrap();
        after.split_whitespace().next().unwrap().parse().unwrap()
    }

    /// The monotonic timestamps (microseconds) of this boot's journal entries that match
    /// `filter` and whose message contains `text`.
    fn journal_times(&self, filter: &str, text: &str) -> Vec<u64> {
        self.ok(&format!(
            "journalctl --boot --no-pager --quiet --output=json {filter}"
        ))
        .lines()
        .filter_map(|line| {
            let entry: serde_json::Value = serde_json::from_str(line).unwrap();
            let message = entry["MESSAGE"].as_str()?;
            message.contains(text).then(|| {
                entry["__MONOTONIC_TIMESTAMP"]
                    .as_str()
                    .unwrap()
                    .parse()
                    .unwrap()
            })
        })
        .collect()
    }

    /// The agent's journal lines that contain `text`, this boot.
    fn agent_logged(&self, text: &str) -> usize {
        self.journal_times(&format!("_SYSTEMD_UNIT={AGENT}"), text)
            .len()
    }

    /// Writes a marker to the journal; its monotonic timestamp.
    fn mark(&self, marker: &str) -> u64 {
        self.ok(&format!(
            "echo {marker} | systemd-cat --identifier=fieldbus-test"
        ));
        let times = self.journal_times("SYSLOG_IDENTIFIER=fieldbus-test", marker);
        assert_eq!(times.len(), 1, "{marker}: {times:?}");
        times[0]
    }

    /// Starts listing the fieldbus set in a loop, until [`Lab::stop_sampling`].
    fn start_sampling(&self) {
        self.ok(&format!(
            "rm -f /run/samples /run/sampling-stop /run/sampling-done; \
             setsid bash -c 'while [ ! -e /run/sampling-stop ]; do \
             nft list set inet {TABLE} {SET_V4} 2>&1; echo ===; done > /run/samples; \
             touch /run/sampling-done' < /dev/null > /dev/null 2>&1 &"
        ));
    }

    /// Every listing since [`Lab::start_sampling`], as sets of elements.
    fn stop_sampling(&self) -> Vec<BTreeSet<String>> {
        self.ok("touch /run/sampling-stop");
        await_until("the sampler to stop", POLL_BOUND, || {
            self.sh("test -e /run/sampling-done").status.success()
        });
        self.ok("cat /run/samples")
            .split("===")
            .map(str::trim)
            .filter(|sample| !sample.is_empty())
            .map(|sample| {
                assert!(
                    sample.contains(&format!("set {SET_V4}")),
                    "a listing of the set failed (the table was gone): {sample}"
                );
                elements(sample)
            })
            .collect()
    }
}

/// The elements in `nft list set` output; empty when the set has none.
fn elements(listed: &str) -> BTreeSet<String> {
    let flat = listed.split_whitespace().collect::<Vec<_>>().join(" ");
    let Some((_, elements)) = flat.split_once("elements = { ") else {
        return BTreeSet::new();
    };
    let (elements, _) = elements.split_once(" }").unwrap();
    elements
        .split(", ")
        .map(|element| element.trim().to_owned())
        .collect()
}

fn set(elements: &[&str]) -> BTreeSet<String> {
    elements
        .iter()
        .map(|element| (*element).to_owned())
        .collect()
}

/// The fieldbus set's elements for devices at `addresses` on port 502.
fn destinations(addresses: &[&str]) -> BTreeSet<String> {
    addresses
        .iter()
        .map(|address| format!("{address} . 502"))
        .collect()
}

/// Polls `done` every 200 ms until it holds; fails naming `what` after `bound`.
fn await_until(what: &str, bound: Duration, mut done: impl FnMut() -> bool) {
    let started = Instant::now();
    while !done() {
        assert!(started.elapsed() < bound, "{what}: not within {bound:?}");
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Laysi/trustiot-v2#865 with the component and the default deny joined (acceptance criteria 1
/// to 4 and 6): the fieldbus set holds exactly the destinations of the agent's devices, and the
/// agent connects to none before the list with it is applied, at its start, after devices are
/// added and removed, after a reboot, after a flush and a crash, after a reload of
/// nftables.service and after a flush while it runs.
#[test]
fn the_agent_keeps_the_fieldbus_sets_to_its_devices() {
    let Some(lab) = Lab::new(Wiring::Joined) else {
        return;
    };

    // The start: the list before any poll.
    lab.save_configuration(1, &[("meter-1", DEVICE_1)]);
    lab.ok(&format!("systemctl start {AGENT}"));
    lab.await_poll(DEVICE_1, "the first poll of meter-1");
    assert_eq!(lab.fieldbus(), destinations(&[DEVICE_1]));
    lab.assert_nothing_refused("at the start");
    assert_eq!(lab.agent_logged("applied the fieldbus allowlist"), 1);
    assert_eq!(lab.probe("agent", DEVICE_1, 502), "ok");
    assert_eq!(lab.probe("agent", DEVICE_1, NOT_A_DEVICE_PORT), "refused");
    assert_eq!(lab.probe("agent", DEVICE_2, 502), "refused");
    // The control: the same port is open to another account, so the refusal is the set's.
    assert_eq!(lab.probe("root", DEVICE_1, NOT_A_DEVICE_PORT), "ok");
    assert_eq!(lab.probe("root", DEVICE_2, 502), "ok");
    eprintln!(
        "evidence: at start the set is {:?}; the agent's account reaches only that destination",
        lab.fieldbus()
    );

    // A device added: its destination is listed before the agent connects to it, the other
    // device stays listed and polled.
    lab.save_configuration(2, &[("meter-1", DEVICE_1), ("meter-2", DEVICE_2)]);
    lab.ok(&format!("systemctl restart {AGENT}"));
    lab.await_poll(DEVICE_2, "the first poll of the added meter-2");
    lab.await_poll(DEVICE_1, "meter-1 polled with meter-2 added");
    assert_eq!(lab.fieldbus(), destinations(&[DEVICE_1, DEVICE_2]));
    lab.assert_nothing_refused("after a device was added");
    assert_eq!(lab.probe("agent", DEVICE_2, 502), "ok");
    eprintln!("evidence: meter-2 added: the set is {:?}", lab.fieldbus());

    // A device removed: its destination leaves the set and is refused again.
    lab.save_configuration(3, &[("meter-2", DEVICE_2)]);
    lab.ok(&format!("systemctl restart {AGENT}"));
    await_until("meter-1's destination leaving the set", POLL_BOUND, || {
        lab.fieldbus() == destinations(&[DEVICE_2])
    });
    lab.await_poll(DEVICE_2, "meter-2 polled with meter-1 removed");
    lab.assert_nothing_refused("after a device was removed");
    assert_eq!(lab.probe("agent", DEVICE_1, 502), "refused");
    assert_eq!(
        lab.refused(),
        1,
        "the probe was refused by the default deny"
    );
    eprintln!("evidence: meter-1 removed: the set is {:?}", lab.fieldbus());

    // A reboot: an empty ruleset, then nftables.service, the rules with the default deny and
    // empty sets, the nameservers, and the agent with its saved configuration, whose list comes
    // before its first poll.
    lab.ok(&format!("systemctl enable {AGENT} lab-site.service"));
    assert!(
        lab.reboot(),
        "systemd did not come up again after the reboot"
    );
    lab.await_poll(DEVICE_2, "the first poll after the reboot");
    assert_eq!(lab.fieldbus(), destinations(&[DEVICE_2]));
    lab.assert_nothing_refused("after the reboot");
    assert_eq!(lab.elements(nameservers::SET_V4), set(&[NAMESERVER]));
    assert_eq!(lab.agent_logged("applied the fieldbus allowlist"), 1);
    eprintln!(
        "evidence: after a reboot the set is {:?} before the first poll",
        lab.fieldbus()
    );

    // `nft flush ruleset` and then the agent's process ends: the automatic restart loads the
    // rules with the default deny and the nameservers, and the agent lists its devices before it
    // polls them.
    let restarts = || -> u64 { lab.property(AGENT, "NRestarts").parse().unwrap() };
    let before = restarts();
    lab.ok(&format!(
        "nft flush ruleset && kill -KILL \"$(systemctl show --property=MainPID --value {AGENT})\""
    ));
    await_until("the agent's automatic restart", RESTART_BOUND, || {
        restarts() > before && lab.property(AGENT, "ActiveState") == "active"
    });
    lab.await_poll(DEVICE_2, "the first poll after the restart");
    assert_eq!(lab.fieldbus(), destinations(&[DEVICE_2]));
    assert_eq!(lab.elements(nameservers::SET_V4), set(&[NAMESERVER]));
    lab.assert_nothing_refused("after the flush and the crash");
    eprintln!(
        "evidence: after nft flush ruleset and a crash the set is {:?}",
        lab.fieldbus()
    );

    // A reload of nftables.service empties the sets in its one transaction (the `flush ruleset`
    // of /etc/nftables.conf) while the agent keeps running; the agent, reloaded after it, sends
    // its list again. Meanwhile the set holds nothing beyond the list.
    let invocation = lab.property(AGENT, "InvocationID");
    let resends = lab.agent_logged("sending the fieldbus allowlist again");
    lab.start_sampling();
    let reloading = lab.mark("reloading-nftables");
    lab.ok("systemctl reload nftables.service");
    await_until("the list in the set again", POLL_BOUND, || {
        lab.fieldbus() == destinations(&[DEVICE_2])
    });
    lab.await_poll(DEVICE_2, "a poll after the reload of nftables.service");
    let samples = lab.stop_sampling();
    assert_eq!(lab.property(AGENT, "InvocationID"), invocation, "restarted");
    assert_eq!(
        lab.agent_logged("sending the fieldbus allowlist again"),
        resends + 1
    );
    let listed = destinations(&[DEVICE_2]);
    assert!(
        samples.iter().all(|sample| sample.is_subset(&listed)),
        "the set held something beyond the list: {samples:?}"
    );
    let applied = lab
        .journal_times(
            &format!("_SYSTEMD_UNIT={AGENT}"),
            "applied the fieldbus allowlist",
        )
        .into_iter()
        .filter(|time| *time > reloading)
        .min()
        .unwrap();
    eprintln!(
        "evidence: systemctl reload nftables.service: the agent was not restarted; {} of {} \
         listings of the set during the reload were empty, none held more than the list; the \
         agent's list was applied again {:?} after the reload began; polling resumed",
        samples.iter().filter(|sample| sample.is_empty()).count(),
        samples.len(),
        Duration::from_micros(applied - reloading)
    );
    assert_eq!(lab.probe("agent", DEVICE_1, 502), "refused");
    assert_eq!(lab.elements(nameservers::SET_V4), set(&[NAMESERVER]));

    // `nft flush ruleset` while the agent runs: the check loads the rules again and reloads the
    // agent (not a restart), which sends its list again; the check puts the nameservers back
    // itself before that reload.
    let flushed = lab.mark("flushing");
    lab.ok("nft flush ruleset");
    await_until(
        "the check loading the rules and the agent's list in the set again",
        CHECK_BOUND,
        || {
            lab.sh(&format!("nft list table inet {TABLE} > /dev/null 2>&1"))
                .status
                .success()
                && lab.fieldbus() == listed
                && lab.elements(nameservers::SET_V4) == set(&[NAMESERVER])
        },
    );
    lab.await_poll(DEVICE_2, "a poll after the check");
    assert_eq!(lab.property(AGENT, "InvocationID"), invocation, "restarted");
    let applied = lab
        .journal_times(
            &format!("_SYSTEMD_UNIT={AGENT}"),
            "applied the fieldbus allowlist",
        )
        .into_iter()
        .filter(|time| *time > flushed)
        .min()
        .unwrap();
    eprintln!(
        "evidence: nft flush ruleset while the agent runs: the check loaded the rules and \
         reloaded the agent, whose list was applied {:?} after the flush; polling resumed",
        Duration::from_micros(applied - flushed)
    );
    assert_eq!(lab.probe("agent", DEVICE_1, 502), "refused");

    // Only the default deny's table deleted, the egress table still there: the rules file
    // includes the default deny, so the check asks for its table too and does the same.
    let deleted = lab.mark("deleting-the-default-deny");
    lab.ok(&format!("nft delete table inet {TABLE}"));
    await_until(
        "the check loading the default deny and the agent's list in the set again",
        CHECK_BOUND,
        || {
            lab.sh(&format!("nft list table inet {TABLE} > /dev/null 2>&1"))
                .status
                .success()
                && lab.fieldbus() == listed
                && lab.elements(nameservers::SET_V4) == set(&[NAMESERVER])
        },
    );
    lab.await_poll(DEVICE_2, "a poll after the check");
    assert_eq!(lab.property(AGENT, "InvocationID"), invocation, "restarted");
    let noticed: Vec<u64> = lab
        .journal_times(
            &format!("_SYSTEMD_UNIT={CHECK}"),
            &format!("table inet {TABLE} is missing"),
        )
        .into_iter()
        .filter(|time| *time > deleted)
        .collect();
    assert_eq!(noticed.len(), 1, "{noticed:?}");
    eprintln!(
        "evidence: nft delete table inet {TABLE}: the check noticed it {:?} later, loaded the \
         rules and reloaded the agent; the set is {:?}",
        Duration::from_micros(noticed[0] - deleted),
        lab.fieldbus()
    );
}

/// Laysi/trustiot-v2#865, acceptance criterion 7: without the component (main before
/// Laysi/trustiot-v2#866, the image as it is) the agent polls as before and sends no list, and
/// a reload of its unit changes nothing but a line in its journal.
#[test]
fn before_the_component_the_agent_polls_as_before() {
    let Some(lab) = Lab::new(Wiring::Main) else {
        return;
    };
    lab.save_configuration(1, &[("meter-1", DEVICE_1)]);
    lab.ok(&format!("systemctl start {AGENT}"));
    lab.await_poll(DEVICE_1, "the first poll of meter-1");
    assert_eq!(
        lab.agent_logged("the fieldbus allowlist component is not installed"),
        1
    );
    assert_eq!(lab.agent_logged("applied the fieldbus allowlist"), 0);
    assert!(
        !lab.sh(&format!("nft list table inet {TABLE}"))
            .status
            .success(),
        "a default deny table exists"
    );
    // Without the default deny the agent's account reaches any device, as before.
    assert_eq!(lab.probe("agent", DEVICE_2, 502), "ok");

    // The check does not ask for the default deny's table, which the rules file does not include:
    // two runs of it pass silently and reload nothing.
    let invocation = lab.property(AGENT, "InvocationID");
    for run in ["first", "second"] {
        let before = lab.property(CHECK, "ExecMainExitTimestampMonotonic");
        await_until(&format!("a {run} run of the check"), CHECK_BOUND, || {
            let after = lab.property(CHECK, "ExecMainExitTimestampMonotonic");
            after != before && after != "0"
        });
        assert_eq!(lab.property(CHECK, "Result"), "success");
    }
    assert_eq!(
        lab.ok(&format!("journalctl --no-pager --quiet --unit={CHECK}")),
        "",
        "the check wrote to the journal"
    );
    assert_eq!(
        lab.agent_logged("reloaded"),
        0,
        "the check reloaded the agent"
    );

    // A reload of nftables.service reaches the agent as a reload, which has no list to send.
    lab.ok("systemctl reload nftables.service");
    await_until("the reload reaching the agent", POLL_BOUND, || {
        lab.agent_logged("there is no list to send") == 1
    });
    lab.await_poll(DEVICE_1, "a poll after the reload");
    assert_eq!(lab.property(AGENT, "InvocationID"), invocation, "restarted");
    assert_eq!(lab.property(AGENT, "ActiveState"), "active");
    eprintln!(
        "evidence: without the component the agent polls, logs that it sends no list, and a \
         reload of nftables.service leaves it running"
    );
}
