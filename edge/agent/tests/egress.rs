//! The kernel-level egress rules for the agent's account (Laysi/trustiot-v2#813,
//! `systemd/trustiot-agent-egress.nft`), the unit that loads them before the agent starts
//! (`systemd/trustiot-agent-egress.service`), the gateway check (`scripts/check-gateway.sh`), and
//! what holds when root flushes the rules while the agent runs (Laysi/trustiot-v2#878): the agent
//! unit's `IPAddressDeny=` and the timer that loads the rules again
//! (`systemd/trustiot-agent-egress-check.{service,timer}`).
//!
//! No gateway is needed: a throwaway Debian 13 container (the gateway's Raspberry Pi OS is
//! Debian 13, with the same nftables and systemd; `tests/egress/Containerfile`) stands in for it.
//! Its network namespace is the gateway's, with a peer namespace behind a veth pair as the site's
//! LAN, DNS server and cloud (`tests/egress/lab.sh`). The probes run as the real account from
//! `systemd/trustiot-agent.sysusers` and as two others (`nobody` and root), with socat and glibc's
//! resolver. Every destination a probe uses has a listener, and the other accounts reach each one,
//! so a refusal of the agent's account is the rules' doing; the rules' counters confirm it.
//!
//! The container needs podman or docker (a skip locally, a failure with `CI` set). The systemd
//! test needs podman, which runs systemd as PID 1; when systemd does not come up in the container
//! it skips, also with `CI` set, like `src/modbus/address.rs`'s sandbox test: the hosted runners
//! are not known to run systemd containers, and the unit files are checked everywhere by
//! `tests/systemd_unit.rs`.

#![allow(
    clippy::unwrap_used,
    clippy::print_stderr,
    reason = "test code; a skipped test says why on stderr"
)]

use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use testkit::container::{Container, container_runtime, runtime_output, unavailable, unique_name};

const RULES: &str = include_str!("../systemd/trustiot-agent-egress.nft");
const EGRESS_UNIT: &str = include_str!("../systemd/trustiot-agent-egress.service");
const AGENT_UNIT: &str = include_str!("../systemd/trustiot-agent.service");
const SYSUSERS: &str = include_str!("../systemd/trustiot-agent.sysusers");
const CHECK_GATEWAY: &str = include_str!("../scripts/check-gateway.sh");
const LAB: &str = include_str!("egress/lab.sh");
const CONTAINERFILE: &str = include_str!("egress/Containerfile");

/// Where the image installs the rules (`ExecStart=` of the egress unit).
const RULES_PATH: &str = "/usr/share/trustiot-agent/trustiot-agent-egress.nft";
/// The drop-in for nftables.service, and where the image installs it.
const NFTABLES_RELOAD: &str = include_str!("../systemd/nftables-reload.conf");
const NFTABLES_DROP_IN: &str =
    "/usr/lib/systemd/system/nftables.service.d/trustiot-agent-egress.conf";
/// The check that loads the rules again when they vanish while the agent runs, and its timer
/// (Laysi/trustiot-v2#878).
const CHECK_UNIT: &str = include_str!("../systemd/trustiot-agent-egress-check.service");
const CHECK_TIMER: &str = include_str!("../systemd/trustiot-agent-egress-check.timer");
const CHECK: &str = "trustiot-agent-egress-check.service";
const TIMER: &str = "trustiot-agent-egress-check.timer";

/// The systemd container runs without an SELinux label: on a host with SELinux enforcing (Fedora,
/// for example) a dependency of trustiot-agent.service fails to start in a labelled container.
/// Without SELinux the option changes nothing; the container stays rootless and in its own network
/// namespace.
const SELINUX_OFF: &str = "label=disable";

/// How long systemd in the container may take to finish booting. It takes a second or two.
const BOOT_BOUND: Duration = Duration::from_secs(120);

/// The image, built once per process and Containerfile content, from an empty build context.
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
    let context = std::env::temp_dir().join(unique_name("egress-context"));
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

/// A stand-in gateway: a throwaway container, removed with its volumes when dropped.
struct Lab {
    container: Container,
}

impl Lab {
    fn run(runtime: &'static str, args: &[&str]) -> Self {
        let image = image(runtime);
        let container = Container {
            runtime,
            name: unique_name("agent-egress"),
        };
        let mut all = vec!["run", "--detach", "--name", &container.name];
        all.extend_from_slice(args);
        let image_at = all.iter().position(|arg| *arg == "IMAGE").unwrap();
        all[image_at] = &image;
        let _id = runtime_output(runtime, &all);
        Self { container }
    }

    /// The gateway's network namespace without systemd: `lab network`, the agent's account, and
    /// the rules not yet loaded.
    fn network() -> Option<Self> {
        let Some(runtime) = container_runtime() else {
            unavailable("podman or docker");
            return None;
        };
        // NET_ADMIN for nft and the links, SYS_ADMIN for the peer namespace (both only over the
        // container's own namespaces with rootless podman), SETUID and SETGID to switch accounts,
        // NET_BIND_SERVICE for the listeners on 53, 80 and 443, CHOWN, FOWNER and DAC_OVERRIDE
        // for systemd-sysusers (it copies the owner and mode of /etc/gshadow to its new file).
        // IPv6 forwarding on (only in the container's namespace), as on a gateway that shares its
        // connection: the kernel then adds subnet-router anycast addresses the rules must refuse.
        let lab = Self::run(
            runtime,
            &[
                "--network",
                "none",
                "--sysctl",
                "net.ipv6.conf.all.forwarding=1",
                "--sysctl",
                "net.ipv6.conf.default.forwarding=1",
                "--memory",
                "512m",
                "--cap-drop",
                "ALL",
                "--cap-add",
                "NET_ADMIN,SYS_ADMIN,SETUID,SETGID,NET_BIND_SERVICE,CHOWN,FOWNER,DAC_OVERRIDE",
                "--security-opt",
                "no-new-privileges",
                "--entrypoint",
                "/bin/sleep",
                "IMAGE",
                "infinity",
            ],
        );
        lab.install();
        lab.ok("lab network");
        Some(lab)
    }

    /// Writes the repository's files where the image puts them, and the helper.
    fn install(&self) {
        self.put(RULES_PATH, RULES);
        self.put("/usr/lib/sysusers.d/trustiot-agent.conf", SYSUSERS);
        self.put("/usr/local/bin/check-gateway", CHECK_GATEWAY);
        self.put("/usr/local/bin/lab", LAB);
        self.ok("chmod 0755 /usr/local/bin/check-gateway /usr/local/bin/lab");
    }

    fn put(&self, path: &str, contents: &str) {
        let parent = Path::new(path).parent().unwrap().display().to_string();
        let mut child = Command::new(self.container.runtime)
            .args(["exec", "--interactive", &self.container.name, "sh", "-c"])
            .arg(format!("mkdir -p '{parent}' && cat > '{path}'"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(contents.as_bytes())
            .unwrap();
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

    fn probe(&self, who: &str, proto: &str, address: &str, port: u16) -> String {
        self.ok(&format!("lab probe {who} {proto} {address} {port}"))
    }

    /// The packets each refusal rule counted, by its comment.
    fn counters(&self) -> BTreeMap<String, u64> {
        self.ok("lab counters")
            .lines()
            .map(|line| {
                let (name, packets) = line.rsplit_once(' ').unwrap();
                (name.to_owned(), packets.parse().unwrap())
            })
            .collect()
    }
}

/// A destination of the probes and the rule that must refuse it for the agent's account.
struct Destination {
    what: &'static str,
    proto: &'static str,
    address: &'static str,
    port: u16,
    rule: &'static str,
}

/// Every kind of destination the rules refuse, each with a listener (or, for UDP, a route), as
/// `tests/egress/lab.sh` sets them up.
const REFUSED: [Destination; 18] = [
    Destination {
        what: "loopback",
        proto: "tcp",
        address: "127.0.0.1",
        port: 8080,
        rule: "local",
    },
    Destination {
        what: "another loopback address",
        proto: "tcp",
        address: "127.0.0.2",
        port: 8080,
        rule: "local",
    },
    Destination {
        what: "IPv6 loopback",
        proto: "tcp",
        address: "::1",
        port: 8080,
        rule: "local",
    },
    Destination {
        what: "the unspecified address, which the kernel sends to loopback",
        proto: "tcp",
        address: "0.0.0.0",
        port: 8080,
        rule: "local",
    },
    Destination {
        what: "the gateway's own LAN address",
        proto: "tcp",
        address: "10.81.3.2",
        port: 8080,
        rule: "local",
    },
    Destination {
        what: "the gateway's own IPv6 LAN address",
        proto: "tcp",
        address: "fd81:3::2",
        port: 8080,
        rule: "local",
    },
    Destination {
        what: "the gateway's own link-local address",
        proto: "tcp",
        address: "fe80::81:2%veth0",
        port: 8080,
        rule: "local",
    },
    Destination {
        what: "a UDP datagram to the gateway's own link-local address",
        proto: "udp",
        address: "fe80::81:2%veth0",
        port: 9,
        rule: "local",
    },
    Destination {
        what: "the LAN prefix's subnet-router anycast address, which the gateway answers itself",
        proto: "udp",
        address: "fd81:3::",
        port: 9,
        rule: "anycast",
    },
    Destination {
        what: "an address local only through `ip route add local`",
        proto: "tcp",
        address: "198.51.100.7",
        port: 8080,
        rule: "local",
    },
    Destination {
        what: "an outside address a NAT rule sends back to the gateway",
        proto: "tcp",
        address: "203.0.113.80",
        port: 80,
        rule: "local",
    },
    Destination {
        what: "a UDP datagram to loopback",
        proto: "udp",
        address: "127.0.0.1",
        port: 9,
        rule: "local",
    },
    Destination {
        what: "the LAN's broadcast address",
        proto: "udp",
        address: "10.81.3.255",
        port: 9,
        rule: "broadcast",
    },
    Destination {
        what: "the limited broadcast address",
        proto: "udp",
        address: "255.255.255.255",
        port: 9,
        rule: "broadcast",
    },
    Destination {
        what: "an IPv4 multicast group",
        proto: "udp",
        address: "239.1.2.3",
        port: 9,
        rule: "multicast",
    },
    Destination {
        what: "an IPv6 multicast group",
        proto: "udp",
        address: "ff05::1:3",
        port: 9,
        rule: "multicast",
    },
    Destination {
        what: "the IPv4 metadata address",
        proto: "tcp",
        address: "169.254.169.254",
        port: 80,
        rule: "metadata",
    },
    Destination {
        what: "the IPv6 metadata address",
        proto: "tcp",
        address: "fd00:ec2::254",
        port: 80,
        rule: "metadata",
    },
];

/// What a refused probe reports: a TCP reset, or the send failing at once.
fn refusal(proto: &str) -> &'static str {
    if proto == "tcp" {
        "refused"
    } else {
        "not-permitted"
    }
}

#[test]
fn the_agents_account_is_refused_the_gateway_broadcast_multicast_and_metadata() {
    let Some(lab) = Lab::network() else {
        return;
    };

    // The rules name the account, so they do not load before it exists (fail closed).
    let before = lab.sh(&format!("nft -f {RULES_PATH}"));
    assert!(
        !before.status.success(),
        "the rules loaded without the account"
    );
    assert!(
        String::from_utf8_lossy(&before.stderr).contains("trustiot-agent"),
        "{}",
        String::from_utf8_lossy(&before.stderr)
    );
    assert!(lab.sh("nft list table inet trustiot_agent").status.code() != Some(0));

    lab.ok("systemd-sysusers");
    lab.ok(&format!("nft -f {RULES_PATH}"));
    // A second load replaces the table: still one table with one set of rules.
    lab.ok(&format!("nft -f {RULES_PATH}"));
    assert_eq!(
        lab.ok("nft list tables | grep -c 'table inet trustiot_agent'"),
        "1"
    );
    assert_eq!(
        lab.ok("nft list chain inet trustiot_agent agent | grep -c 'goto refuse'"),
        "6"
    );

    // Every other account reaches every destination, so each one is there to be refused.
    for who in ["other", "root"] {
        for destination in &REFUSED {
            assert_eq!(
                lab.probe(
                    who,
                    destination.proto,
                    destination.address,
                    destination.port
                ),
                "ok",
                "{who} to {} ({} {})",
                destination.what,
                destination.proto,
                destination.address
            );
        }
    }
    let untouched = lab.counters();
    assert!(
        untouched.values().all(|packets| *packets == 0),
        "the other accounts' packets met the rules: {untouched:?}"
    );

    for destination in &REFUSED {
        let before = lab.counters();
        assert_eq!(
            lab.probe(
                "agent",
                destination.proto,
                destination.address,
                destination.port
            ),
            refusal(destination.proto),
            "agent to {} ({} {})",
            destination.what,
            destination.proto,
            destination.address
        );
        let after = lab.counters();
        assert!(
            after[destination.rule] > before[destination.rule],
            "the {} rule did not count the agent's packet to {}: {before:?} -> {after:?}",
            destination.rule,
            destination.what
        );
    }

    // Everything else stays open for the agent: devices on the LAN (no default drop here,
    // Laysi/trustiot-v2#814 adds the allowlist).
    for (proto, address, port) in [
        ("tcp", "10.81.3.1", 80),
        ("tcp", "fd81:3::1", 80),
        ("udp", "10.81.3.1", 9),
    ] {
        assert_eq!(
            lab.probe("agent", proto, address, port),
            "ok",
            "agent to the LAN peer {proto} {address}"
        );
    }
}

#[test]
fn the_agent_resolves_and_reaches_the_cloud_with_the_rules_in_force() {
    let Some(lab) = Lab::network() else {
        return;
    };
    lab.ok("systemd-sysusers");
    lab.ok(&format!("nft -f {RULES_PATH}"));

    // As on the gateway, /etc/resolv.conf names the network's DNS server, not a local stub.
    assert_eq!(
        lab.ok("grep '^nameserver' /etc/resolv.conf"),
        "nameserver 10.81.3.1"
    );
    assert_eq!(lab.ok("lab resolve agent cloud.test"), "10.81.3.1");
    assert_eq!(lab.probe("agent", "tcp", "cloud.test", 443), "ok");
    assert_eq!(
        lab.ok("lab counters | awk '{ s += $2 } END { print s }'"),
        "0"
    );

    // A loopback stub resolver would be refused to the agent: this is what check-gateway's
    // resolver check catches.
    lab.ok("printf 'nameserver 127.0.0.53\\n' > /etc/resolv.conf");
    assert_eq!(lab.ok("lab resolve root cloud.test"), "10.81.3.1");
    assert_eq!(lab.ok("lab resolve agent cloud.test"), "failed");
    let check = lab.sh("check-gateway --allow tcp:8080:socat");
    let report = String::from_utf8_lossy(&check.stdout);
    assert_eq!(check.status.code(), Some(1), "{report}");
    assert!(
        report.contains(
            "FAIL resolver: /etc/resolv.conf names the nameserver 127.0.0.53, a local address \
             (127.0.0.53), which the agent's egress rules refuse"
        ),
        "{report}"
    );

    // glibc reads `127.0.53` as 127.0.0.53: root resolves through the stub, the agent cannot, and
    // the check reads the address the same way.
    lab.ok("printf 'nameserver 127.0.53\\n' > /etc/resolv.conf");
    assert_eq!(lab.ok("lab resolve root cloud.test"), "10.81.3.1");
    assert_eq!(lab.ok("lab resolve agent cloud.test"), "failed");

    // Every spelling and kind of address the rules refuse, and no nameserver at all (glibc then
    // asks 127.0.0.1), fails the check; the network's DNS servers pass.
    let resolver = |conf: &str| {
        lab.ok(&format!("printf '{conf}' > /etc/resolv.conf"));
        let check = lab.sh("check-gateway --allow tcp:8080:socat");
        let report = String::from_utf8_lossy(&check.stdout).into_owned();
        (check.status.code(), report)
    };
    for (conf, reason) in [
        (
            "nameserver 127.0.53\\n",
            "127.0.53, a local address (127.0.0.53)",
        ),
        (
            "nameserver 0:0:0:0:0:0:0:1\\n",
            "0:0:0:0:0:0:0:1, a local address (::1)",
        ),
        (
            "nameserver ::ffff:7f00:35\\n",
            "::ffff:7f00:35, a local address (127.0.0.53)",
        ),
        // The gateway's own LAN address, where a dnsmasq could listen.
        (
            "nameserver 10.81.3.2\\n",
            "10.81.3.2, a local address (10.81.3.2)",
        ),
        (
            "nameserver 10.81.3.255\\n",
            "10.81.3.255, a broadcast address",
        ),
        (
            "nameserver 224.0.0.251\\n",
            "224.0.0.251, a multicast address",
        ),
        (
            "nameserver 169.254.169.254\\n",
            "169.254.169.254, the metadata address",
        ),
        ("nameserver 0.0.0.0\\n", "0.0.0.0, the unspecified address"),
        (
            "nameserver 10.81.3.1\\nnameserver 127.0.0.53\\n",
            "127.0.0.53, a local address",
        ),
    ] {
        let (code, report) = resolver(conf);
        assert_eq!(code, Some(1), "{conf}: {report}");
        assert!(
            report.contains(&format!(
                "FAIL resolver: /etc/resolv.conf names the nameserver {reason}"
            )),
            "{conf}: {report}"
        );
    }
    // No nameserver glibc can use (it ignores names and malformed addresses): glibc then asks
    // 127.0.0.1, which the agent cannot reach either.
    for conf in [
        "search example.test\\n",
        "nameserver dns.example.test\\nnameserver 10.81.3.1.7\\nnameserver\\n",
    ] {
        let (code, report) = resolver(conf);
        assert_eq!(code, Some(1), "{conf}: {report}");
        assert!(
            report.contains("FAIL resolver: /etc/resolv.conf names no nameserver glibc can use"),
            "{conf}: {report}"
        );
    }
    lab.ok("printf 'nameserver dns.example.test\\nnameserver 10.81.3.1.7\\n' > /etc/resolv.conf");
    assert_eq!(lab.ok("lab resolve root cloud.test"), "failed");
    for conf in [
        "nameserver 10.81.3.1\\nnameserver fd81:3::1\\n",
        "nameserver dns.example.test\\nnameserver 10.81.3.1\\n",
    ] {
        let (code, report) = resolver(conf);
        assert_eq!(code, Some(0), "{conf}: {report}");
        assert!(report.contains("ok resolver"), "{conf}: {report}");
    }
    assert_eq!(lab.ok("lab resolve agent cloud.test"), "10.81.3.1");
}

#[test]
fn the_gateway_check_flags_listeners_off_loopback_and_missing_rules() {
    let Some(lab) = Lab::network() else {
        return;
    };
    lab.ok("systemd-sysusers");

    // Before the rules are loaded; the gateway's local service listens on every address
    // (port 8080), the DNS stub on 127.0.0.53 only.
    let check = lab.sh("check-gateway");
    let report = String::from_utf8_lossy(&check.stdout);
    assert_eq!(check.status.code(), Some(1), "{report}");
    assert!(
        report.contains("FAIL listeners: tcp 0.0.0.0:8080 (socat)"),
        "{report}"
    );
    assert!(
        report.contains("FAIL listeners: tcp [::]:8080 (socat)"),
        "{report}"
    );
    assert!(
        !report.contains("127.0.0.53"),
        "a loopback listener was flagged: {report}"
    );
    assert!(
        report.contains("FAIL egress: table inet trustiot_agent is not loaded"),
        "{report}"
    );
    assert!(report.contains("ok resolver"), "{report}");

    lab.ok(&format!("nft -f {RULES_PATH}"));
    // An exception for another process does not cover the listener.
    let check = lab.sh("check-gateway --allow tcp:8080:sshd");
    let report = String::from_utf8_lossy(&check.stdout);
    assert_eq!(check.status.code(), Some(1), "{report}");
    assert!(
        report.contains("FAIL listeners: tcp 0.0.0.0:8080"),
        "{report}"
    );
    assert!(
        report.contains("ok egress: table inet trustiot_agent is loaded and is the agent's rules"),
        "{report}"
    );

    let check = lab.sh("check-gateway --allow tcp:8080:socat");
    let report = String::from_utf8_lossy(&check.stdout);
    assert_eq!(check.status.code(), Some(0), "{report}");
    assert!(
        report.contains("ALLOWED listeners: tcp 0.0.0.0:8080 (socat)"),
        "{report}"
    );

    // A table that is not exactly the agent's rules is caught: one that lost a rule, one whose
    // refusals are all there but no longer reached from the output hook, and one that accepts
    // the account's packets before the jump. Each case starts from the shipped file.
    let egress_fails = |change: &str, line: &str| {
        lab.ok(&format!("nft -f {RULES_PATH} && {change}"));
        let check = lab.sh("check-gateway --allow tcp:8080");
        let report = String::from_utf8_lossy(&check.stdout);
        assert_eq!(check.status.code(), Some(1), "after `{change}`: {report}");
        assert!(
            report.contains("FAIL egress: table inet trustiot_agent is not the agent's rules: ")
                && report.contains(line),
            "after `{change}`, expected a FAIL egress naming {line:?}: {report}"
        );
    };
    let handle = |chain: &str, pattern: &str| {
        format!(
            "$(nft -a list chain inet trustiot_agent {chain} | awk '/{pattern}/ {{ print $NF }}')"
        )
    };
    egress_fails(
        &format!(
            "nft delete rule inet trustiot_agent agent handle {}",
            handle("agent", "comment \"multicast\"")
        ),
        r#"the agent's rules have "fib daddr type multicast counter goto refuse comment "multicast"""#,
    );
    egress_fails(
        &format!(
            "nft delete rule inet trustiot_agent output handle {}",
            handle("output", "jump agent")
        ),
        r#"line 4 is "}", the agent's rules have "meta skuid "#,
    );
    egress_fails(
        r#"nft insert rule inet trustiot_agent output meta skuid "trustiot-agent" accept"#,
        r#" accept", the agent's rules have "meta skuid "#,
    );
    // Rules loaded for the account's old uid no longer match the agent's processes.
    egress_fails(
        "usermod --uid 4242 trustiot-agent",
        r#"the agent's rules have "meta skuid 4242 jump agent""#,
    );
    // Back to the shipped rules (now for uid 4242): ok again.
    lab.ok(&format!("nft -f {RULES_PATH}"));
    let check = lab.sh("check-gateway --allow tcp:8080");
    let report = String::from_utf8_lossy(&check.stdout);
    assert_eq!(check.status.code(), Some(0), "{report}");

    let usage = lab.sh("check-gateway --allow icmp:1");
    assert_eq!(usage.status.code(), Some(2));
}

/// The stand-in for the agent, run by the agent's unit. At every start it records in `probes` in
/// its state directory whether its account could send a datagram to 198.51.100.1, an address the
/// gateway holds through its local routing table (`reached`, or `refused` at once with `EPERM`).
/// Only the egress rules refuse it, not the unit's `IPAddressDeny=` (Laysi/trustiot-v2#878), so
/// the probe shows whether the rules were in force also where systemd applies that. Then it stays
/// up, and whenever a file `probe-now` appears there it runs each of its lines as `lab <line>`,
/// under the unit's sandbox and IP firewall, and writes `<line> => <outcome>` lines to
/// `probe-results`.
const STAND_IN: &str = r#"#!/bin/bash
if (echo x > /dev/udp/198.51.100.1/9) 2> /dev/null; then outcome=reached; else outcome=refused; fi
echo "$outcome" >> "$STATE_DIRECTORY/probes"
while :; do
    if [ -e "$STATE_DIRECTORY/probe-now" ]; then
        while read -r line; do
            # shellcheck disable=SC2086 # the line is the words of a lab command
            echo "$line => $(/usr/local/bin/lab $line 2>&1)"
        done < "$STATE_DIRECTORY/probe-now" > "$STATE_DIRECTORY/probe-results.new"
        rm "$STATE_DIRECTORY/probe-now"
        mv "$STATE_DIRECTORY/probe-results.new" "$STATE_DIRECTORY/probe-results"
    fi
    sleep 0.2
done
"#;

/// systemd as PID 1 in the container, with the units installed as the image installs them.
/// `None` (and a printed reason) when the container cannot run systemd.
fn systemd_lab() -> Option<Lab> {
    systemd_lab_with(false)
}

/// [`systemd_lab`], with the stand-in network of `lab network` (the site's LAN, DNS server and
/// cloud) when `network` is set; without it, only a service on the gateway's 127.0.0.1:8080.
fn systemd_lab_with(network: bool) -> Option<Lab> {
    let Some(runtime) = container_runtime() else {
        unavailable("podman or docker");
        return None;
    };
    if runtime != "podman" {
        eprintln!("skipped: systemd as PID 1 in a test container needs podman, not {runtime}");
        return None;
    }
    let lab = Lab::run(
        runtime,
        &[
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
            "IMAGE",
            "/sbin/init",
        ],
    );
    let started = Instant::now();
    loop {
        let state = lab.sh("systemctl is-system-running");
        let state = String::from_utf8_lossy(&state.stdout).trim().to_owned();
        if state == "running" || state == "degraded" {
            break;
        }
        if started.elapsed() > BOOT_BOUND {
            eprintln!(
                "skipped: systemd in the container did not finish booting within {BOOT_BOUND:?} \
                 (state {state:?})"
            );
            return None;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    lab.install();
    lab.put("/usr/lib/systemd/system/trustiot-agent.service", AGENT_UNIT);
    lab.put(
        "/usr/lib/systemd/system/trustiot-agent-egress.service",
        EGRESS_UNIT,
    );
    lab.put(NFTABLES_DROP_IN, NFTABLES_RELOAD);
    lab.put(&format!("/usr/lib/systemd/system/{CHECK}"), CHECK_UNIT);
    lab.put(&format!("/usr/lib/systemd/system/{TIMER}"), CHECK_TIMER);
    lab.put("/usr/bin/trustiot-agent", STAND_IN);
    // Not Type=notify and no watchdog: the stand-in does not speak sd_notify. Everything else,
    // the dependencies and the sandbox included, is the real unit's.
    lab.put(
        "/etc/systemd/system/trustiot-agent.service.d/stand-in.conf",
        "[Service]\nType=exec\nWatchdogSec=0\n",
    );
    lab.ok(
        "chmod 0755 /usr/bin/trustiot-agent && groupadd --system tss && systemd-sysusers \
         && systemctl daemon-reload",
    );
    if network {
        // The gateway's local service listens on every address there, and the network holds the
        // stand-in's probe address through the local routing table.
        lab.ok("lab network");
        return Some(lab);
    }
    // The stand-in's probe address, held only through the local routing table.
    lab.ok("ip route add local 198.51.100.0/24 dev lo");
    // The gateway's local service, reachable for root.
    let _ = runtime_output(
        runtime,
        &[
            "exec",
            "--detach",
            &lab.container.name,
            "socat",
            "TCP-LISTEN:8080,bind=127.0.0.1,fork,reuseaddr",
            "EXEC:/bin/true",
        ],
    );
    lab.ok(
        "for i in $(seq 600); do ss -H -tln 'sport = :8080' | grep -q . && exit 0; sleep 0.1; done; \
         echo 'nothing listens on 127.0.0.1:8080 within 60 s' >&2; exit 1",
    );
    Some(lab)
}

fn property(lab: &Lab, unit: &str, name: &str) -> String {
    lab.ok(&format!("systemctl show --property={name} --value {unit}"))
}

/// Turns the check of `trustiot-agent-egress-check.service` into a no-op, for a test of another
/// mechanism: a check that runs within the test would load the rules again on its own. Its timer
/// keeps running it.
fn disable_check(lab: &Lab) {
    lab.put(
        &format!("/etc/systemd/system/{CHECK}.d/off.conf"),
        "[Service]\nExecStart=\nExecStart=/bin/true\n",
    );
    lab.ok("systemctl daemon-reload");
}

/// Waits until systemd has no queued jobs. `systemctl restart` returns when the job of the unit
/// it names finishes, while the restarts it propagates to the units that require that unit may
/// still be running.
fn settle(lab: &Lab) {
    lab.ok(
        "for i in $(seq 600); do [ \"$(systemctl list-jobs --no-legend | wc -l)\" = 0 ] && exit 0; \
         sleep 0.1; done; echo 'systemd still had queued jobs after 60 s' >&2; exit 1",
    );
}

/// Waits until the stand-in agent has recorded `count` probes. With `Type=exec`, systemctl returns
/// as soon as the stand-in runs, possibly before it wrote its probe.
fn await_probes(lab: &Lab, count: usize) {
    lab.ok(&format!(
        "for i in $(seq 600); do \
         [ \"$(cat /var/lib/trustiot-agent/probes 2> /dev/null | wc -l)\" -ge {count} ] && exit 0; \
         sleep 0.1; done; echo 'the stand-in did not record {count} probes within 60 s' >&2; exit 1"
    ));
}

#[test]
fn the_rules_are_in_force_before_the_agent_starts_and_a_failed_load_keeps_it_stopped() {
    let Some(lab) = systemd_lab() else {
        return;
    };
    // The check (Laysi/trustiot-v2#878) would also load the rules when nftables.service's restart
    // flushes them; this test is about the units' dependencies alone.
    disable_check(&lab);
    let agent = "trustiot-agent.service";
    let egress = "trustiot-agent-egress.service";
    assert!(
        lab.sh("(: <> /dev/tcp/127.0.0.1/8080)").status.success(),
        "root reaches the service"
    );
    assert!(
        lab.sh("echo x > /dev/udp/198.51.100.1/9").status.success(),
        "root sends to the stand-in's probe address"
    );

    // Starting the agent starts nftables.service, then loads the rules, then starts the agent,
    // whose first datagram to the gateway's own address is already refused.
    lab.ok(&format!("systemctl start {agent}"));
    for unit in ["nftables.service", egress, agent] {
        assert_eq!(property(&lab, unit, "ActiveState"), "active", "{unit}");
    }
    let monotonic = |unit: &str, name: &str| -> u64 { property(&lab, unit, name).parse().unwrap() };
    let flushed = monotonic("nftables.service", "ExecMainExitTimestampMonotonic");
    let loaded = monotonic(egress, "ExecMainExitTimestampMonotonic");
    let started = monotonic(agent, "ExecMainStartTimestampMonotonic");
    assert!(
        0 < flushed && flushed <= loaded && loaded <= started,
        "nftables.service ran at {flushed}, the rules loaded at {loaded}, the agent started at \
         {started}"
    );
    await_probes(&lab, 1);
    assert_eq!(lab.ok("cat /var/lib/trustiot-agent/probes"), "refused");

    // nftables.service flushes the whole ruleset (Debian's /etc/nftables.conf begins with
    // `flush ruleset`). It is already active, so a start does nothing; after a reload the rules
    // are loaded again; a restart restarts the rules and the agent; a stop stops both.
    lab.ok("systemctl start nftables.service");
    lab.ok("nft list table inet trustiot_agent > /dev/null");
    lab.ok("systemctl reload nftables.service");
    lab.ok("nft list table inet trustiot_agent > /dev/null");
    assert_eq!(property(&lab, agent, "ActiveState"), "active");
    // The reload is one transaction of /etc/nftables.conf (with its `flush ruleset`) and the
    // rules: when the rules file does not load or is missing, the reload fails and nothing was
    // flushed. The marker table shows that /etc/nftables.conf's flush did not apply either.
    let reload_changes_nothing = |break_it: &str, why: &str| {
        lab.ok(&format!(
            "cp {RULES_PATH} /root/rules.good && nft add table inet reload_marker && {break_it}"
        ));
        assert!(
            !lab.sh("systemctl reload nftables.service").status.success(),
            "the reload succeeded with {why}"
        );
        lab.ok("nft list table inet trustiot_agent > /dev/null");
        lab.ok("nft list table inet reload_marker > /dev/null");
        assert_eq!(property(&lab, agent, "ActiveState"), "active");
        lab.ok(&format!(
            "mv /root/rules.good {RULES_PATH} && nft delete table inet reload_marker"
        ));
    };
    reload_changes_nothing(
        &format!("echo 'not nft' >> {RULES_PATH}"),
        "a broken rules file",
    );
    reload_changes_nothing(&format!("rm {RULES_PATH}"), "the rules file missing");
    // With both files in place the reload still applies /etc/nftables.conf (its flush removes
    // the marker) and the rules.
    lab.ok("nft add table inet reload_marker && systemctl reload nftables.service");
    assert!(
        !lab.sh("nft list table inet reload_marker").status.success(),
        "the reload did not apply /etc/nftables.conf"
    );
    lab.ok("nft list table inet trustiot_agent > /dev/null");
    assert_eq!(property(&lab, agent, "ActiveState"), "active");
    // A restart of nftables.service restarts the rules and then the agent (Requires= propagates
    // an explicit restart, systemd.unit(5)): both come back with a new invocation, the agent's
    // after the rules loaded again.
    let egress_before = property(&lab, egress, "InvocationID");
    let agent_before = property(&lab, agent, "InvocationID");
    lab.ok("systemctl restart nftables.service");
    settle(&lab);
    lab.ok("nft list table inet trustiot_agent > /dev/null");
    for (unit, before) in [(egress, &egress_before), (agent, &agent_before)] {
        assert_eq!(
            property(&lab, unit, "ActiveState"),
            "active",
            "{unit} after a restart of nftables.service"
        );
        assert_ne!(
            &property(&lab, unit, "InvocationID"),
            before,
            "{unit} was not restarted with nftables.service"
        );
    }
    let reloaded: u64 = property(&lab, egress, "ExecMainExitTimestampMonotonic")
        .parse()
        .expect("a timestamp");
    let restarted: u64 = property(&lab, agent, "ExecMainStartTimestampMonotonic")
        .parse()
        .expect("a timestamp");
    assert!(
        reloaded <= restarted,
        "the agent restarted at {restarted}, before the rules loaded again at {reloaded}"
    );
    // The restarted stand-in has recorded its probe before the stop.
    await_probes(&lab, 2);
    lab.ok("systemctl stop nftables.service");
    assert_eq!(property(&lab, egress, "ActiveState"), "inactive");
    assert_eq!(
        property(&lab, agent, "ActiveState"),
        "inactive",
        "the agent ran on after the flush"
    );
    assert_eq!(
        lab.ok("cat /var/lib/trustiot-agent/probes"),
        "refused\nrefused",
        "every start of the agent found the rules in force"
    );

    // When the rules cannot be loaded, the agent does not start at all, and nothing retries it.
    // Two causes: nftables.service fails (a broken /etc/nftables.conf), and the rules file is
    // missing.
    let refused_start = |cause: &str, failed_unit: &str| {
        lab.ok("rm -f /var/lib/trustiot-agent/probes");
        let start = lab.sh(&format!("systemctl start {agent}"));
        assert!(
            !start.status.success(),
            "the agent started without its rules ({cause})"
        );
        assert_eq!(
            property(&lab, failed_unit, "ActiveState"),
            "failed",
            "{cause}"
        );
        assert_eq!(
            lab.ok("systemctl list-jobs --no-legend | wc -l"),
            "0",
            "{cause}"
        );
        assert_eq!(property(&lab, agent, "ActiveState"), "inactive", "{cause}");
        assert_eq!(property(&lab, agent, "ExecMainPID"), "0", "{cause}");
        assert_eq!(
            lab.sh("test -e /var/lib/trustiot-agent/probes")
                .status
                .code(),
            Some(1),
            "the stand-in ran ({cause})"
        );
        assert!(
            lab.sh("nft list table inet trustiot_agent").status.code() != Some(0),
            "{cause}"
        );
    };
    lab.ok("cp /etc/nftables.conf /etc/nftables.conf.good && echo 'not nft' >> /etc/nftables.conf");
    refused_start("a broken /etc/nftables.conf", "nftables.service");
    lab.ok(
        "mv /etc/nftables.conf.good /etc/nftables.conf && systemctl reset-failed nftables.service",
    );
    lab.ok(&format!("mv {RULES_PATH} {RULES_PATH}.away"));
    refused_start("no rules file", egress);
    lab.ok(&format!(
        "mv {RULES_PATH}.away {RULES_PATH} && systemctl reset-failed {egress}"
    ));
    lab.ok(&format!(
        "mv {NFTABLES_DROP_IN} /root/drop-in.away && systemctl daemon-reload"
    ));
    refused_start("no drop-in for nftables.service", egress);

    // Once everything is installed again, starting the agent works.
    lab.ok(&format!(
        "mv /root/drop-in.away {NFTABLES_DROP_IN} && systemctl daemon-reload \
         && systemctl reset-failed {egress}"
    ));
    lab.ok(&format!("systemctl start {agent}"));
    await_probes(&lab, 1);
    assert_eq!(lab.ok("cat /var/lib/trustiot-agent/probes"), "refused");
}

/// How long an automatic restart of the agent may take to come. The unit's delays grow from
/// `RestartSec=2s` towards `RestartMaxDelaySec=1min` in `RestartSteps=5`; the test below waits for
/// at most the fourth automatic restart, about 15 s after the third, so this bound only catches a
/// restart that never comes.
const RESTART_BOUND: Duration = Duration::from_secs(300);

/// Polls `done` every 200 ms until it holds; fails naming `what` after `bound`.
fn await_until(what: &str, bound: Duration, mut done: impl FnMut() -> bool) {
    let started = Instant::now();
    while !done() {
        assert!(
            started.elapsed() < bound,
            "{what} did not happen within {bound:?}"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// What every start of the stand-in agent found, oldest first.
fn probes(lab: &Lab) -> Vec<String> {
    let output = lab.sh("cat /var/lib/trustiot-agent/probes 2> /dev/null");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_owned)
        .collect()
}

fn rules_loaded(lab: &Lab) -> bool {
    lab.sh("nft list table inet trustiot_agent > /dev/null 2>&1")
        .status
        .success()
}

/// Laysi/trustiot-v2#863: root flushes the ruleset outside systemd, which leaves the egress unit
/// active, and then the agent's process ends. The automatic restart (`Restart=always`) loads the
/// rules again before the agent runs, so its first connection is refused again; when they cannot
/// load, the agent does not run, and a later restart brings it back once they can.
#[test]
fn a_restart_after_the_rules_were_flushed_loads_them_again_or_keeps_the_agent_stopped() {
    let Some(lab) = systemd_lab() else {
        return;
    };
    // The check (Laysi/trustiot-v2#878) would load the flushed rules again on its own; this test
    // is about the agent's ExecStartPre= alone.
    disable_check(&lab);
    let agent = "trustiot-agent.service";
    let egress = "trustiot-agent-egress.service";
    lab.ok(&format!("systemctl start {agent}"));
    await_probes(&lab, 1);
    assert_eq!(probes(&lab), ["refused"]);
    let egress_invocation = property(&lab, egress, "InvocationID");
    let egress_loaded = property(&lab, egress, "ExecMainExitTimestampMonotonic");

    // The gap: after a flush outside systemd the egress unit still says active.
    lab.ok("nft flush ruleset");
    assert!(!rules_loaded(&lab), "the flush left the rules in place");
    assert_eq!(property(&lab, egress, "ActiveState"), "active");
    assert_eq!(property(&lab, agent, "ActiveState"), "active");

    // The agent's process ends as in a crash, not through systemctl.
    let restarts = |lab: &Lab| -> u64 { property(lab, agent, "NRestarts").parse().unwrap() };
    let before = restarts(&lab);
    let crashed = property(&lab, agent, "MainPID");
    lab.ok(&format!("kill -KILL {crashed}"));
    await_until(
        "the restarted stand-in recording its probe",
        RESTART_BOUND,
        || probes(&lab).len() == 2,
    );
    assert_eq!(
        probes(&lab),
        ["refused", "refused"],
        "the restarted agent ran without its rules"
    );
    assert!(rules_loaded(&lab));
    // One automatic restart, counted as one: the check's timer (Laysi/trustiot-v2#878) must
    // neither cancel it (a timer bound to the agent with BindsTo=, which the agent requires, did)
    // nor restart the agent a second time and reset the counter that RestartSteps= grows the delay
    // by (an agent that requires the timer did). The timer restarted with the agent.
    assert_eq!(restarts(&lab), before + 1, "one automatic restart");
    settle(&lab);
    assert_eq!(property(&lab, TIMER, "ActiveState"), "active");
    // The agent's unit loaded them itself: the egress unit did not run again.
    assert_eq!(property(&lab, egress, "InvocationID"), egress_invocation);
    assert_eq!(
        property(&lab, egress, "ExecMainExitTimestampMonotonic"),
        egress_loaded
    );

    // The load runs as root ("+"), the agent does not: the restarted process has the agent's
    // account, no capabilities in any set, and cannot gain any.
    let pid = property(&lab, agent, "MainPID");
    assert_ne!(pid, "0");
    let uid = lab.ok("id -u trustiot-agent");
    let status = lab.ok(&format!("cat /proc/{pid}/status"));
    let field = |name: &str| -> Vec<&str> {
        status
            .lines()
            .find_map(|line| line.strip_prefix(&format!("{name}:")))
            .unwrap_or_else(|| panic!("no {name} in /proc/{pid}/status:\n{status}"))
            .split_whitespace()
            .collect()
    };
    assert_eq!(field("Uid"), [uid.as_str(); 4], "{status}");
    for set in ["CapInh", "CapPrm", "CapEff", "CapBnd", "CapAmb"] {
        assert_eq!(field(set), ["0000000000000000"], "{set}: {status}");
    }
    assert_eq!(field("NoNewPrivs"), ["1"]);

    // Fail closed: the rules flushed again and the file no longer loads. The restart's
    // ExecStartPre= fails, the stand-in does not run, and nothing loaded the rules.
    lab.ok(&format!(
        "cp {RULES_PATH} /root/rules.good && echo 'not nft' >> {RULES_PATH} && nft flush ruleset"
    ));
    let before = restarts(&lab);
    let crashed = property(&lab, agent, "MainPID");
    lab.ok(&format!("kill -KILL {crashed}"));
    await_until("a restart whose rules load failed", RESTART_BOUND, || {
        restarts(&lab) > before && property(&lab, agent, "Result") == "exit-code"
    });
    assert_ne!(property(&lab, agent, "ActiveState"), "active");
    assert_eq!(property(&lab, agent, "MainPID"), "0");
    assert_eq!(
        probes(&lab),
        ["refused", "refused"],
        "the stand-in ran although its rules did not load"
    );
    assert!(!rules_loaded(&lab), "something loaded the broken rules");
    assert_eq!(property(&lab, egress, "InvocationID"), egress_invocation);

    // Restart=always keeps trying: once the file loads again, a later restart loads it and the
    // agent runs with its rules, without anyone starting it.
    lab.ok(&format!("mv /root/rules.good {RULES_PATH}"));
    await_until(
        "the stand-in running again after the rules file was fixed",
        RESTART_BOUND,
        || probes(&lab).len() == 3,
    );
    assert_eq!(probes(&lab), ["refused", "refused", "refused"]);
    assert!(rules_loaded(&lab));
    assert_eq!(property(&lab, agent, "ActiveState"), "active");
}

/// What `trustiot-agent-egress-check.timer` configures as the longest time from one check to the
/// next (`OnUnitActiveSec=` plus `AccuracySec=`): the bound on noticing rules flushed outside
/// systemd.
fn check_bound() -> Duration {
    let seconds = |key: &str| -> Duration {
        let value = CHECK_TIMER
            .lines()
            .find_map(|line| line.strip_prefix(&format!("{key}=")))
            .unwrap_or_else(|| panic!("no {key}= in the timer"));
        Duration::from_secs(value.strip_suffix('s').unwrap().parse().unwrap())
    };
    seconds("OnUnitActiveSec") + seconds("AccuracySec")
}

/// How long a test waits for the check to act. It only catches a check that never comes: the
/// bound itself is asserted from the journal's timestamps.
const CHECK_WAIT: Duration = Duration::from_secs(300);

/// The monotonic timestamps (microseconds) of the journal entries that match `filter` (a
/// journalctl match such as `_SYSTEMD_UNIT=x.service`) and whose message contains `text`.
fn journal_times(lab: &Lab, filter: &str, text: &str) -> Vec<u64> {
    lab.ok(&format!(
        "journalctl --no-pager --quiet --output=json {filter}"
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

/// Laysi/trustiot-v2#878: root flushes the ruleset outside systemd while the agent runs, and
/// nothing restarts the agent. The check loads the rules again within its bound and has the agent
/// restarted (it fills its allowlist sets when it starts, Laysi/trustiot-v2#865); when the file
/// does not load, the restart keeps the agent from running until it loads again. Without the check
/// the rules stay gone.
#[test]
fn rules_flushed_while_the_agent_runs_come_back_within_the_checks_bound_or_the_agent_stops() {
    let Some(lab) = systemd_lab() else {
        return;
    };
    let agent = "trustiot-agent.service";
    let egress = "trustiot-agent-egress.service";
    lab.ok(&format!("systemctl start {agent}"));
    await_probes(&lab, 1);
    assert_eq!(probes(&lab), ["refused"]);
    assert_eq!(property(&lab, TIMER, "ActiveState"), "active");

    // A check that finds the rules stays silent: not even systemd's lines about starting and
    // finishing it reach the journal.
    await_until("a first check", CHECK_WAIT, || {
        property(&lab, CHECK, "ExecMainExitTimestampMonotonic") != "0"
    });
    assert_eq!(property(&lab, CHECK, "Result"), "success");
    assert_eq!(
        lab.ok(&format!("journalctl --no-pager --quiet --unit={CHECK}")),
        "",
        "a check that found the rules wrote to the journal"
    );
    let egress_invocation = property(&lab, egress, "InvocationID");
    let agent_invocation = property(&lab, agent, "InvocationID");

    // The flush, outside systemd: the units stay active and no one restarts the agent.
    lab.ok("nft flush ruleset && echo flushed | systemd-cat --identifier=egress-test");
    await_until(
        "the check loading the flushed rules again",
        CHECK_WAIT,
        || rules_loaded(&lab),
    );
    let flushed = journal_times(&lab, "SYSLOG_IDENTIFIER=egress-test", "flushed");
    let noticed = journal_times(
        &lab,
        &format!("_SYSTEMD_UNIT={CHECK}"),
        "table inet trustiot_agent is missing",
    );
    assert_eq!(
        (flushed.len(), noticed.len()),
        (1, 1),
        "{flushed:?} {noticed:?}"
    );
    let latency = Duration::from_micros(noticed[0].saturating_sub(flushed[0]));
    eprintln!(
        "the check noticed the flush {latency:?} after it (bound {:?})",
        check_bound()
    );
    assert!(
        latency <= check_bound(),
        "the check noticed the flush {latency:?} after it, beyond the timer's {:?}",
        check_bound()
    );
    // Then it had the agent restarted, and the restarted stand-in found the rules in force. The
    // check loaded them itself: the egress unit did not run again.
    await_probes(&lab, 2);
    assert_eq!(probes(&lab), ["refused", "refused"]);
    settle(&lab);
    assert_ne!(property(&lab, agent, "InvocationID"), agent_invocation);
    assert_eq!(property(&lab, egress, "InvocationID"), egress_invocation);
    assert_eq!(property(&lab, agent, "ActiveState"), "active");
    assert_eq!(
        property(&lab, TIMER, "ActiveState"),
        "active",
        "the check runs on with the restarted agent"
    );

    // Fail closed: the rules flushed again, and the file no longer loads. The check has the agent
    // restarted, the restart's ExecStartPre= fails, and the stand-in does not run.
    lab.ok(&format!(
        "cp {RULES_PATH} /root/rules.good && echo 'not nft' >> {RULES_PATH} && nft flush ruleset"
    ));
    await_until(
        "the check restarting the agent into a start whose rules do not load",
        CHECK_WAIT,
        || {
            !journal_times(&lab, &format!("_SYSTEMD_UNIT={CHECK}"), "did not load").is_empty()
                && property(&lab, agent, "MainPID") == "0"
                && property(&lab, agent, "Result") == "exit-code"
        },
    );
    assert_ne!(property(&lab, agent, "ActiveState"), "active");
    assert_eq!(
        probes(&lab),
        ["refused", "refused"],
        "the stand-in ran although its rules did not load"
    );
    assert!(!rules_loaded(&lab), "something loaded the broken rules");

    // Restart=always keeps trying: once the file loads again, the agent runs with its rules, and
    // the check with it, without anyone starting them.
    lab.ok(&format!("mv /root/rules.good {RULES_PATH}"));
    await_until(
        "the stand-in running again after the rules file was fixed",
        RESTART_BOUND,
        || probes(&lab).len() == 3,
    );
    assert_eq!(probes(&lab), ["refused", "refused", "refused"]);
    assert!(rules_loaded(&lab));
    settle(&lab);
    assert_eq!(property(&lab, agent, "ActiveState"), "active");
    assert_eq!(property(&lab, TIMER, "ActiveState"), "active");

    // The control: with the check turned into a no-op, a flush stays. Two runs of it start after
    // the flush, and the rules are still gone.
    disable_check(&lab);
    lab.ok("nft flush ruleset");
    for run in ["first", "second"] {
        let before = property(&lab, CHECK, "ExecMainStartTimestampMonotonic");
        await_until(
            &format!("a {run} run of the no-op check"),
            CHECK_WAIT,
            || property(&lab, CHECK, "ExecMainStartTimestampMonotonic") != before,
        );
    }
    assert!(!rules_loaded(&lab), "the rules came back without the check");
}

/// The agent enabled as the image enables it starts at boot with its check, and systemd finds no
/// ordering cycle among their units.
#[test]
fn the_enabled_agent_starts_at_boot_with_its_check() {
    let Some(lab) = systemd_lab() else {
        return;
    };
    lab.ok("systemctl enable trustiot-agent.service");
    let _ = runtime_output(lab.container.runtime, &["restart", &lab.container.name]);
    await_until("systemd booting again", BOOT_BOUND, || {
        let state = lab.sh("systemctl is-system-running");
        let state = String::from_utf8_lossy(&state.stdout).trim().to_owned();
        state == "running" || state == "degraded"
    });
    await_probes(&lab, 1);
    assert_eq!(probes(&lab), ["refused"]);
    settle(&lab);
    assert_eq!(
        property(&lab, "trustiot-agent.service", "ActiveState"),
        "active"
    );
    assert_eq!(property(&lab, TIMER, "ActiveState"), "active");
    let cycles = lab.ok("journalctl --boot --no-pager --quiet | grep -i 'ordering cycle' || true");
    assert_eq!(cycles, "", "systemd found an ordering cycle at boot");
}

/// Whether systemd in the lab applies `IPAddressDeny=`. It loads BPF programs into the unit's
/// cgroup, which systemd in a rootless container cannot do on a host that keeps BPF to privileged
/// processes (`kernel.unprivileged_bpf_disabled`, set on Debian and Fedora); systemd then runs the
/// unit without them, and logs that only at debug level. Needs `lab network`.
fn ip_firewall_applies(lab: &Lab) -> bool {
    lab.put(
        "/run/systemd/system/ip-firewall-probe.service",
        "[Service]\nType=oneshot\nIPAddressDeny=any\n\
         ExecStart=/bin/bash -c 'echo x > /dev/udp/10.81.3.1/9'\n",
    );
    lab.ok("systemctl daemon-reload");
    !lab.sh("systemctl start ip-firewall-probe.service")
        .status
        .success()
}

/// Has the stand-in run each `lab` command line of `expected` under the agent's unit, and checks
/// what each printed.
fn assert_outcomes(lab: &Lab, when: &str, expected: &[(&str, &str)]) {
    let state = "/var/lib/trustiot-agent";
    let request: String = expected
        .iter()
        .map(|(line, _)| format!("{line}\n"))
        .collect();
    lab.ok(&format!("rm -f {state}/probe-results"));
    lab.put(&format!("{state}/probe-now.new"), &request);
    lab.ok(&format!("mv {state}/probe-now.new {state}/probe-now"));
    await_until(&format!("the stand-in's probes {when}"), CHECK_WAIT, || {
        lab.sh(&format!("test -e {state}/probe-results"))
            .status
            .success()
    });
    let results = lab.ok(&format!("cat {state}/probe-results"));
    eprintln!("the stand-in's probes {when}:\n{results}");
    let found: Vec<(&str, &str)> = results
        .lines()
        .map(|line| {
            let (command, outcome) = line.split_once(" => ").unwrap();
            (command, outcome.trim())
        })
        .collect();
    assert_eq!(found, expected, "{when}");
}

/// What the agent needs: devices on the LAN (also one on an IPv4 link-local address, as RS485
/// converters without DHCP use), the cloud by name, and the network's DNS server.
const NEEDED: [(&str, &str); 5] = [
    ("probe self tcp 10.81.3.1 80", "ok"),
    ("probe self tcp fd81:3::1 80", "ok"),
    ("probe self tcp 169.254.81.1 80", "ok"),
    ("resolve self cloud.test", "10.81.3.1"),
    ("probe self tcp cloud.test 443", "ok"),
];

/// Laysi/trustiot-v2#878: with the egress rules flushed outside systemd and before the check loads
/// them again, the agent's unit still refuses loopback, the metadata addresses, multicast and the
/// limited broadcast address (`IPAddressDeny=`, independent of nftables), while what the agent
/// needs works, with and without the rules. The gateway's own LAN and link-local addresses and
/// subnet broadcasts are not covered at this layer: they wait for the check. Without
/// `IPAddressDeny=` the same stand-in reaches loopback.
///
/// systemd applies `IPAddressDeny=` only where it may load BPF programs: in a rootful container.
/// Run rootless, the test says so and skips.
#[test]
fn with_its_rules_flushed_the_agents_unit_still_refuses_loopback_metadata_multicast_and_broadcast()
{
    let Some(lab) = systemd_lab_with(true) else {
        return;
    };
    if !ip_firewall_applies(&lab) {
        eprintln!(
            "skipped: systemd in the container does not apply IPAddressDeny= (loading cgroup BPF \
             programs needs privileges a rootless container lacks); run this test as root, which \
             runs a rootful container"
        );
        return;
    }
    // This test is about what holds before the check loads the rules again.
    disable_check(&lab);
    lab.ok("lab link-local");
    let agent = "trustiot-agent.service";
    lab.ok(&format!("systemctl start {agent}"));
    await_probes(&lab, 1);
    assert_eq!(probes(&lab), ["refused"]);

    // Both layers in force: what the agent needs works, and so does a DNS server on a link-local
    // address (NetworkManager writes one with its interface).
    assert_outcomes(&lab, "with the rules in force", &NEEDED);
    let link_local_dns = |when: &str| {
        lab.ok("printf 'nameserver fe80::81:1%%veth0\\n' > /etc/resolv.conf");
        assert_outcomes(&lab, when, &[("resolve self cloud.test", "10.81.3.1")]);
        lab.ok("printf 'nameserver 10.81.3.1\\n' > /etc/resolv.conf");
    };
    link_local_dns("through a link-local DNS server, with the rules in force");

    lab.ok("nft flush ruleset");
    assert!(!rules_loaded(&lab));
    assert_outcomes(
        &lab,
        "with the rules flushed",
        &[
            // Refused by IPAddressDeny=: UDP at once (EPERM); a TCP SYN is dropped, so the
            // connect times out (here after 10 s).
            ("probe self udp 127.0.0.1 9", "not-permitted"),
            ("probe self udp 127.0.0.2 9", "not-permitted"),
            ("probe self udp ::1 9", "not-permitted"),
            ("probe self tcp 127.0.0.1 8080 10", "timeout"),
            ("probe self tcp 0.0.0.0 8080 10", "timeout"),
            ("probe self udp 169.254.169.254 9", "not-permitted"),
            ("probe self tcp 169.254.169.254 80 10", "timeout"),
            ("probe self udp fd00:ec2::254 9", "not-permitted"),
            ("probe self udp 239.1.2.3 9", "not-permitted"),
            ("probe self udp ff05::1:3 9", "not-permitted"),
            ("probe self udp 255.255.255.255 9", "not-permitted"),
            // Not covered here, until the check loads the rules again (within its bound): the
            // gateway's own addresses and its subnet's broadcast address.
            ("probe self udp 10.81.3.2 9", "ok"),
            ("probe self udp fe80::81:2%veth0 9", "ok"),
            ("probe self udp 10.81.3.255 9", "ok"),
        ],
    );
    assert_outcomes(&lab, "with the rules flushed", &NEEDED);
    link_local_dns("through a link-local DNS server, with the rules flushed");

    // The control: without IPAddressDeny=, the same flush lets the stand-in reach loopback, the
    // metadata address and multicast.
    lab.put(
        "/etc/systemd/system/trustiot-agent.service.d/no-ip-deny.conf",
        "[Service]\nIPAddressDeny=\n",
    );
    lab.ok(&format!(
        "systemctl daemon-reload && systemctl restart {agent}"
    ));
    await_probes(&lab, 2);
    assert_eq!(
        probes(&lab),
        ["refused", "refused"],
        "the restart loads the rules"
    );
    lab.ok("nft flush ruleset");
    assert_outcomes(
        &lab,
        "without IPAddressDeny=, with the rules flushed",
        &[
            ("probe self udp 127.0.0.1 9", "ok"),
            ("probe self tcp 127.0.0.1 8080 10", "ok"),
            ("probe self tcp 169.254.169.254 80 10", "ok"),
            ("probe self udp 239.1.2.3 9", "ok"),
        ],
    );
}
