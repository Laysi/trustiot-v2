//! The kernel-level egress rules for the agent's account (Laysi/trustiot-v2#813,
//! `systemd/trustiot-agent-egress.nft`), the unit that loads them before the agent starts
//! (`systemd/trustiot-agent-egress.service`), and the gateway check (`scripts/check-gateway.sh`);
//! and the unix sockets the agent's sandbox hides, which the rules cannot see
//! (Laysi/trustiot-v2#826).
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
const CHECK: &str = include_str!("../scripts/check-gateway.sh");
const LAB: &str = include_str!("egress/lab.sh");
const CONTAINERFILE: &str = include_str!("egress/Containerfile");

/// Where the image installs the rules (`ExecStart=` of the egress unit).
const RULES_PATH: &str = "/usr/share/trustiot-agent/trustiot-agent-egress.nft";
/// The drop-in for nftables.service, and where the image installs it.
const NFTABLES_RELOAD: &str = include_str!("../systemd/nftables-reload.conf");
const NFTABLES_DROP_IN: &str =
    "/usr/lib/systemd/system/nftables.service.d/trustiot-agent-egress.conf";

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
        self.put("/usr/local/bin/check-gateway", CHECK);
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

/// systemd as PID 1 in the container, with the units installed as the image installs them.
/// `None` (and a printed reason) when the container cannot run systemd.
fn systemd_lab() -> Option<Lab> {
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
    // The stand-in for the agent: at the moment it starts, can its account reach the gateway's
    // loopback service? It records the answer in its state directory and stays up.
    lab.put(
        "/usr/bin/trustiot-agent",
        "#!/bin/bash\n\
         if (: <> /dev/tcp/127.0.0.1/8080) 2> /dev/null; then outcome=reached; else outcome=refused; fi\n\
         echo \"$outcome\" >> \"$STATE_DIRECTORY/probes\"\n\
         exec sleep infinity\n",
    );
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
    let agent = "trustiot-agent.service";
    let egress = "trustiot-agent-egress.service";
    assert!(
        lab.sh("(: <> /dev/tcp/127.0.0.1/8080)").status.success(),
        "root reaches the service"
    );

    // Starting the agent starts nftables.service, then loads the rules, then starts the agent,
    // whose first connection is already refused.
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

/// The unix sockets the agent's sandbox hides (`InaccessiblePaths=` in the unit,
/// Laysi/trustiot-v2#826): the system bus, under both its names (D-Bus's default address is the
/// one under /var/run, a link to /run), avahi's socket, which also answers `mdns4_minimal`, and the
/// local sshd's unix socket (`sshd-unix-local.socket`), through which a key in the account's home
/// would open a session outside the sandbox that reaches the other two.
const HIDDEN_SOCKETS: [&str; 4] = [
    "/run/dbus/system_bus_socket",
    "/var/run/dbus/system_bus_socket",
    "/run/avahi-daemon/socket",
    "/run/ssh-unix-local/socket",
];

/// A stand-in for the socket units systemd's ssh generator writes on the gateway: the same
/// listening path and mode, and an instance per connection that closes it. No sshd is installed:
/// what the test needs is a socket the agent's account can reach outside the sandbox.
const SSH_UNIX_LOCAL_SOCKET: &str = "[Unit]\nDescription=Stand-in for sshd-unix-local.socket\n\n\
     [Socket]\nListenStream=/run/ssh-unix-local/socket\nSocketMode=0666\nAccept=yes\n\n\
     [Install]\nWantedBy=sockets.target\n";
const SSH_UNIX_LOCAL_SERVICE: &str = "[Unit]\nDescription=Stand-in for sshd-unix-local@.service\n\n\
     [Service]\nExecStart=/bin/true\nStandardInput=socket\n";

/// Connects to each unix stream socket it is given and prints `<path> reached` or
/// `<path> refused <the error>`.
const UNIX_PROBE: &str = r#"#!/usr/bin/perl
use strict;
use warnings;
use IO::Socket::UNIX;
use Socket qw(SOCK_STREAM);

for my $path (@ARGV) {
    if (IO::Socket::UNIX->new(Type => SOCK_STREAM, Peer => $path)) {
        print "$path reached\n";
    } else {
        print "$path refused $!\n";
    }
}
"#;

/// The stand-in for the agent under the unit's sandbox: it probes the hidden sockets as it starts
/// and appends what it found, then `end`, to `sockets` in its state directory; then it speaks
/// sd_notify as the agent does, as the unit's main process (`NotifyAccess=main`): `READY=1`, then
/// `WATCHDOG=1` every second. It probes again whenever a file `probe-now` appears there.
const SANDBOX_STAND_IN: &str = r#"#!/usr/bin/perl
use strict;
use warnings;
use IO::Socket::UNIX;
use Socket qw(SOCK_DGRAM);

my $state = $ENV{STATE_DIRECTORY} or die "no STATE_DIRECTORY\n";
my $notify_socket = $ENV{NOTIFY_SOCKET} or die "no NOTIFY_SOCKET\n";
$notify_socket =~ s/^@/\0/;
my @hidden = qw(HIDDEN);

sub probe {
    open(my $found, '-|', '/usr/local/bin/unix-probe', @hidden) or die "unix-probe: $!\n";
    my @lines = <$found>;
    close($found) or die "unix-probe failed\n";
    open(my $out, '>>', "$state/sockets") or die "$state/sockets: $!\n";
    print $out @lines, "end\n";
    close($out) or die "$state/sockets: $!\n";
}

sub notify {
    my ($message) = @_;
    my $socket = IO::Socket::UNIX->new(Type => SOCK_DGRAM, Peer => $notify_socket)
        or die "connecting to NOTIFY_SOCKET: $!\n";
    defined($socket->send($message)) or die "sending $message: $!\n";
}

probe();
notify("READY=1");
while (1) {
    sleep 1;
    notify("WATCHDOG=1");
    probe() if unlink("$state/probe-now");
}
"#;

/// The stand-in's state directory.
const STAND_IN_STATE: &str = "/var/lib/trustiot-agent";

/// How long systemd may take to record a `WATCHDOG=1` from the sandbox stand-in, which sends one
/// every second.
const WATCHDOG_BOUND: Duration = Duration::from_secs(60);

/// What the sandbox stand-in found in its `count`-th probe, by socket, once it has recorded that
/// many.
fn socket_probe(lab: &Lab, count: usize) -> BTreeMap<String, String> {
    lab.ok(&format!(
        "for i in $(seq 600); do \
         [ \"$(grep -cx end {STAND_IN_STATE}/sockets 2> /dev/null)\" -ge {count} ] && exit 0; \
         sleep 0.1; done; echo 'the stand-in did not record {count} socket probes within 60 s' >&2; \
         exit 1"
    ));
    let recorded = lab.ok(&format!("cat {STAND_IN_STATE}/sockets"));
    let mut probes = vec![BTreeMap::new()];
    for line in recorded.lines() {
        if line == "end" {
            probes.push(BTreeMap::new());
        } else {
            let (path, outcome) = line.split_once(' ').unwrap();
            probes
                .last_mut()
                .unwrap()
                .insert(path.to_owned(), outcome.to_owned());
        }
    }
    let found: BTreeMap<String, String> = probes.swap_remove(count - 1);
    let mut expected = HIDDEN_SOCKETS.to_vec();
    expected.sort_unstable();
    assert_eq!(
        found.keys().map(String::as_str).collect::<Vec<_>>(),
        expected,
        "{recorded}"
    );
    found
}

/// Asserts that every hidden socket had `outcome` in the stand-in's `count`-th probe.
fn assert_probe(lab: &Lab, count: usize, outcome: &str, when: &str) {
    let found = socket_probe(lab, count);
    for (path, seen) in &found {
        assert_eq!(seen, outcome, "{path} {when}: {found:?}");
    }
}

/// Starts (or restarts) the agent's unit with a fresh record of socket probes. `systemctl` returns
/// only once the stand-in sent `READY=1` (`Type=notify`), and fails when it does not.
fn start_agent(lab: &Lab, verb: &str) {
    lab.ok(&format!(
        "rm -f {STAND_IN_STATE}/sockets {STAND_IN_STATE}/probe-now \
         && systemctl {verb} trustiot-agent.service"
    ));
}

/// Laysi/trustiot-v2#826: under the agent unit's sandbox, a process of the agent's account cannot
/// connect to the system bus or to avahi's socket, which it could otherwise use to have avahi send
/// mDNS on the LAN outside its egress rules, nor to the local sshd's unix socket, which would lead
/// out of the sandbox to both; sd_notify still works (`READY=1` and `WATCHDOG=1`). The unit is the
/// real one, `Type=notify` and the watchdog included; only the executable is a stand-in. The same
/// account reaches every one of these sockets outside the sandbox, and so does the stand-in once
/// the unit's `InaccessiblePaths=` is reset, so the refusals are that setting's doing. Paths that
/// do not exist do not keep the agent from starting (the `-` prefix).
#[test]
fn the_agents_sandbox_hides_dbus_avahi_and_the_local_sshd_and_keeps_sd_notify() {
    let Some(lab) = systemd_lab() else {
        return;
    };
    let agent = "trustiot-agent.service";
    lab.put("/usr/local/bin/unix-probe", UNIX_PROBE);
    lab.put(
        "/usr/bin/trustiot-agent",
        &SANDBOX_STAND_IN.replace("qw(HIDDEN)", &format!("qw({})", HIDDEN_SOCKETS.join(" "))),
    );
    lab.put(
        "/etc/systemd/system/sshd-unix-local.socket",
        SSH_UNIX_LOCAL_SOCKET,
    );
    lab.put(
        "/etc/systemd/system/sshd-unix-local@.service",
        SSH_UNIX_LOCAL_SERVICE,
    );
    // The real unit: without systemd_lab's drop-in for its shell stand-in.
    lab.ok(
        "chmod 0755 /usr/local/bin/unix-probe /usr/bin/trustiot-agent \
         && rm /etc/systemd/system/trustiot-agent.service.d/stand-in.conf \
         && systemctl daemon-reload && systemctl start sshd-unix-local.socket",
    );
    assert_eq!(property(&lab, agent, "Type"), "notify");
    assert_eq!(property(&lab, agent, "NotifyAccess"), "main");
    // Name resolution goes through avahi's socket as on the gateway (libnss-mdns).
    assert!(
        lab.ok("grep '^hosts:' /etc/nsswitch.conf")
            .contains("mdns4_minimal [NOTFOUND=return] dns"),
        "{}",
        lab.ok("cat /etc/nsswitch.conf")
    );

    // The sockets are there, and the agent's account reaches each of them outside the sandbox.
    let hidden = HIDDEN_SOCKETS.join(" ");
    let outside = |when: &str| {
        let found = lab.ok(&format!(
            "setpriv --reuid=trustiot-agent --regid=trustiot-agent --clear-groups \
             /usr/local/bin/unix-probe {hidden}"
        ));
        for path in HIDDEN_SOCKETS {
            assert!(
                found.lines().any(|line| line == format!("{path} reached")),
                "the agent's account outside the sandbox {when}: {found}"
            );
        }
    };
    outside("before the agent starts");

    // Under the sandbox: none of them. systemctl returned, so READY=1 arrived.
    start_agent(&lab, "start");
    assert_eq!(property(&lab, agent, "ActiveState"), "active");
    // The unit's watchdog is armed (systemd shows it only once the unit runs).
    assert_eq!(property(&lab, agent, "WatchdogUSec"), "30s");
    assert_probe(&lab, 1, "refused Permission denied", "in the sandbox");
    // Each WATCHDOG=1 moves the watchdog's timestamp; nothing else does once the unit runs.
    let pinged = || -> u64 {
        property(&lab, agent, "WatchdogTimestampMonotonic")
            .parse()
            .unwrap()
    };
    let first = pinged();
    let started = Instant::now();
    while pinged() <= first {
        assert!(
            started.elapsed() < WATCHDOG_BOUND,
            "systemd recorded no WATCHDOG=1 from the stand-in within {WATCHDOG_BOUND:?}"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
    assert_eq!(property(&lab, agent, "ActiveState"), "active");
    assert_eq!(property(&lab, agent, "NRestarts"), "0");

    // avahi's socket unit starts again, as on a package upgrade: its directory stays, and so does
    // the running agent's view of it.
    lab.ok("systemctl stop avahi-daemon.service avahi-daemon.socket");
    lab.ok("systemctl start avahi-daemon.socket");
    outside("after avahi's socket unit started again");
    lab.ok(&format!("touch {STAND_IN_STATE}/probe-now"));
    assert_probe(
        &lab,
        2,
        "refused Permission denied",
        "after avahi's socket unit started again",
    );

    // Without the setting (an empty assignment resets the list), the same stand-in reaches them.
    let drop_in = "/etc/systemd/system/trustiot-agent.service.d/no-hidden-paths.conf";
    lab.put(drop_in, "[Service]\nInaccessiblePaths=\n");
    lab.ok("systemctl daemon-reload");
    start_agent(&lab, "restart");
    assert_probe(&lab, 1, "reached", "with InaccessiblePaths= reset");
    lab.ok(&format!("rm {drop_in} && systemctl daemon-reload"));
    start_agent(&lab, "restart");
    assert_probe(
        &lab,
        1,
        "refused Permission denied",
        "with the setting back",
    );

    // A gateway without D-Bus, avahi and the local sshd: the paths are missing and the agent starts
    // all the same.
    lab.ok(
        "systemctl stop trustiot-agent.service avahi-daemon.service avahi-daemon.socket \
         dbus.service dbus.socket sshd-unix-local.socket \
         && rm -rf /run/dbus /run/avahi-daemon /run/ssh-unix-local",
    );
    start_agent(&lab, "start");
    assert_eq!(property(&lab, agent, "ActiveState"), "active");
    assert_probe(
        &lab,
        1,
        "refused No such file or directory",
        "with the paths missing",
    );
}

/// Name resolution for the agent's account does not depend on avahi (Laysi/trustiot-v2#826): with
/// the gateway's /etc/nsswitch.conf (`mdns4_minimal` before `dns`) and avahi's socket out of
/// reach, a DNS name that is not `.local` still resolves through the network's DNS server.
/// `mdns4_minimal` does not ask avahi about such names at all, so this shows that the step in front
/// of `dns` lets them through; it does not exercise a failing avahi connection, and it runs
/// outside the sandbox (the systemd test above cannot reach a DNS server: the rules refuse the
/// agent's account every address on the gateway itself).
#[test]
fn the_agent_resolves_dns_names_with_avahi_out_of_reach() {
    let Some(lab) = Lab::network() else {
        return;
    };
    lab.ok("systemd-sysusers");
    lab.ok(&format!("nft -f {RULES_PATH}"));
    assert!(
        lab.ok("grep '^hosts:' /etc/nsswitch.conf")
            .contains("files mdns4_minimal [NOTFOUND=return] dns"),
        "{}",
        lab.ok("cat /etc/nsswitch.conf")
    );
    lab.put("/usr/local/bin/unix-probe", UNIX_PROBE);
    assert_eq!(
        lab.ok(
            "chmod 0755 /usr/local/bin/unix-probe && setpriv --reuid=trustiot-agent \
             --regid=trustiot-agent --clear-groups /usr/local/bin/unix-probe \
             /run/avahi-daemon/socket"
        ),
        "/run/avahi-daemon/socket refused No such file or directory"
    );
    assert_eq!(lab.ok("lab resolve agent cloud.test"), "10.81.3.1");
}
