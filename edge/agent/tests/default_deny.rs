//! The agent account's default deny (Laysi/trustiot-v2#879): the rules file
//! `systemd/trustiot-agent-default-deny.nft`, the executable that fills its DNS allowance
//! (`src/bin/trustiot-agent-nameservers.rs`, the parsing in `src/nameservers.rs`) and its units
//! (`systemd/trustiot-agent-nameservers.service`, `.path`), joined into the egress rules the way
//! `README.md` (section "連線位址的限制") tells Laysi/trustiot-v2#866 to join them.
//!
//! No gateway is needed: the throwaway Debian 13 container of `tests/egress.rs` (the same image,
//! from `tests/egress/Containerfile`) stands in for it, with `tests/egress/lab.sh`'s network
//! (the peer namespace is the site's LAN, its DNS server and the cloud) and a few more services
//! on the peer: a fieldbus endpoint on TCP 502 and a second DNS server. The executables this
//! crate builds are copied in: the allowlist component (Laysi/trustiot-v2#864) fills the fieldbus
//! sets, the new one the nameserver sets.
//!
//! The first test runs nft and both executables directly, as root, in the container's network
//! namespace: what the agent's account reaches with the default deny loaded and its sets filled,
//! what it is refused (each refusal checked first without the default deny and for another
//! account, and counted by the default deny's own rule), how the DNS allowance follows
//! `/etc/resolv.conf`, and which loads keep or empty the sets. The second runs systemd as PID 1
//! with the units installed and joined as Laysi/trustiot-v2#866 will: the path unit follows
//! nameserver changes (written in place and renamed over the file), and the agent's start and the
//! reload of nftables.service fill the DNS allowance again after loads that empty it.
//!
//! The container needs podman or docker (a skip locally, a failure with `CI` set). The systemd
//! test needs podman and skips, also with `CI` set, when systemd does not come up in the
//! container, as `tests/egress.rs` does.

#![allow(
    clippy::unwrap_used,
    clippy::print_stderr,
    reason = "test code; a skipped test says why on stderr, and the evidence lines go there too"
)]

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use agent::allowlist::{Allowlist, Destination, TABLE};
use agent::nameservers::{self, Nameservers};
use testkit::container::{Container, container_runtime, runtime_output, unavailable, unique_name};

const DEFAULT_DENY: &str = include_str!("../systemd/trustiot-agent-default-deny.nft");
const RULES: &str = include_str!("../systemd/trustiot-agent-egress.nft");
const SYSUSERS: &str = include_str!("../systemd/trustiot-agent.sysusers");
const AGENT_UNIT: &str = include_str!("../systemd/trustiot-agent.service");
const EGRESS_UNIT: &str = include_str!("../systemd/trustiot-agent-egress.service");
const NFTABLES_RELOAD: &str = include_str!("../systemd/nftables-reload.conf");
const NAMESERVERS_SERVICE: &str = include_str!("../systemd/trustiot-agent-nameservers.service");
const NAMESERVERS_PATH: &str = include_str!("../systemd/trustiot-agent-nameservers.path");
const LAB: &str = include_str!("egress/lab.sh");
const CONTAINERFILE: &str = include_str!("egress/Containerfile");

/// Where the image installs the egress rules, and where Laysi/trustiot-v2#866 installs the
/// default deny that the egress rules include.
const RULES_PATH: &str = "/usr/share/trustiot-agent/trustiot-agent-egress.nft";
const DEFAULT_DENY_PATH: &str = "/usr/share/trustiot-agent/trustiot-agent-default-deny.nft";
/// The line Laysi/trustiot-v2#866 adds to the egress rules (README.md, "連線位址的限制").
const INCLUDE: &str = "include \"/usr/share/trustiot-agent/trustiot-agent-default-deny.nft\"\n";
/// The drop-in for nftables.service, and where the image installs it.
const NFTABLES_DROP_IN: &str =
    "/usr/lib/systemd/system/nftables.service.d/trustiot-agent-egress.conf";

/// The executables under test, and where Laysi/trustiot-v2#866 installs them.
const NAMESERVERS_EXECUTABLE: &str = env!("CARGO_BIN_EXE_trustiot-agent-nameservers");
const NAMESERVERS_INSTALLED: &str = "/usr/libexec/trustiot-agent/nameservers";
const ALLOWLIST_EXECUTABLE: &str = env!("CARGO_BIN_EXE_trustiot-agent-allowlist");
const ALLOWLIST_INSTALLED: &str = "/usr/libexec/trustiot-agent/allowlist";

/// The site's DNS server of `lab.sh` and the second one this file adds, on the same peer.
const NAMESERVER: &str = "10.81.3.1";
const SECOND_NAMESERVER: &str = "10.81.3.53";
const SECOND_NAMESERVER6: &str = "fd81:3::53";
/// A host beyond the site (RFC 5737), the final destination of a source-routed probe.
const FAR_AWAY: &str = "192.0.2.99";

/// The systemd container runs without an SELinux label, as in `tests/egress.rs`.
const SELINUX_OFF: &str = "label=disable";
/// How long systemd in the container may take to finish booting. It takes a second or two.
const BOOT_BOUND: Duration = Duration::from_secs(120);
/// How long the path unit may take to apply a changed `/etc/resolv.conf`; it takes milliseconds.
const PATH_BOUND: Duration = Duration::from_secs(120);
/// How long an automatic restart of the agent may take to come (`RestartSec=2s` for the first).
const RESTART_BOUND: Duration = Duration::from_secs(300);

/// The egress rules joined with the default deny, as Laysi/trustiot-v2#866 joins them: the
/// shipped file and the include line after it.
fn joined_rules() -> String {
    format!("{RULES}\n{INCLUDE}")
}

/// The file declares the table and the sets exactly as the two executables write their
/// elements, and replaces only its chains.
#[test]
fn the_file_declares_the_sets_the_executables_fill_and_replaces_only_its_chains() {
    let commands: Vec<&str> = DEFAULT_DENY
        .lines()
        .filter(|line| !line.starts_with('#') && !line.trim().is_empty())
        .collect();
    let declarations = format!(
        "{}{}",
        Allowlist::declarations(),
        Nameservers::declarations()
    );
    let declared: Vec<&str> = declarations.lines().collect();
    assert_eq!(commands[..declared.len()], declared[..]);
    assert_eq!(
        commands[declared.len()..declared.len() + 6],
        [
            format!("add chain inet {TABLE} output"),
            format!("add chain inet {TABLE} agent"),
            format!("add chain inet {TABLE} refuse"),
            format!("delete chain inet {TABLE} output"),
            format!("delete chain inet {TABLE} agent"),
            format!("delete chain inet {TABLE} refuse"),
        ]
    );
    for never in ["delete table", "flush ruleset", "flush set", "flush table"] {
        assert!(
            !commands.iter().any(|command| command.contains(never)),
            "the file has {never:?}"
        );
    }
    assert!(INCLUDE.contains(DEFAULT_DENY_PATH));
}

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
    let context = std::env::temp_dir().join(unique_name("default-deny-context"));
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
            name: unique_name("agent-default-deny"),
        };
        let mut all = vec!["run", "--detach", "--name", &container.name];
        all.extend_from_slice(args);
        let image_at = all.iter().position(|arg| *arg == "IMAGE").unwrap();
        all[image_at] = &image;
        let _id = runtime_output(runtime, &all);
        Self { container }
    }

    /// Makes /etc/resolv.conf the container's own file, as on the gateway: the runtime may
    /// bind-mount its own over it, which can be written in place but neither removed nor replaced
    /// by a rename (NetworkManager's way). The mount is the container's, so SYS_ADMIN in its
    /// namespace may remove it. (Not --dns none: podman refuses it with --network none.)
    fn own_resolv_conf(&self) {
        self.ok(&format!(
            "if grep -q ' {0} ' /proc/self/mountinfo; then umount {0}; fi; \
             grep -q ' {0} ' /proc/self/mountinfo && exit 1; : > {0}",
            nameservers::RESOLV_CONF
        ));
    }

    /// The gateway's network namespace without systemd, as in `tests/egress.rs`, with the
    /// agent's account, the files installed, the network up and only the Laysi/trustiot-v2#813
    /// rules loaded.
    fn network() -> Option<Self> {
        let Some(runtime) = container_runtime() else {
            unavailable("podman or docker");
            return None;
        };
        // The capabilities of tests/egress.rs's network container, for the same reasons. /tmp,
        // where the executables create their script files, is a tmpfs as in the systemd
        // container, rather than the image's overlay. NET_RAW only for the source-route probe,
        // which hands it to one probe's process.
        let lab = Self::run(
            runtime,
            &[
                "--network",
                "none",
                "--tmpfs",
                "/tmp",
                "--memory",
                "512m",
                "--cap-drop",
                "ALL",
                "--cap-add",
                "NET_ADMIN,SYS_ADMIN,SETUID,SETGID,NET_BIND_SERVICE,CHOWN,FOWNER,DAC_OVERRIDE,NET_RAW",
                "--security-opt",
                "no-new-privileges",
                "--entrypoint",
                "/bin/sleep",
                "IMAGE",
                "infinity",
            ],
        );
        lab.own_resolv_conf();
        lab.install();
        lab.ok("systemd-sysusers");
        lab.site();
        lab.ok(&format!("nft -f {RULES_PATH}"));
        Some(lab)
    }

    /// Writes the repository's files where the image installs them (the egress rules as main
    /// ships them, the default deny next to them), the helper and the two executables.
    fn install(&self) {
        self.put(RULES_PATH, RULES.as_bytes());
        self.put(DEFAULT_DENY_PATH, DEFAULT_DENY.as_bytes());
        self.put(
            "/usr/lib/sysusers.d/trustiot-agent.conf",
            SYSUSERS.as_bytes(),
        );
        self.put("/usr/local/bin/lab", LAB.as_bytes());
        self.ok("chmod 0755 /usr/local/bin/lab && mkdir -p /usr/libexec/trustiot-agent");
        for (built, installed) in [
            (NAMESERVERS_EXECUTABLE, NAMESERVERS_INSTALLED),
            (ALLOWLIST_EXECUTABLE, ALLOWLIST_INSTALLED),
        ] {
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
    }

    /// `lab network`, and on the peer a fieldbus endpoint on TCP 502 (every address) and a
    /// second DNS server on 10.81.3.53 and fd81:3::53 that answers as the first.
    fn site(&self) {
        self.ok("lab network");
        self.ok(&format!(
            // `background` names nsenter itself: setsid executes a command, not a shell function.
            "peer() {{ nsenter --net=/run/netns/peer \"$@\"; }}; \
             background() {{ setsid nsenter --net=/run/netns/peer \"$@\" \
                < /dev/null > /dev/null 2>&1 & }}; \
             peer ip addr add {SECOND_NAMESERVER}/24 dev veth1 \
             && peer ip -6 addr add {SECOND_NAMESERVER6}/64 dev veth1 nodad || exit 1; \
             background socat TCP4-LISTEN:502,fork,reuseaddr EXEC:/bin/true; \
             background socat TCP6-LISTEN:502,ipv6only=1,fork,reuseaddr EXEC:/bin/true; \
             background dnsmasq --keep-in-foreground --no-resolv --no-hosts \
                --bind-interfaces --listen-address={SECOND_NAMESERVER} \
                --listen-address={SECOND_NAMESERVER6} \
                --host-record=cloud.test,10.81.3.1,fd81:3::1 --pid-file=/run/dnsmasq-second.pid; \
             for i in $(seq 600); do \
                  peer ss -H -tln 'sport = :502' | grep -q . \
                  && peer ss -H -uln 'sport = :53' | grep -q '{SECOND_NAMESERVER}:' \
                  && peer ss -H -uln 'sport = :53' | grep -q '{SECOND_NAMESERVER6}' && exit 0; \
                  sleep 0.1; \
                done; echo 'the fieldbus endpoint or the second DNS server did not listen within \
                60 s' >&2; exit 1"
        ));
    }

    fn put(&self, path: &str, contents: &[u8]) {
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

    fn probe(&self, who: &str, proto: &str, address: &str, port: u16) -> String {
        self.ok(&format!("lab probe {who} {proto} {address} {port}"))
    }

    fn resolve(&self, who: &str) -> String {
        self.ok(&format!("lab resolve {who} cloud.test"))
    }

    /// Hands a list to the allowlist component as root, as its socket would: exit code and
    /// answer line.
    fn allowlist(&self, list: &Allowlist) -> (Option<i32>, String) {
        self.put("/root/request", list.encode().as_bytes());
        let output = self.sh(&format!("{ALLOWLIST_INSTALLED} < /root/request"));
        (
            output.status.code(),
            String::from_utf8_lossy(&output.stdout).trim().to_owned(),
        )
    }

    /// Runs the nameserver executable as root: exit code and its log line.
    fn nameservers(&self) -> (Option<i32>, String) {
        let output = self.sh(NAMESERVERS_INSTALLED);
        (
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        )
    }

    fn resolv_conf(&self, contents: &str) {
        self.put(nameservers::RESOLV_CONF, contents.as_bytes());
    }

    /// The elements of one of the default deny's sets, as nft writes them; empty when the set
    /// has none.
    fn elements(&self, set: &str) -> BTreeSet<String> {
        let listed = self.ok(&format!("nft list set inet {TABLE} {set}"));
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

    /// Every set of the default deny with its elements.
    fn sets(&self) -> BTreeMap<&'static str, BTreeSet<String>> {
        [
            "fieldbus4",
            "fieldbus6",
            nameservers::SET_V4,
            nameservers::SET_V6,
        ]
        .into_iter()
        .map(|set| (set, self.elements(set)))
        .collect()
    }

    fn table_exists(&self) -> bool {
        self.sh(&format!("nft list table inet {TABLE}"))
            .status
            .success()
    }

    /// The default deny's chains, without counter values.
    fn chains(&self) -> String {
        self.ok(&format!(
            "set -o pipefail; for chain in output agent refuse; do nft list chain inet {TABLE} \
             $chain; done | sed -E 's/counter packets [0-9]+ bytes [0-9]+/counter/g'"
        ))
    }

    /// Packets the default deny's refusal counted.
    fn denied(&self) -> u64 {
        self.counter("default deny")
    }

    /// The packets the default deny's rule with this comment counted.
    fn counter(&self, comment: &str) -> u64 {
        self.ok(&format!(
            "set -o pipefail; nft list chain inet {TABLE} agent | grep 'comment \"{comment}\"' \
             | sed -E 's/.*counter packets ([0-9]+) .*/\\1/'"
        ))
        .parse()
        .unwrap()
    }

    /// Sends one UDP datagram as `who` to port 53 of `FAR_AWAY`, loosely source-routed (IPv4
    /// option LSRR, which an unprivileged socket may set) through the nameserver: the header's
    /// destination is the nameserver, the real one is in the option. What `lab probe` reports,
    /// and socat's error output. With `net_raw` the probe's process holds `CAP_NET_RAW`: some
    /// kernels refuse the option to a socket without it, some do not.
    fn source_routed(&self, who: &str, net_raw: bool) -> (String, String) {
        let account = match who {
            "agent" => "setpriv --reuid=trustiot-agent --regid=trustiot-agent --clear-groups",
            _ => "setpriv --reuid=nobody --regid=nogroup --clear-groups",
        };
        let caps = if net_raw {
            " --inh-caps=+net_raw --ambient-caps=+net_raw"
        } else {
            ""
        };
        // LSRR (0x83), length 7, pointer 4, the first hop 10.81.3.1 (0a 51 03 01), end of list.
        let output = self.sh(&format!(
            "printf x | timeout 60 {account}{caps} socat -u - \
             UDP4-DATAGRAM:{FAR_AWAY}:53,ip-options=x8307040a51030100"
        ));
        let error = String::from_utf8_lossy(&output.stderr).into_owned();
        let outcome = if output.status.success() {
            "ok"
        } else if error.contains("Operation not permitted") {
            "not-permitted"
        } else {
            "error"
        };
        (outcome.to_owned(), error)
    }

    /// Packets the Laysi/trustiot-v2#813 refusals counted, all rules together.
    fn refused_by_813(&self) -> u64 {
        self.ok("lab counters")
            .lines()
            .map(|line| line.rsplit_once(' ').unwrap().1.parse::<u64>().unwrap())
            .sum()
    }
}

fn set(elements: &[&str]) -> BTreeSet<String> {
    elements.iter().map(|&element| element.to_owned()).collect()
}

fn list(entries: &[(&str, u16)]) -> Allowlist {
    Allowlist::new(
        entries
            .iter()
            .map(|(address, port)| Destination::new(address.parse().unwrap(), *port).unwrap()),
    )
    .unwrap()
}

/// A destination the default deny refuses to the agent's account, with what it stands for.
struct Refused {
    what: &'static str,
    proto: &'static str,
    address: &'static str,
    port: u16,
}

/// What the default deny refuses while the fieldbus sets hold 10.81.3.1 and fd81:3::1 on 502 and
/// the nameserver set 10.81.3.1. Every TCP destination has a listener on the peer, so another
/// account (and the agent's without the default deny) gets through.
const REFUSED: [Refused; 11] = [
    Refused {
        what: "a fieldbus port at an address the list does not have",
        proto: "tcp",
        address: "10.81.3.53",
        port: 502,
    },
    Refused {
        what: "another port of an address the list has",
        proto: "tcp",
        address: "10.81.3.1",
        port: 80,
    },
    Refused {
        what: "another port of an IPv6 address the list has",
        proto: "tcp",
        address: "fd81:3::1",
        port: 80,
    },
    Refused {
        what: "UDP to a destination of the list",
        proto: "udp",
        address: "10.81.3.1",
        port: 502,
    },
    Refused {
        what: "UDP to the cloud's port",
        proto: "udp",
        address: "10.81.3.1",
        port: 443,
    },
    Refused {
        what: "UDP other than DNS",
        proto: "udp",
        address: "10.81.3.1",
        port: 9,
    },
    Refused {
        what: "NTP, which systemd-timesyncd sends from its own account",
        proto: "udp",
        address: "10.81.3.1",
        port: 123,
    },
    Refused {
        what: "DNS over UDP to a DNS server that is not a nameserver of resolv.conf",
        proto: "udp",
        address: SECOND_NAMESERVER,
        port: 53,
    },
    Refused {
        what: "DNS over TCP to a DNS server that is not a nameserver of resolv.conf",
        proto: "tcp",
        address: SECOND_NAMESERVER,
        port: 53,
    },
    Refused {
        what: "DNS over IPv6 to a DNS server that is not a nameserver of resolv.conf",
        proto: "udp",
        address: SECOND_NAMESERVER6,
        port: 53,
    },
    Refused {
        what: "UDP to an IPv6 destination of the list",
        proto: "udp",
        address: "fd81:3::1",
        port: 502,
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
fn the_agents_account_reaches_only_the_allowlist_the_cloud_and_its_nameservers() {
    let Some(lab) = Lab::network() else {
        return;
    };
    lab.resolv_conf(&format!("nameserver {NAMESERVER}\n"));

    // The control: with only the Laysi/trustiot-v2#813 rules, the agent's account reaches every
    // destination the default deny will refuse, so each refusal below is the new file's doing.
    for destination in &REFUSED {
        assert_eq!(
            lab.probe(
                "agent",
                destination.proto,
                destination.address,
                destination.port
            ),
            "ok",
            "without the default deny, agent to {} ({} {} {})",
            destination.what,
            destination.proto,
            destination.address,
            destination.port
        );
    }
    assert_eq!(lab.resolve("agent"), NAMESERVER);
    eprintln!(
        "evidence: without the default deny the agent's account reaches all {} destinations",
        REFUSED.len()
    );

    // Neither executable creates the table: without the default deny both fail.
    let (code, answer) = lab.allowlist(&list(&[(NAMESERVER, 502)]));
    assert_eq!(code, Some(1), "{answer}");
    let (code, log) = lab.nameservers();
    assert_eq!(code, Some(1), "{log}");
    assert!(
        log.contains("did not apply the nameservers 10.81.3.1"),
        "{log}"
    );
    assert!(!lab.table_exists(), "an executable created the table");
    eprintln!("evidence: no default deny loaded: the nameserver executable exits {code:?}: {log}");

    // The egress rules joined with the default deny, as Laysi/trustiot-v2#866 joins them. Loading
    // them twice keeps one table with its four sets and three chains.
    lab.put(RULES_PATH, joined_rules().as_bytes());
    lab.ok(&format!("nft -f {RULES_PATH} && nft -f {RULES_PATH}"));
    let table = lab.ok(&format!("nft list table inet {TABLE}"));
    assert_eq!(table.matches("\tset ").count(), 4, "{table}");
    assert_eq!(table.matches("\tchain ").count(), 3, "{table}");
    assert_eq!(
        lab.ok("nft list tables | grep -c trustiot_agent_allow"),
        "1"
    );
    lab.ok("nft list table inet trustiot_agent > /dev/null");
    eprintln!("evidence: the default deny's table after loading the joined rules twice:\n{table}");

    // Empty sets: no fieldbus destination and no DNS, while the cloud's 443 stays open.
    assert_eq!(lab.probe("agent", "tcp", NAMESERVER, 502), "refused");
    assert_eq!(lab.resolve("agent"), "failed");
    assert_eq!(lab.probe("agent", "tcp", NAMESERVER, 443), "ok");
    eprintln!(
        "evidence: empty sets: agent to 10.81.3.1:502 refused, resolving fails, 10.81.3.1:443 ok"
    );

    // The components fill the sets; with `ok` the allowance is in force.
    let fieldbus = list(&[(NAMESERVER, 502), ("fd81:3::1", 502)]);
    assert_eq!(lab.allowlist(&fieldbus), (Some(0), "ok 2".to_owned()));
    let (code, log) = lab.nameservers();
    assert_eq!(code, Some(0), "{log}");
    assert_eq!(
        log,
        "the agent may reach the nameservers 10.81.3.1 on port 53"
    );
    let filled = BTreeMap::from([
        ("fieldbus4", set(&["10.81.3.1 . 502"])),
        ("fieldbus6", set(&["fd81:3::1 . 502"])),
        (nameservers::SET_V4, set(&[NAMESERVER])),
        (nameservers::SET_V6, set(&[])),
    ]);
    assert_eq!(lab.sets(), filled);

    let denied = lab.denied();
    let refused_by_813 = lab.refused_by_813();
    for (proto, address, port, what) in [
        ("tcp", NAMESERVER, 502, "a fieldbus destination of the list"),
        (
            "tcp",
            "fd81:3::1",
            502,
            "an IPv6 fieldbus destination of the list",
        ),
        ("tcp", NAMESERVER, 443, "the cloud's port"),
        ("tcp", "fd81:3::1", 443, "the cloud's port over IPv6"),
        (
            "tcp",
            SECOND_NAMESERVER,
            443,
            "the cloud's port at any address",
        ),
        (
            "tcp",
            "cloud.test",
            443,
            "the cloud by name, resolved through the nameserver",
        ),
        ("udp", NAMESERVER, 53, "DNS over UDP to the nameserver"),
        ("tcp", NAMESERVER, 53, "DNS over TCP to the nameserver"),
    ] {
        assert_eq!(
            lab.probe("agent", proto, address, port),
            "ok",
            "agent to {what}"
        );
        eprintln!("evidence: allowed: agent to {what} ({proto} {address} {port}): ok");
    }
    assert_eq!(lab.resolve("agent"), NAMESERVER);
    assert_eq!(
        lab.denied(),
        denied,
        "an allowed packet met the default deny"
    );

    // Every refusal: the agent's account is refused, the default deny counts it (and the
    // Laysi/trustiot-v2#813 rules do not), and another account gets through.
    for destination in &REFUSED {
        let before = lab.denied();
        assert_eq!(
            lab.probe(
                "agent",
                destination.proto,
                destination.address,
                destination.port
            ),
            refusal(destination.proto),
            "agent to {} ({} {} {})",
            destination.what,
            destination.proto,
            destination.address,
            destination.port
        );
        assert!(
            lab.denied() > before,
            "the default deny did not count the agent's packet to {}",
            destination.what
        );
        assert_eq!(
            lab.probe(
                "other",
                destination.proto,
                destination.address,
                destination.port
            ),
            "ok",
            "nobody to {}",
            destination.what
        );
        eprintln!(
            "evidence: refused: agent to {} ({} {} {}): {}; nobody: ok",
            destination.what,
            destination.proto,
            destination.address,
            destination.port,
            refusal(destination.proto)
        );
    }
    assert_eq!(
        lab.refused_by_813(),
        refused_by_813,
        "a Laysi/trustiot-v2#813 refusal counted a probe of the default deny"
    );

    // A source route through the nameserver: the header names the nameserver's port 53, which
    // the DNS allowance accepts, but the datagram is for another host. This kernel may refuse the
    // option itself to an account without CAP_NET_RAW (recorded, not required); with it, as on a
    // kernel that lets every account set it, nobody's datagram leaves and the default deny
    // refuses the agent's, as it refuses the same host without the route.
    assert_eq!(lab.probe("agent", "udp", FAR_AWAY, 53), "not-permitted");
    let (outcome, error) = lab.source_routed("other", false);
    eprintln!(
        "evidence: nobody without CAP_NET_RAW, UDP {FAR_AWAY}:53 source-routed: {outcome} {}",
        error.trim()
    );
    let (outcome, error) = lab.source_routed("other", true);
    assert_eq!(outcome, "ok", "nobody: {error}");
    let options = lab.counter("IP options");
    let (outcome, error) = lab.source_routed("agent", true);
    assert_eq!(outcome, "not-permitted", "agent: {error}");
    assert!(
        error.contains("sendto") || error.contains("write"),
        "refused on sending, not on setting the option: {error}"
    );
    assert!(
        lab.counter("IP options") > options,
        "the IP options rule did not count it"
    );
    eprintln!(
        "evidence: refused: agent (with CAP_NET_RAW) to UDP {FAR_AWAY}:53 source-routed through \
         the nameserver: not-permitted, counted by the IP options rule; nobody: ok"
    );

    // The DNS allowance follows /etc/resolv.conf: another nameserver is refused until the
    // executable runs, then allowed, and the old one is refused.
    lab.resolv_conf(&format!(
        "# NetworkManager\nnameserver {SECOND_NAMESERVER}\n"
    ));
    assert_eq!(lab.resolve("agent"), "failed");
    let (code, log) = lab.nameservers();
    assert_eq!(code, Some(0), "{log}");
    assert_eq!(lab.elements(nameservers::SET_V4), set(&[SECOND_NAMESERVER]));
    assert_eq!(lab.resolve("agent"), NAMESERVER);
    assert_eq!(lab.probe("agent", "udp", NAMESERVER, 53), "not-permitted");
    assert_eq!(lab.probe("agent", "tcp", NAMESERVER, 53), "refused");
    assert_eq!(lab.probe("agent", "tcp", SECOND_NAMESERVER, 53), "ok");
    eprintln!(
        "evidence: nameserver changed to {SECOND_NAMESERVER}: {log}; the agent resolves, DNS to {NAMESERVER} is refused"
    );
    // An IPv6 nameserver with a zone, and glibc's limit of three.
    lab.resolv_conf(&format!(
        "nameserver {SECOND_NAMESERVER6}%veth0\nnameserver 192.0.2.1\nnameserver 192.0.2.2\n\
         nameserver {NAMESERVER}\n"
    ));
    let (code, log) = lab.nameservers();
    assert_eq!(code, Some(0), "{log}");
    assert_eq!(
        lab.elements(nameservers::SET_V6),
        set(&[SECOND_NAMESERVER6])
    );
    assert_eq!(
        lab.elements(nameservers::SET_V4),
        set(&["192.0.2.1", "192.0.2.2"])
    );
    assert_eq!(lab.resolve("agent"), NAMESERVER);
    assert_eq!(lab.probe("agent", "udp", NAMESERVER, 53), "not-permitted");
    eprintln!(
        "evidence: three nameservers, IPv6 first: {log}; the fourth (10.81.3.1) stays refused"
    );
    // A full set replaced by another full set, in the one transaction.
    for (contents, expected) in [
        (
            "nameserver 192.0.2.1\nnameserver 192.0.2.2\nnameserver 192.0.2.3\n",
            ["192.0.2.1", "192.0.2.2", "192.0.2.3"],
        ),
        (
            "nameserver 192.0.2.4\nnameserver 192.0.2.5\nnameserver 192.0.2.6\n",
            ["192.0.2.4", "192.0.2.5", "192.0.2.6"],
        ),
    ] {
        lab.resolv_conf(contents);
        let (code, log) = lab.nameservers();
        assert_eq!(code, Some(0), "{log}");
        assert_eq!(lab.elements(nameservers::SET_V4), set(&expected));
        assert!(lab.elements(nameservers::SET_V6).is_empty());
    }
    eprintln!("evidence: three IPv4 nameservers replaced by three others");
    // No file: no nameserver, as for glibc.
    lab.ok(&format!("rm {}", nameservers::RESOLV_CONF));
    let (code, log) = lab.nameservers();
    assert_eq!(code, Some(0), "{log}");
    assert_eq!(log, "the agent may reach the nameservers none on port 53");
    assert!(
        lab.elements(nameservers::SET_V4).is_empty()
            && lab.elements(nameservers::SET_V6).is_empty()
    );
    lab.resolv_conf(&format!("nameserver {NAMESERVER}\n"));
    assert_eq!(lab.nameservers().0, Some(0));
    assert_eq!(lab.sets(), filled);

    // A load of the rules file (trustiot-agent-egress.service's start and reload, the agent's
    // ExecStartPre=, a manual `nft -f`) keeps the sets' elements and replaces the chains.
    let chains = lab.chains();
    lab.ok("nft add rule inet trustiot_agent_allow agent tcp dport 8080 accept");
    assert_ne!(lab.chains(), chains);
    lab.ok(&format!("nft -f {RULES_PATH}"));
    assert_eq!(
        lab.chains(),
        chains,
        "a load left a rule that the file does not have"
    );
    assert_eq!(lab.sets(), filled, "a load of the rules emptied the sets");
    assert_eq!(lab.probe("agent", "tcp", NAMESERVER, 502), "ok");
    assert_eq!(lab.resolve("agent"), NAMESERVER);
    eprintln!(
        "evidence: a load of the rules file keeps the sets: {:?}",
        lab.sets()
    );

    // A load with /etc/nftables.conf (nftables.service's start, restart and reload: its
    // `flush ruleset` is in the same transaction) creates the table anew: the sets are empty, and
    // the allowlist component still answers ok to the next list, so the agent cannot tell.
    lab.ok(&format!(
        "nft 'include \"/etc/nftables.conf\"; include \"{RULES_PATH}\"'"
    ));
    let empty = BTreeMap::from([
        ("fieldbus4", set(&[])),
        ("fieldbus6", set(&[])),
        (nameservers::SET_V4, set(&[])),
        (nameservers::SET_V6, set(&[])),
    ]);
    assert_eq!(lab.sets(), empty);
    assert_eq!(lab.chains(), chains);
    assert_eq!(lab.probe("agent", "tcp", NAMESERVER, 502), "refused");
    assert_eq!(lab.resolve("agent"), "failed");
    assert_eq!(lab.allowlist(&fieldbus), (Some(0), "ok 2".to_owned()));
    eprintln!(
        "evidence: a load with /etc/nftables.conf empties the sets; the next list answers ok 2"
    );

    // After `nft flush ruleset` the table is gone: both executables fail until the rules load
    // again, which brings the table back with empty sets.
    lab.ok("nft flush ruleset");
    let (code, answer) = lab.allowlist(&fieldbus);
    assert_eq!(code, Some(1), "{answer}");
    assert!(answer.starts_with("failed: "), "{answer}");
    assert_eq!(lab.nameservers().0, Some(1));
    lab.ok(&format!("nft -f {RULES_PATH}"));
    assert_eq!(lab.sets(), empty);
    eprintln!(
        "evidence: after nft flush ruleset: allowlist {answer:?}, nameservers exit 1; a load brings empty sets"
    );
}

/// systemd as PID 1 with the units installed as the image installs them and joined as
/// Laysi/trustiot-v2#866 will (README.md, "連線位址的限制"): the default deny included by the
/// egress rules, the nameserver executable after the agent's reload of the rules and after the
/// reload of nftables.service, and the path unit wanted by the agent. `None` (and a printed
/// reason) when the container cannot run systemd.
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
    lab.own_resolv_conf();
    lab.install();
    lab.put(RULES_PATH, joined_rules().as_bytes());
    for (path, contents) in [
        ("/usr/lib/systemd/system/trustiot-agent.service", AGENT_UNIT),
        (
            "/usr/lib/systemd/system/trustiot-agent-egress.service",
            EGRESS_UNIT,
        ),
        (
            "/usr/lib/systemd/system/trustiot-agent-nameservers.service",
            NAMESERVERS_SERVICE,
        ),
        (
            "/usr/lib/systemd/system/trustiot-agent-nameservers.path",
            NAMESERVERS_PATH,
        ),
    ] {
        lab.put(path, contents.as_bytes());
    }
    // The drop-in as shipped, with the line Laysi/trustiot-v2#866 adds: after the reload's one
    // transaction, the nameservers again.
    lab.put(
        NFTABLES_DROP_IN,
        format!("{NFTABLES_RELOAD}ExecReload={NAMESERVERS_INSTALLED}\n").as_bytes(),
    );
    // The lines Laysi/trustiot-v2#866 adds to the agent's unit, as a drop-in here: the path unit
    // watches from before the agent starts, and the nameservers are filled after the unit's own
    // reload of the rules (a drop-in's ExecStartPre= runs after the unit's).
    lab.put(
        "/etc/systemd/system/trustiot-agent.service.d/default-deny.conf",
        format!(
            "[Unit]\nWants=trustiot-agent-nameservers.path\nAfter=trustiot-agent-nameservers.path\n\
             [Service]\nExecStartPre=+{NAMESERVERS_INSTALLED}\n"
        )
        .as_bytes(),
    );
    // The stand-in for the agent, which does not speak sd_notify; the probes run as its account
    // with setpriv, which the rules match the same way.
    lab.put(
        "/usr/bin/trustiot-agent",
        b"#!/bin/sh\nexec sleep infinity\n",
    );
    lab.put(
        "/etc/systemd/system/trustiot-agent.service.d/stand-in.conf",
        b"[Service]\nType=exec\nWatchdogSec=0\n",
    );
    lab.ok(
        "chmod 0755 /usr/bin/trustiot-agent && groupadd --system tss && systemd-sysusers \
         && systemctl daemon-reload",
    );
    lab.site();
    lab.resolv_conf(&format!("nameserver {NAMESERVER}\n"));
    Some(lab)
}

fn property(lab: &Lab, unit: &str, name: &str) -> String {
    lab.ok(&format!("systemctl show --property={name} --value {unit}"))
}

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

#[test]
fn the_dns_allowance_follows_resolv_conf_and_every_load_that_empties_it() {
    let Some(lab) = systemd_lab() else {
        return;
    };
    let agent = "trustiot-agent.service";

    // The agent's start loads the joined rules and fills the nameservers before the agent runs;
    // the path unit is watching.
    lab.ok(&format!("systemctl start {agent}"));
    for unit in [
        "nftables.service",
        "trustiot-agent-egress.service",
        "trustiot-agent-nameservers.path",
        agent,
    ] {
        assert_eq!(property(&lab, unit, "ActiveState"), "active", "{unit}");
    }
    assert_eq!(lab.elements(nameservers::SET_V4), set(&[NAMESERVER]));
    assert_eq!(lab.resolve("agent"), NAMESERVER);
    eprintln!(
        "evidence: after the agent's start the nameserver set is {:?} and the agent resolves",
        lab.elements(nameservers::SET_V4)
    );

    // A nameserver change, written in place: the path unit runs the service, under its own
    // sandbox, and the allowance follows.
    let runs = |lab: &Lab| -> String {
        property(lab, "trustiot-agent-nameservers.service", "InvocationID")
    };
    let before = runs(&lab);
    lab.ok(&format!(
        "printf 'nameserver {SECOND_NAMESERVER}\\n' > {}",
        nameservers::RESOLV_CONF
    ));
    await_until(
        "the nameserver set following an in-place write of resolv.conf",
        PATH_BOUND,
        || lab.elements(nameservers::SET_V4) == set(&[SECOND_NAMESERVER]),
    );
    assert_ne!(runs(&lab), before, "the path unit did not run the service");
    assert_eq!(
        property(&lab, "trustiot-agent-nameservers.service", "Result"),
        "success"
    );
    assert_eq!(lab.resolve("agent"), NAMESERVER);
    assert_eq!(lab.probe("agent", "udp", NAMESERVER, 53), "not-permitted");
    eprintln!(
        "evidence: resolv.conf written in place with {SECOND_NAMESERVER}: the set follows, the agent resolves through it, DNS to {NAMESERVER} is refused"
    );

    // Replaced by a rename, as NetworkManager writes it, with an IPv6 nameserver.
    lab.ok(&format!(
        "printf 'nameserver {SECOND_NAMESERVER6}\\n' > /etc/resolv.conf.new \
         && mv /etc/resolv.conf.new {}",
        nameservers::RESOLV_CONF
    ));
    await_until(
        "the nameserver set following a rename over resolv.conf",
        PATH_BOUND,
        || {
            lab.elements(nameservers::SET_V6) == set(&[SECOND_NAMESERVER6])
                && lab.elements(nameservers::SET_V4).is_empty()
        },
    );
    assert_eq!(lab.resolve("agent"), NAMESERVER);
    assert_eq!(
        lab.probe("agent", "udp", SECOND_NAMESERVER, 53),
        "not-permitted"
    );
    eprintln!(
        "evidence: resolv.conf replaced by a rename with {SECOND_NAMESERVER6}: the IPv6 set follows and the IPv4 set is empty"
    );
    lab.ok(&format!(
        "printf 'nameserver {NAMESERVER}\\n' > /etc/resolv.conf.new && mv /etc/resolv.conf.new {}",
        nameservers::RESOLV_CONF
    ));
    await_until("the nameserver set back on 10.81.3.1", PATH_BOUND, || {
        lab.elements(nameservers::SET_V4) == set(&[NAMESERVER])
    });
    let journal = lab.ok("journalctl -t trustiot-agent-nameservers --no-pager -o cat");
    eprintln!("evidence: the service's journal:\n{journal}");

    // The fieldbus allowance, from the allowlist component.
    let fieldbus = list(&[(NAMESERVER, 502)]);
    assert_eq!(lab.allowlist(&fieldbus), (Some(0), "ok 1".to_owned()));
    assert_eq!(lab.probe("agent", "tcp", NAMESERVER, 502), "ok");
    let filled = BTreeMap::from([
        ("fieldbus4", set(&["10.81.3.1 . 502"])),
        ("fieldbus6", set(&[])),
        (nameservers::SET_V4, set(&[NAMESERVER])),
        (nameservers::SET_V6, set(&[])),
    ]);
    assert_eq!(lab.sets(), filled);

    // A reload of the egress unit keeps every set.
    lab.ok("systemctl reload trustiot-agent-egress.service");
    assert_eq!(lab.sets(), filled);
    assert_eq!(lab.probe("agent", "tcp", NAMESERVER, 502), "ok");
    eprintln!(
        "evidence: systemctl reload trustiot-agent-egress.service keeps the sets: {:?}",
        lab.sets()
    );

    // A reload of nftables.service (one transaction with /etc/nftables.conf's flush) empties the
    // sets while the agent keeps running; the drop-in's second line fills the nameservers again,
    // the fieldbus sets wait for the agent's next list (Laysi/trustiot-v2#865).
    let invocation = property(&lab, agent, "InvocationID");
    lab.ok("systemctl reload nftables.service");
    assert_eq!(
        property(&lab, agent, "InvocationID"),
        invocation,
        "the agent was restarted"
    );
    assert_eq!(lab.elements("fieldbus4"), set(&[]));
    assert_eq!(lab.elements(nameservers::SET_V4), set(&[NAMESERVER]));
    assert_eq!(lab.resolve("agent"), NAMESERVER);
    assert_eq!(lab.probe("agent", "tcp", NAMESERVER, 502), "refused");
    assert_eq!(lab.allowlist(&fieldbus), (Some(0), "ok 1".to_owned()));
    assert_eq!(lab.probe("agent", "tcp", NAMESERVER, 502), "ok");
    eprintln!(
        "evidence: systemctl reload nftables.service: fieldbus sets empty until the next list, nameservers filled by the drop-in"
    );

    // `nft flush ruleset` outside systemd, then the agent's process ends: the automatic restart
    // loads the rules (the table is new, every set empty) and fills the nameservers before the
    // agent runs.
    lab.ok("nft flush ruleset");
    let (code, answer) = lab.allowlist(&fieldbus);
    assert_eq!(code, Some(1), "{answer}");
    let restarts = |lab: &Lab| -> u64 { property(lab, agent, "NRestarts").parse().unwrap() };
    let before = restarts(&lab);
    let crashed = property(&lab, agent, "MainPID");
    lab.ok(&format!("kill -KILL {crashed}"));
    await_until("the agent's automatic restart", RESTART_BOUND, || {
        restarts(&lab) > before && property(&lab, agent, "ActiveState") == "active"
    });
    assert_eq!(lab.elements(nameservers::SET_V4), set(&[NAMESERVER]));
    assert_eq!(lab.elements("fieldbus4"), set(&[]));
    assert_eq!(lab.resolve("agent"), NAMESERVER);
    assert_eq!(lab.probe("agent", "tcp", NAMESERVER, 502), "refused");
    assert_eq!(lab.allowlist(&fieldbus), (Some(0), "ok 1".to_owned()));
    eprintln!(
        "evidence: after nft flush ruleset and an automatic restart: nameservers filled again, fieldbus sets empty until the next list"
    );

    // A restart of nftables.service restarts the agent, whose start fills the nameservers.
    lab.ok("systemctl restart nftables.service");
    await_until(
        "the agent active after a restart of nftables.service",
        RESTART_BOUND,
        || {
            lab.sh("systemctl list-jobs --no-legend | grep -q .")
                .status
                .code()
                == Some(1)
                && property(&lab, agent, "ActiveState") == "active"
        },
    );
    assert_eq!(lab.elements(nameservers::SET_V4), set(&[NAMESERVER]));
    assert_eq!(lab.resolve("agent"), NAMESERVER);
    eprintln!(
        "evidence: systemctl restart nftables.service: the agent's restart filled the nameservers"
    );
}
