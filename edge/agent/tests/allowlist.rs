//! The fieldbus allowlist component (Laysi/trustiot-v2#864): the executable
//! `trustiot-agent-allowlist` (`src/bin/trustiot-agent-allowlist.rs`, the format in
//! `src/allowlist.rs`) and its units, `systemd/trustiot-agent-allowlist.socket` and
//! `systemd/trustiot-agent-allowlist@.service`.
//!
//! No gateway is needed: the throwaway Debian 13 container of `tests/egress.rs` (the same image,
//! from `tests/egress/Containerfile`) stands in for it, with the executable this crate builds
//! copied in. The first test runs the executable directly, as root, in the container's network
//! namespace with `tests/egress/lab.sh`'s network and the Laysi/trustiot-v2#813 rules loaded:
//! without the default deny's table a list fails and creates nothing; with a default deny shaped
//! like Laysi/trustiot-v2#865's loaded, each list replaces the sets and the agent's account
//! reaches exactly the listed destinations; a refused or failed request leaves the whole ruleset
//! as it was. The second runs systemd as PID 1 with the units installed as the image installs
//! them: only the agent's account can connect, and the instance runs as a dynamic account whose
//! only capability is `CAP_NET_ADMIN`.
//!
//! The container needs podman or docker (a skip locally, a failure with `CI` set). The systemd
//! test needs podman and skips, also with `CI` set, when systemd does not come up in the
//! container, as `tests/egress.rs` does: the hosted runners are not known to run systemd
//! containers, and `tests/systemd_unit.rs` checks the unit files everywhere.

#![allow(
    clippy::unwrap_used,
    clippy::print_stderr,
    reason = "test code; a skipped test says why on stderr"
)]

use std::collections::BTreeSet;
use std::io::Write;
use std::net::{IpAddr, Ipv4Addr};
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use agent::allowlist::{
    Allowlist, Destination, MAX_ENTRIES, MAX_REQUEST_BYTES, SET_V4, SET_V6, SOCKET, TABLE,
};
use testkit::container::{Container, container_runtime, runtime_output, unavailable, unique_name};

/// The executable under test, and where the image installs it (`ExecStart=` of the service).
const EXECUTABLE: &str = env!("CARGO_BIN_EXE_trustiot-agent-allowlist");
const INSTALLED: &str = "/usr/libexec/trustiot-agent/allowlist";
const SOCKET_UNIT: &str = include_str!("../systemd/trustiot-agent-allowlist.socket");
const SERVICE_UNIT: &str = include_str!("../systemd/trustiot-agent-allowlist@.service");
/// The Laysi/trustiot-v2#813 rules, which the component must leave alone, and where they go.
const RULES: &str = include_str!("../systemd/trustiot-agent-egress.nft");
const RULES_PATH: &str = "/usr/share/trustiot-agent/trustiot-agent-egress.nft";
const SYSUSERS: &str = include_str!("../systemd/trustiot-agent.sysusers");
const LAB: &str = include_str!("egress/lab.sh");
const CONTAINERFILE: &str = include_str!("egress/Containerfile");

/// The systemd container runs without an SELinux label, as in `tests/egress.rs`.
const SELINUX_OFF: &str = "label=disable";

/// How long systemd in the container may take to finish booting. It takes a second or two.
const BOOT_BOUND: Duration = Duration::from_secs(120);

/// The TCP part of a default deny shaped like Laysi/trustiot-v2#865's, which this issue leaves out:
/// the agent's account reaches over TCP only the destinations in the sets (what it does with other
/// protocols and DNS is Laysi/trustiot-v2#865's). Its chains are in the component's table, where a
/// rule can use the sets.
const DEFAULT_DENY_CHAINS: &str = "table inet trustiot_agent_allow {
    chain output {
        type filter hook output priority filter + 10; policy accept;
        meta skuid \"trustiot-agent\" meta l4proto tcp jump agent
    }
    chain agent {
        ip daddr . tcp dport @fieldbus4 accept
        ip6 daddr . tcp dport @fieldbus6 accept
        reject with tcp reset
    }
}
";

/// The default deny's file: the table and the sets as [`Allowlist::declarations`] declares them
/// (the component never creates them), then the chains.
fn default_deny() -> String {
    format!("{}{DEFAULT_DENY_CHAINS}", Allowlist::declarations())
}

/// The image of `tests/egress.rs`, built once per process and Containerfile content (with the
/// same tag, so the two test files share it), from an empty build context.
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
    let context = std::env::temp_dir().join(unique_name("allowlist-context"));
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
            name: unique_name("agent-allowlist"),
        };
        let mut all = vec!["run", "--detach", "--name", &container.name];
        all.extend_from_slice(args);
        let image_at = all.iter().position(|arg| *arg == "IMAGE").unwrap();
        all[image_at] = &image;
        let _id = runtime_output(runtime, &all);
        Self { container }
    }

    /// The gateway's network namespace without systemd, as in `tests/egress.rs`: `lab network`,
    /// the agent's account, and the Laysi/trustiot-v2#813 rules loaded.
    fn network() -> Option<Self> {
        let Some(runtime) = container_runtime() else {
            unavailable("podman or docker");
            return None;
        };
        // The capabilities of tests/egress.rs's network container, for the same reasons: nft and
        // the links, the peer namespace, switching accounts, the listeners and systemd-sysusers.
        // /tmp, where the executable creates its script file, is a tmpfs as in the systemd
        // container, rather than the image's overlay.
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
                "NET_ADMIN,SYS_ADMIN,SETUID,SETGID,NET_BIND_SERVICE,CHOWN,FOWNER,DAC_OVERRIDE",
                "--security-opt",
                "no-new-privileges",
                "--entrypoint",
                "/bin/sleep",
                "IMAGE",
                "infinity",
            ],
        );
        lab.put(RULES_PATH, RULES.as_bytes());
        lab.put(
            "/usr/lib/sysusers.d/trustiot-agent.conf",
            SYSUSERS.as_bytes(),
        );
        lab.put("/usr/local/bin/lab", LAB.as_bytes());
        lab.install_executable();
        lab.ok("chmod 0755 /usr/local/bin/lab && systemd-sysusers && lab network");
        lab.ok(&format!("nft -f {RULES_PATH}"));
        Some(lab)
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

    /// Copies the executable this crate built to where the image installs it.
    fn install_executable(&self) {
        self.ok("mkdir -p /usr/libexec/trustiot-agent");
        let copied = Command::new(self.container.runtime)
            .arg("cp")
            .arg(EXECUTABLE)
            .arg(format!("{}:{INSTALLED}", self.container.name))
            .output()
            .unwrap();
        assert!(
            copied.status.success(),
            "copying {EXECUTABLE}: {}",
            String::from_utf8_lossy(&copied.stderr)
        );
        self.ok(&format!(
            "chown root:root {INSTALLED} && chmod 0755 {INSTALLED}"
        ));
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

    /// Hands `request` to the executable as root, as the socket would: its exit code and its
    /// answer line. The answer is exactly one line.
    fn request(&self, request: &[u8]) -> (Option<i32>, String) {
        self.put("/root/request", request);
        let output = self.sh(&format!("{INSTALLED} < /root/request"));
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        assert!(
            stdout.ends_with('\n') && stdout.lines().count() == 1,
            "not one answer line: {stdout:?}, stderr {}",
            String::from_utf8_lossy(&output.stderr)
        );
        (output.status.code(), stdout.trim_end().to_owned())
    }

    /// The elements of one of the component's sets, as nft writes them (`<address> . <port>`).
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

    /// Asserts that the sets hold exactly `list`.
    fn assert_sets(&self, list: &Allowlist, when: &str) {
        let (mut v4, mut v6) = (BTreeSet::new(), BTreeSet::new());
        for destination in list.iter() {
            let element = format!("{} . {}", destination.address(), destination.port());
            if destination.address().is_ipv4() {
                v4.insert(element);
            } else {
                v6.insert(element);
            }
        }
        assert_eq!(self.elements(SET_V4), v4, "{SET_V4} {when}");
        assert_eq!(self.elements(SET_V6), v6, "{SET_V6} {when}");
    }

    /// What `nft <command>` lists, with the counters of the Laysi/trustiot-v2#813 rules left out
    /// (a refused connection counts there, and the comparisons are about the rules).
    fn listing(&self, command: &str) -> String {
        self.ok(&format!(
            "set -o pipefail; nft {command} | sed -E 's/counter packets [0-9]+ bytes [0-9]+/counter/g'"
        ))
    }

    /// The whole ruleset.
    fn ruleset(&self) -> String {
        self.listing("list ruleset")
    }

    /// Loads the default deny's file ([`default_deny`]).
    fn load_default_deny(&self) {
        self.put("/root/default-deny.nft", default_deny().as_bytes());
        self.ok("nft -f /root/default-deny.nft");
    }

    /// Whether the component's table exists.
    fn table_exists(&self) -> bool {
        self.sh(&format!("nft list table inet {TABLE}"))
            .status
            .success()
    }

    /// The default deny's two chains.
    fn chains(&self) -> String {
        ["output", "agent"]
            .map(|chain| self.listing(&format!("list chain inet {TABLE} {chain}")))
            .join("\n")
    }

    /// Every table but the component's.
    fn other_tables(&self) -> String {
        let ours = format!("table inet {TABLE}");
        self.ok("nft list tables")
            .lines()
            .filter(|table| *table != ours)
            .map(|table| self.listing(&format!("list {table}")))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn probe(&self, who: &str, address: &str, port: u16) -> String {
        self.ok(&format!("lab probe {who} tcp {address} {port}"))
    }
}

fn list(entries: &[(&str, u16)]) -> Allowlist {
    Allowlist::new(
        entries
            .iter()
            .map(|(address, port)| Destination::new(address.parse().unwrap(), *port).unwrap()),
    )
    .unwrap()
}

/// `count` IPv4 destinations from `first` upwards, on port 502.
fn many(first: Ipv4Addr, count: usize) -> Allowlist {
    let first = u32::from(first);
    Allowlist::new((0..count).map(|index| {
        let address = Ipv4Addr::from(first + u32::try_from(index).unwrap());
        Destination::new(IpAddr::V4(address), 502).unwrap()
    }))
    .unwrap()
}

#[test]
fn each_list_replaces_the_sets_and_nothing_else_changes() {
    let Some(lab) = Lab::network() else {
        return;
    };
    // The host's own rules, which the component must not touch either.
    lab.ok(
        "nft add table inet host_rules \
         && nft add chain inet host_rules input '{ type filter hook input priority 0; policy accept; }' \
         && nft add rule inet host_rules input tcp dport 22 accept",
    );
    let others = lab.other_tables();
    assert!(
        others.contains("table inet trustiot_agent {")
            && others.contains("table inet host_rules {"),
        "{others}"
    );
    // Without the default deny's table the component creates nothing: a list fails, so an applied
    // list always lands in the sets of a loaded default deny.
    let first = list(&[("10.81.3.1", 443), ("fd81:3::1", 443), ("192.0.2.10", 502)]);
    let before = lab.ruleset();
    let (code, answer) = lab.request(first.encode().as_bytes());
    assert_eq!(code, Some(1), "{answer}");
    assert!(answer.starts_with("failed: nft "), "{answer}");
    assert_eq!(lab.ruleset(), before);
    assert!(!lab.table_exists(), "the component created the table");
    eprintln!("evidence: no default deny loaded: exit {code:?}, {answer}; no table was created");

    // The default deny's file declares the table and the sets and loads the chains that use them.
    lab.load_default_deny();
    let chains = lab.chains();

    // The first list fills the sets and changes nothing else.
    assert_eq!(
        lab.request(first.encode().as_bytes()),
        (Some(0), "ok 3".to_owned())
    );
    lab.assert_sets(&first, "after the first list");
    let table = lab.ok(&format!("nft list table inet {TABLE}"));
    eprintln!("evidence: the table after the first list:\n{table}");
    assert_eq!(table.matches("\tset ").count(), 2, "{table}");
    assert!(
        table.contains("type ipv4_addr . inet_service") && table.contains("size 4096"),
        "{table}"
    );
    assert_eq!(lab.chains(), chains);
    assert_eq!(lab.other_tables(), others);

    // With that default deny, the agent's account reaches over TCP exactly the listed
    // destinations; other accounts are not affected.
    for (address, port, expected) in [
        ("10.81.3.1", 443, "ok"),
        ("10.81.3.1", 80, "refused"),
        ("fd81:3::1", 443, "ok"),
        ("fd81:3::1", 80, "refused"),
    ] {
        assert_eq!(
            lab.probe("agent", address, port),
            expected,
            "agent to {address} {port} with the first list"
        );
        eprintln!("evidence: first list, agent to {address} port {port}: {expected}");
    }
    assert_eq!(lab.probe("other", "10.81.3.1", 80), "ok");
    eprintln!("evidence: first list, nobody to 10.81.3.1 port 80: ok");

    // A changed list replaces the sets: what left is closed, what came is open, and the chain
    // that uses the sets is still there.
    let second = list(&[("10.81.3.1", 80), ("fd81:3::1", 80), ("192.0.2.10", 503)]);
    assert_eq!(
        lab.request(second.encode().as_bytes()),
        (Some(0), "ok 3".to_owned())
    );
    lab.assert_sets(&second, "after the second list");
    for (address, port, expected) in [
        ("10.81.3.1", 443, "refused"),
        ("10.81.3.1", 80, "ok"),
        ("fd81:3::1", 443, "refused"),
        ("fd81:3::1", 80, "ok"),
    ] {
        assert_eq!(
            lab.probe("agent", address, port),
            expected,
            "agent to {address} {port} with the second list"
        );
        eprintln!("evidence: second list, agent to {address} port {port}: {expected}");
    }
    assert_eq!(lab.other_tables(), others);

    // Anything but a complete list is refused whole: the answer says why, and the ruleset (the
    // sets, the chains, the other tables) is exactly as before.
    let header = "trustiot-agent-allowlist v1\n";
    let entry = "10.81.3.1 443\n";
    let too_many: String = (0..=MAX_ENTRIES)
        .map(|index| {
            let index = u32::try_from(index).unwrap();
            format!("{} 502\n", Ipv4Addr::from(0x0a01_0000 + index))
        })
        .collect();
    let refused: Vec<(&str, Vec<u8>)> = vec![
        ("an empty request", Vec::new()),
        (
            "another version",
            format!("trustiot-agent-allowlist v2\n{entry}end\n").into_bytes(),
        ),
        ("a cut list", format!("{header}{entry}").into_bytes()),
        (
            "an nft command",
            format!("{header}{entry}flush ruleset\nend\n").into_bytes(),
        ),
        (
            "a rule",
            format!("{header}{entry}add rule inet {TABLE} agent accept\nend\n").into_bytes(),
        ),
        (
            "an entry with a command after it",
            format!("{header}10.81.3.1 443; flush ruleset\nend\n").into_bytes(),
        ),
        (
            "an entry with a verdict",
            format!("{header}10.81.3.1 443 accept\nend\n").into_bytes(),
        ),
        (
            "an entry with a protocol",
            format!("{header}udp 10.81.3.1 53\nend\n").into_bytes(),
        ),
        (
            "a host name",
            format!("{header}cloud.test 443\nend\n").into_bytes(),
        ),
        (
            "a network",
            format!("{header}10.81.3.0/24 443\nend\n").into_bytes(),
        ),
        (
            "loopback",
            format!("{header}{entry}127.0.0.1 8080\nend\n").into_bytes(),
        ),
        (
            "the metadata address",
            format!("{header}169.254.169.254 80\nend\n").into_bytes(),
        ),
        ("port 0", format!("{header}10.81.3.1 0\nend\n").into_bytes()),
        (
            "a destination twice",
            format!("{header}{entry}{entry}end\n").into_bytes(),
        ),
        (
            "a NUL byte",
            format!("{header}10.81.3.1 443\0\nend\n").into_bytes(),
        ),
        (
            "more entries than allowed",
            format!("{header}{too_many}end\n").into_bytes(),
        ),
        (
            "more bytes than allowed",
            format!("{header}{}", "x".repeat(MAX_REQUEST_BYTES)).into_bytes(),
        ),
    ];
    for (what, request) in refused {
        let before = lab.ruleset();
        let (code, answer) = lab.request(&request);
        assert_eq!(code, Some(2), "{what}: {answer}");
        assert!(answer.starts_with("refused: "), "{what}: {answer}");
        assert_eq!(lab.ruleset(), before, "{what} changed the ruleset");
        eprintln!("evidence: {what}: exit {code:?}, {answer}; the ruleset is unchanged");
    }
    lab.assert_sets(&second, "after the refused requests");

    // A valid list that the kernel cannot apply changes nothing either, not even the set whose
    // part of the transaction came first: here fieldbus6 holds at most one element, so the kernel
    // refuses the second IPv6 destination, and the flush and the new elements of fieldbus4,
    // earlier in the same transaction, do not apply.
    lab.ok(&format!(
        "nft delete table inet {TABLE} \
         && nft add table inet {TABLE} \
         && nft add set inet {TABLE} {SET_V4} '{{ type ipv4_addr . inet_service; size 4096; }}' \
         && nft add element inet {TABLE} {SET_V4} '{{ 10.81.3.1 . 80 }}' \
         && nft add set inet {TABLE} {SET_V6} '{{ type ipv6_addr . inet_service; size 1; }}'"
    ));
    let two_v6 = list(&[("10.81.3.1", 443), ("fd81:3::1", 443), ("fd81:3::1", 80)]);
    let before = lab.ruleset();
    let (code, answer) = lab.request(two_v6.encode().as_bytes());
    assert_eq!(code, Some(1), "{answer}");
    assert!(answer.starts_with("failed: nft "), "{answer}");
    assert_eq!(lab.ruleset(), before);
    assert_eq!(
        lab.elements(SET_V4),
        BTreeSet::from(["10.81.3.1 . 80".to_owned()])
    );
    eprintln!(
        "evidence: two IPv6 destinations for a fieldbus6 of size 1: exit {code:?}, {answer}; \
         the ruleset is unchanged, fieldbus4 still holds only 10.81.3.1 . 80"
    );

    // Once the table is gone (`nft flush ruleset`, a reload of nftables.service), a list fails and
    // creates nothing, until the default deny's file is loaded again.
    lab.ok(&format!("nft delete table inet {TABLE}"));
    let (code, answer) = lab.request(first.encode().as_bytes());
    assert_eq!(code, Some(1), "{answer}");
    assert!(!lab.table_exists(), "the component created the table");
    lab.load_default_deny();
    assert_eq!(
        lab.request(first.encode().as_bytes()),
        (Some(0), "ok 3".to_owned())
    );
    lab.assert_sets(&first, "after the default deny was loaded again");
    assert_eq!(lab.chains(), chains);
    eprintln!(
        "evidence: table deleted: exit {code:?}, {answer}, no table created; after the default \
         deny was loaded again: ok 3"
    );

    // A full list replaces a full list of other destinations in one transaction, and an empty
    // list empties both sets.
    for full in [
        many(Ipv4Addr::new(10, 1, 0, 0), MAX_ENTRIES),
        many(Ipv4Addr::new(10, 2, 0, 0), MAX_ENTRIES),
    ] {
        assert_eq!(
            lab.request(full.encode().as_bytes()),
            (Some(0), format!("ok {MAX_ENTRIES}"))
        );
        lab.assert_sets(&full, "after a full list");
        let first = full.iter().next().unwrap();
        eprintln!(
            "evidence: a full list from {} ({} bytes): ok {MAX_ENTRIES}, the sets hold exactly it",
            first.address(),
            full.encode().len()
        );
    }
    assert_eq!(
        lab.request(Allowlist::default().encode().as_bytes()),
        (Some(0), "ok 0".to_owned())
    );
    lab.assert_sets(&Allowlist::default(), "after the empty list");
    assert_eq!(lab.chains(), chains);
    assert_eq!(lab.other_tables(), others);
    eprintln!(
        "evidence: the empty list: ok 0, both sets empty; the other tables never changed:\n{others}"
    );
}

/// systemd as PID 1 in the container, with the units and the executable installed as the image
/// installs them and the agent's account created. `None` (and a printed reason) when the
/// container cannot run systemd.
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
    lab.put(
        "/usr/lib/sysusers.d/trustiot-agent.conf",
        SYSUSERS.as_bytes(),
    );
    lab.put(
        "/usr/lib/systemd/system/trustiot-agent-allowlist.socket",
        SOCKET_UNIT.as_bytes(),
    );
    lab.put(
        "/usr/lib/systemd/system/trustiot-agent-allowlist@.service",
        SERVICE_UNIT.as_bytes(),
    );
    lab.install_executable();
    lab.ok("systemctl daemon-reload");
    Some(lab)
}

/// Sends the file `request` (in the container) to the socket as `who`; what came back.
fn send(lab: &Lab, who: &str, request: &str) -> Output {
    let account = match who {
        "agent" => "setpriv --reuid=trustiot-agent --regid=trustiot-agent --clear-groups",
        "other" => "setpriv --reuid=nobody --regid=nogroup --clear-groups",
        _ => unreachable!("unknown account {who}"),
    };
    // -t: how long socat waits for the answer after it sent the whole request.
    lab.sh(&format!(
        "timeout 60 {account} socat -t 30 - UNIX-CONNECT:{SOCKET} < {request}"
    ))
}

/// Waits until no instance of the service is loaded: each has ended and none stayed behind.
fn await_no_instances(lab: &Lab) {
    lab.ok(
        "for i in $(seq 600); do \
         [ -z \"$(systemctl list-units --all --no-legend 'trustiot-agent-allowlist@*')\" ] && exit 0; \
         sleep 0.1; done; systemctl list-units --all 'trustiot-agent-allowlist@*' >&2; \
         echo 'an instance was still loaded after 60 s' >&2; exit 1",
    );
}

#[test]
fn only_the_agents_account_reaches_the_component_which_holds_only_cap_net_admin() {
    let Some(lab) = systemd_lab() else {
        return;
    };
    let socket = "trustiot-agent-allowlist.socket";

    // The socket's group is the agent's: without the account, the socket does not start.
    assert!(
        !lab.sh(&format!("systemctl start {socket}"))
            .status
            .success(),
        "the socket started without the agent's group"
    );
    lab.ok(&format!(
        "systemctl reset-failed {socket} && systemd-sysusers && systemctl start {socket}"
    ));
    assert_eq!(
        lab.ok(&format!("stat -c '%U %G %a' {SOCKET}")),
        "root trustiot-agent 660"
    );
    assert_eq!(
        lab.ok("stat -c '%U %G %a' /run/trustiot-agent-allowlist"),
        "root root 755"
    );
    eprintln!(
        "evidence: without the agent's group the socket did not start; with it: {}",
        lab.ok(&format!(
            "stat -c '%n %U %G %a' /run/trustiot-agent-allowlist {SOCKET}"
        ))
    );

    // Before the default deny's table exists, the instance answers that nft failed and creates
    // nothing, under the same sandbox.
    let first = list(&[("10.81.3.1", 502), ("fd81:3::1", 8886)]);
    lab.put("/root/first", first.encode().as_bytes());
    let early = send(&lab, "agent", "/root/first");
    let early = String::from_utf8_lossy(&early.stdout).into_owned();
    assert!(early.starts_with("failed: nft "), "{early}");
    assert!(!lab.table_exists(), "the component created the table");
    eprintln!(
        "evidence: through the socket, before the table exists: {}",
        early.trim_end()
    );
    lab.put(
        "/root/declarations.nft",
        Allowlist::declarations().as_bytes(),
    );
    lab.ok("nft -f /root/declarations.nft");

    // The agent's account sends a list and gets the answer; the sets hold it.
    let sent = send(&lab, "agent", "/root/first");
    assert_eq!(
        String::from_utf8_lossy(&sent.stdout),
        "ok 2\n",
        "{}",
        String::from_utf8_lossy(&sent.stderr)
    );
    lab.assert_sets(&first, "after the agent's list");

    // Another account cannot connect, and changes nothing.
    let second = list(&[("10.81.3.1", 4196)]);
    lab.put("/root/second", second.encode().as_bytes());
    let other = send(&lab, "other", "/root/second");
    assert!(!other.status.success());
    assert!(
        String::from_utf8_lossy(&other.stderr).contains("Permission denied"),
        "{}",
        String::from_utf8_lossy(&other.stderr)
    );
    assert!(other.stdout.is_empty());
    lab.assert_sets(&first, "after another account tried");
    eprintln!(
        "evidence: the agent's account got \"ok 2\"; nobody got: {}",
        String::from_utf8_lossy(&other.stderr).trim()
    );

    // A refused request through the socket: the answer and the journal say why; the sets stay.
    let bad_request = b"trustiot-agent-allowlist v1\n10.81.3.1 4196\nflush ruleset\nend\n";
    lab.put("/root/bad", bad_request);
    let bad = send(&lab, "agent", "/root/bad");
    assert_eq!(
        String::from_utf8_lossy(&bad.stdout),
        "refused: line 3: not an IP address in canonical form\n"
    );
    lab.assert_sets(&first, "after a refused request");
    await_no_instances(&lab);
    let journal = lab.ok("for i in $(seq 600); do \
         journalctl --no-pager -o cat -t trustiot-agent-allowlist | grep -q 'refused a request' \
         && break; sleep 0.1; done; \
         journalctl --no-pager -o 'with-unit' -t trustiot-agent-allowlist");
    let uid = lab.ok("id -u trustiot-agent");
    let refused_line = format!(
        "refused a request of {} bytes; the sets are unchanged: line 3",
        bad_request.len()
    );
    assert!(
        journal.contains("applied 2 destinations (1 IPv4, 1 IPv6)")
            && journal.contains(&refused_line),
        "{journal}"
    );
    // The instance's name carries the connecting process's PID and UID.
    assert!(
        journal
            .lines()
            .any(|line| line.contains(&format!("-{uid}.service")) && line.contains("applied")),
        "no instance of UID {uid}: {journal}"
    );
    eprintln!("evidence: the journal (the agent's UID is {uid}):\n{journal}");

    // One instance at a time: while a client keeps one busy, another connection is closed
    // without an answer and changes nothing; once the first ends, the second goes through.
    // The holder's request never ends: socat waits for input from a sleep (the image has no
    // pkill, so the sleep's PID is kept to end it).
    lab.ok(&format!(
        "setsid bash -c '{{ sleep 600 & echo $! > /root/holder.pid; wait; }} | setpriv \
         --reuid=trustiot-agent --regid=trustiot-agent --clear-groups \
         socat -t 30 - UNIX-CONNECT:{SOCKET}' < /dev/null > /dev/null 2>&1 &"
    ));
    lab.ok(
        "for i in $(seq 600); do \
         [ -n \"$(systemctl list-units --state=active --no-legend 'trustiot-agent-allowlist@*')\" ] \
         && exit 0; sleep 0.1; done; echo 'no instance became active within 60 s' >&2; exit 1",
    );
    let busy = send(&lab, "agent", "/root/second");
    assert!(busy.stdout.is_empty(), "{busy:?}");
    lab.assert_sets(&first, "while another instance ran");
    lab.ok("test -s /root/holder.pid && kill \"$(cat /root/holder.pid)\"");
    await_no_instances(&lab);
    let after = send(&lab, "agent", "/root/second");
    assert_eq!(String::from_utf8_lossy(&after.stdout), "ok 1\n");
    lab.assert_sets(&second, "after the busy instance ended");
    eprintln!(
        "evidence: while one instance ran, a second connection got no answer and the sets \
         stayed; after it ended the same request got \"ok 1\""
    );

    // What the instance runs with: replace the executable for one connection with one that
    // reports its credentials and tries to write outside its private /tmp.
    lab.put(
        "/run/systemd/system/trustiot-agent-allowlist@.service.d/probe.conf",
        b"[Service]\nExecStart=\nExecStart=/bin/sh -c 'grep -E \"^(Uid|Gid|Cap[A-Za-z]+|NoNewPrivs|Seccomp):\" /proc/self/status; touch /etc/probe /var/lib/probe /tmp/probe 2>&1; true'\n",
    );
    lab.ok("systemctl daemon-reload");
    let report = send(&lab, "agent", "/root/second");
    let report = String::from_utf8_lossy(&report.stdout).into_owned();
    eprintln!("evidence: what an instance runs with:\n{report}");
    let field = |name: &str| -> Vec<String> {
        report
            .lines()
            .find_map(|line| line.strip_prefix(&format!("{name}:")))
            .unwrap_or_else(|| panic!("no {name} in {report}"))
            .split_whitespace()
            .map(str::to_owned)
            .collect()
    };
    for name in ["Uid", "Gid"] {
        let ids = field(name);
        assert_eq!(ids.len(), 4, "{report}");
        assert!(ids.iter().all(|id| id != "0"), "{name} is root: {report}");
        assert!(
            ids.iter().all(|id| *id != uid),
            "{name} is the agent's: {report}"
        );
    }
    // CAP_NET_ADMIN is capability 12: 0x1000, in every set that matters, and nothing else.
    for name in ["CapPrm", "CapEff", "CapBnd", "CapAmb"] {
        assert_eq!(field(name), ["0000000000001000"], "{name}: {report}");
    }
    assert_eq!(field("NoNewPrivs"), ["1"], "{report}");
    assert_eq!(field("Seccomp"), ["2"], "{report}");
    for path in ["/etc/probe", "/var/lib/probe"] {
        assert!(
            report.contains(&format!(
                "touch: cannot touch '{path}': Read-only file system"
            )),
            "{path}: {report}"
        );
    }
    assert!(!report.contains("'/tmp/probe'"), "{report}");
    lab.ok(
        "rm -r /run/systemd/system/trustiot-agent-allowlist@.service.d && systemctl daemon-reload",
    );
    await_no_instances(&lab);

    // The real executable again, under the same sandbox: a list still applies.
    let sent = send(&lab, "agent", "/root/first");
    assert_eq!(String::from_utf8_lossy(&sent.stdout), "ok 2\n");
    lab.assert_sets(&first, "at the end");
}
