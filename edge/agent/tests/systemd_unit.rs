//! The systemd unit and its account (Laysi/trustiot-v2#86, acceptance criterion 4): the agent runs
//! as its own non-root account, systemd learns of readiness and liveness from the agent, and the
//! sandbox lines stay. With `systemd-analyze` on the machine the unit must also pass `verify` and
//! score at most [`MAX_EXPOSURE`] in `security`; without it these two skip, unless `CI` is set
//! (CI's runners have it).

#![allow(
    clippy::unwrap_used,
    reason = "test helpers unwrap like the tests themselves (clippy.toml allows it in tests)"
)]

use std::collections::BTreeMap;
use std::process::Command;

const UNIT: &str = include_str!("../systemd/trustiot-agent.service");
const SYSUSERS: &str = include_str!("../systemd/trustiot-agent.sysusers");
const ACCOUNT: &str = "trustiot-agent";
/// The unit that loads the account's egress rules (Laysi/trustiot-v2#813), and the rules.
const EGRESS_UNIT: &str = include_str!("../systemd/trustiot-agent-egress.service");
const EGRESS: &str = "trustiot-agent-egress.service";
const RULES: &str = include_str!("../systemd/trustiot-agent-egress.nft");
/// Where the image installs the rules.
const EGRESS_RULES: &str = "/usr/share/trustiot-agent/trustiot-agent-egress.nft";
/// The drop-in for nftables.service, and where the image installs it.
const NFTABLES_RELOAD: &str = include_str!("../systemd/nftables-reload.conf");
const NFTABLES_DROP_IN: &str =
    "/usr/lib/systemd/system/nftables.service.d/trustiot-agent-egress.conf";
/// The highest `systemd-analyze security` exposure allowed; the unit scores 1.5 on 2026-10-21
/// (what is left: network access, the TPM device, the tss group, /proc/meminfo), and still 1.5
/// with netlink sockets allowed on 2026-10-31.
const MAX_EXPOSURE: f64 = 2.0;

/// The unit's settings: section, then key, then every value in order (a key may repeat).
fn settings() -> BTreeMap<String, BTreeMap<String, Vec<String>>> {
    parse(UNIT)
}

/// A unit file's settings: section, then key, then every value in order (a key may repeat).
fn parse(unit: &str) -> BTreeMap<String, BTreeMap<String, Vec<String>>> {
    let mut sections: BTreeMap<String, BTreeMap<String, Vec<String>>> = BTreeMap::new();
    let mut section = String::new();
    for line in unit.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(name) = line
            .strip_prefix('[')
            .and_then(|rest| rest.strip_suffix(']'))
        {
            section = name.to_owned();
            continue;
        }
        let (key, value) = line.split_once('=').unwrap();
        assert!(!section.is_empty(), "{line} is outside a section");
        sections
            .entry(section.clone())
            .or_default()
            .entry(key.to_owned())
            .or_default()
            .push(value.to_owned());
    }
    sections
}

fn service(key: &str) -> Vec<String> {
    settings()["Service"].get(key).cloned().unwrap_or_default()
}

/// The value of a key that appears once.
fn single(key: &str) -> String {
    let values = service(key);
    assert_eq!(values.len(), 1, "{key}= should appear once: {values:?}");
    values[0].clone()
}

#[test]
fn the_agent_runs_as_its_own_account_and_never_as_root() {
    assert_eq!(single("User"), ACCOUNT);
    assert_eq!(single("Group"), ACCOUNT);
    assert!(
        service("DynamicUser").is_empty(),
        "the TPM group needs a stable account"
    );
    assert_eq!(
        single("SupplementaryGroups"),
        "tss",
        "/dev/tpmrm0 belongs to tss"
    );
    let users: Vec<&str> = SYSUSERS
        .lines()
        .filter(|line| line.starts_with("u "))
        .collect();
    assert_eq!(users.len(), 1);
    let fields: Vec<&str> = users[0].split_whitespace().collect();
    assert_eq!(
        (fields[1], fields[2]),
        (ACCOUNT, "-"),
        "a system account with an automatic ID"
    );
    assert!(users[0].ends_with("/usr/sbin/nologin"));
    assert_ne!(ACCOUNT, "root");
}

#[test]
fn systemd_learns_readiness_and_liveness_from_the_agent_and_always_restarts_it() {
    assert_eq!(single("Type"), "notify");
    assert_eq!(single("NotifyAccess"), "main");
    assert_eq!(single("WatchdogSec"), "30s");
    assert_eq!(single("Restart"), "always");
    assert!(!single("RestartSec").is_empty());
    assert_eq!(
        settings()["Unit"]["StartLimitIntervalSec"],
        ["0"],
        "an unattended gateway never stops restarting"
    );
    // The agent stops within 10 s (main.rs STOP_LIMIT), so systemd must wait longer.
    assert_eq!(single("TimeoutStopSec"), "20s");
    assert_eq!(
        single("ExecStart"),
        format!(
            "/usr/bin/trustiot-agent --config {}",
            agent::config::DEFAULT_PATH
        )
    );
    assert_eq!(settings()["Install"]["WantedBy"], ["multi-user.target"]);
}

#[test]
fn the_sandbox_lines_stay() {
    let expected = [
        ("NoNewPrivileges", "yes"),
        ("CapabilityBoundingSet", ""),
        ("AmbientCapabilities", ""),
        ("ProtectSystem", "strict"),
        ("ProtectHome", "yes"),
        ("PrivateTmp", "yes"),
        ("DevicePolicy", "closed"),
        ("DeviceAllow", "/dev/tpmrm0 rw"),
        (
            "RestrictAddressFamilies",
            "AF_UNIX AF_INET AF_INET6 AF_NETLINK",
        ),
        ("MemoryDenyWriteExecute", "yes"),
        ("ConfigurationDirectory", "trustiot"),
        ("StateDirectory", "trustiot-agent"),
        ("StateDirectoryMode", "0700"),
        ("UMask", "0077"),
        ("ProtectKernelTunables", "yes"),
        ("ProtectKernelModules", "yes"),
        ("ProtectControlGroups", "yes"),
        ("RestrictNamespaces", "yes"),
        ("RestrictSUIDSGID", "yes"),
        ("LockPersonality", "yes"),
        ("SystemCallArchitectures", "native"),
    ];
    for (key, value) in expected {
        assert_eq!(single(key), value, "{key}=");
    }
    assert_eq!(
        service("SystemCallFilter"),
        ["@system-service", "~@privileged @resources"]
    );
    // Not a sandbox line, but kept with them: the offline buffer's batches are allocated with
    // their own mmap, so freeing them returns the memory (edge/agent/README.md, 記憶體上限).
    assert_eq!(service("Environment"), ["MALLOC_MMAP_THRESHOLD_=65536"]);
    for absent in ["PrivateDevices", "ProcSubset", "PrivateUsers"] {
        // PrivateDevices hides the TPM, ProcSubset=pid hides /proc/meminfo, PrivateUsers drops
        // the tss group; each would break the agent.
        assert!(service(absent).is_empty(), "{absent}= breaks the agent");
    }
}

/// No PIN or password on the agent's command line or in its environment (Laysi/trustiot-v2#812,
/// rebuild checklist D5): the TPM key has none (`tests/secret_inputs.rs`), and the unit passes
/// nothing that could carry one. The command line is only the configuration's path, the one
/// environment variable is the allocator's, and nothing else feeds the process: no environment
/// file, no variables from the manager, no credentials, no other command run with the agent.
#[test]
fn the_unit_passes_no_secret_on_the_command_line_or_in_the_environment() {
    assert_eq!(
        single("ExecStart"),
        format!(
            "/usr/bin/trustiot-agent --config {}",
            agent::config::DEFAULT_PATH
        )
    );
    assert_eq!(service("Environment"), ["MALLOC_MMAP_THRESHOLD_=65536"]);
    for absent in [
        "EnvironmentFile",
        "PassEnvironment",
        "LoadCredential",
        "LoadCredentialEncrypted",
        "SetCredential",
        "SetCredentialEncrypted",
        "ImportCredential",
        "ExecStartPre",
        "ExecStartPost",
        "ExecCondition",
        "ExecReload",
        "ExecStop",
        "ExecStopPost",
    ] {
        assert!(service(absent).is_empty(), "{absent}= is set");
    }
    // sysusers.d has no password field; the one line is the account, with no login shell.
    let lines: Vec<&str> = SYSUSERS
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .collect();
    assert_eq!(
        lines,
        ["u trustiot-agent - \"TrustIoT gateway agent\" /var/lib/trustiot-agent /usr/sbin/nologin"]
    );
}

#[test]
fn the_sandbox_allows_every_address_family_the_agent_uses() {
    // AF_UNIX: sd_notify and the journal; AF_INET and AF_INET6: the broker and
    // the Modbus devices; AF_NETLINK: getifaddrs, with which the Modbus address guard
    // (src/modbus/address.rs, Interfaces::read) reads the gateway's own addresses before every
    // connection. Without AF_NETLINK it refuses every connection (Laysi/trustiot-v2#805);
    // src/modbus/address.rs runs that read under this line when a systemd user manager is there.
    let line = single("RestrictAddressFamilies");
    let allowed: Vec<&str> = line.split_whitespace().collect();
    assert!(!line.starts_with('~'), "an allow list, not a deny list");
    for family in ["AF_UNIX", "AF_INET", "AF_INET6", "AF_NETLINK"] {
        assert!(allowed.contains(&family), "{family} is missing: {line}");
    }
}

fn systemd_analyze() -> bool {
    let found = Command::new("systemd-analyze")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success());
    if !found {
        testkit::container::unavailable("systemd-analyze");
    }
    found
}

/// Both units in one directory: `verify` loads the egress unit the agent's unit requires from
/// the directory of the unit it checks. The egress unit requires nftables.service, which a test
/// machine without Debian's nftables package lacks (and `verify` then fails), so a stand-in with
/// the same type is written next to them.
fn unit_file() -> testkit::mqtt::ScratchDir {
    let dir = testkit::mqtt::ScratchDir::new("agent-unit");
    dir.write("trustiot-agent.service", UNIT);
    dir.write("trustiot-agent-egress.service", EGRESS_UNIT);
    dir.write(
        "nftables.service",
        "[Unit]\nDescription=Stand-in for Debian's nftables.service\n\n\
         [Service]\nType=oneshot\nRemainAfterExit=yes\nExecStart=/bin/true\n",
    );
    dir
}

#[test]
fn the_unit_passes_systemd_analyze_verify() {
    if !systemd_analyze() {
        return;
    }
    let dir = unit_file();
    for unit in ["trustiot-agent.service", "trustiot-agent-egress.service"] {
        let output = Command::new("systemd-analyze")
            .args(["verify", "--man=no"])
            .arg(dir.path().join(unit))
            .env("LC_ALL", "C")
            .output()
            .unwrap();
        // The executables (the agent's, and nft where nftables is not installed) may be missing
        // on the test machine; any other complaint is an error.
        let stderr = String::from_utf8_lossy(&output.stderr);
        let complaints: Vec<&str> = stderr
            .lines()
            .filter(|line| {
                !line.trim().is_empty()
                    && !line.contains("/usr/bin/trustiot-agent")
                    && !line.contains("/usr/sbin/nft")
            })
            .collect();
        assert!(complaints.is_empty(), "{unit}: {complaints:#?}");
    }
}

/// The settings of the egress unit: section, then key, then every value in order.
fn egress_settings() -> BTreeMap<String, BTreeMap<String, Vec<String>>> {
    parse(EGRESS_UNIT)
}

#[test]
fn the_agent_starts_only_after_its_egress_rules_load() {
    // Requires=: a failed load fails the agent's start (fail closed), and stopping or
    // restarting the egress unit stops or restarts the agent. After=: the rules are in force
    // before the agent's process starts. tests/egress.rs runs both under systemd.
    let unit = settings();
    assert!(
        unit["Unit"]["Requires"].contains(&EGRESS.to_owned()),
        "{:?}",
        unit["Unit"]
    );
    assert!(
        unit["Unit"]["After"].contains(&EGRESS.to_owned()),
        "{:?}",
        unit["Unit"]
    );
    let egress = egress_settings();
    let service = &egress["Service"];
    assert_eq!(service["Type"], ["oneshot"]);
    assert_eq!(service["RemainAfterExit"], ["yes"]);
    let load = format!("/usr/sbin/nft -f {EGRESS_RULES}");
    assert_eq!(service["ExecStart"], std::slice::from_ref(&load));
    assert_eq!(service["ExecReload"], std::slice::from_ref(&load));
    assert!(
        !service.contains_key("ExecStop"),
        "stopping the unit must leave the rules in force"
    );
    // nftables.service flushes the ruleset: it starts first (so it cannot start later), the rules
    // load after it, they stop (with the agent) or restart when it does, and its reload loads
    // them in the same transaction (the drop-in, which must be installed).
    let after = &egress["Unit"]["After"];
    assert!(after.contains(&"nftables.service".to_owned()), "{after:?}");
    assert!(
        after.contains(&"systemd-sysusers.service".to_owned()),
        "{after:?}"
    );
    assert_eq!(egress["Unit"]["Requires"], ["nftables.service"]);
    assert_eq!(
        service["ExecStartPre"],
        [format!("/usr/bin/test -f {NFTABLES_DROP_IN}")]
    );
    assert_eq!(service["CapabilityBoundingSet"], ["CAP_NET_ADMIN"]);
    let drop_in = parse(NFTABLES_RELOAD);
    assert_eq!(
        drop_in["Service"]["ExecReload"],
        [
            String::new(),
            format!("/usr/sbin/nft 'include \"/etc/nftables.conf\"; include \"{EGRESS_RULES}\"'")
        ],
        "the drop-in replaces Debian's reload with one transaction of both files, which nft \
         reads itself (no shell pipeline that could hide a failed read)"
    );
    // The rules file names the account the agent runs as.
    assert!(
        RULES.contains(&format!("meta skuid \"{ACCOUNT}\" jump agent")),
        "the rules do not match the agent's account"
    );
}

#[test]
fn the_unit_scores_at_most_the_allowed_exposure() {
    if !systemd_analyze() {
        return;
    }
    let dir = unit_file();
    let output = Command::new("systemd-analyze")
        .args(["security", "--offline=true", "--no-pager"])
        .arg(dir.path().join("trustiot-agent.service"))
        .env("LC_ALL", "C")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let exposure: f64 = stdout
        .lines()
        .find_map(|line| line.split_once("Overall exposure level for trustiot-agent.service: "))
        .and_then(|(_, rest)| rest.split_whitespace().next())
        .and_then(|value| value.parse().ok())
        .unwrap_or_else(|| {
            panic!(
                "no exposure level in:\n{stdout}\n{}",
                String::from_utf8_lossy(&output.stderr)
            )
        });
    assert!(
        exposure <= MAX_EXPOSURE,
        "exposure {exposure} is above {MAX_EXPOSURE}:\n{stdout}"
    );
}

/// A systemd time span as the unit writes it (`30s`, `1min`).
fn seconds(span: &str) -> u64 {
    if let Some(minutes) = span.strip_suffix("min") {
        minutes.parse::<u64>().unwrap() * 60
    } else {
        span.strip_suffix('s').unwrap().parse().unwrap()
    }
}

#[test]
fn a_stuck_tpm_cannot_outlast_the_watchdog_or_the_start_timeout() {
    use agent::sdnotify::PROGRESS_TICK;
    use agent::tpm::RESPONSE_TIMEOUT;

    // A TPM signature blocks the connection thread, which stops counting ticks; the main runtime
    // withholds WATCHDOG=1 when a whole notification interval (half of WatchdogSec) passed
    // without a tick. A signature that gives up within RESPONSE_TIMEOUT must leave a tick in
    // every interval, or systemd kills the agent before the agent's own timeout fires.
    let notify_every = seconds(&single("WatchdogSec")) * 1000 / 2;
    let stalled = u64::try_from((RESPONSE_TIMEOUT + PROGRESS_TICK).as_millis()).unwrap();
    assert!(
        stalled < notify_every,
        "RESPONSE_TIMEOUT {RESPONSE_TIMEOUT:?} + tick {PROGRESS_TICK:?} must stay below half of WatchdogSec ({notify_every} ms)"
    );
    // Start-up opens the key: one TPM2_ReadPublic and one test signature, each bounded by
    // RESPONSE_TIMEOUT, before READY=1.
    let start = seconds(&single("TimeoutStartSec")) * 1000;
    let opening = u64::try_from((RESPONSE_TIMEOUT * 2).as_millis()).unwrap();
    assert!(
        opening < start,
        "opening the TPM key may take {opening} ms, TimeoutStartSec is {start} ms"
    );
}

/// The fieldbus allowlist component's socket and service (Laysi/trustiot-v2#864;
/// edge/agent/README.md, section "fieldbus 放行集合"). tests/allowlist.rs runs them under systemd.
const ALLOWLIST_SOCKET: &str = include_str!("../systemd/trustiot-agent-allowlist.socket");
const ALLOWLIST_SERVICE: &str = include_str!("../systemd/trustiot-agent-allowlist@.service");
/// Where the image installs the component (the service's `ExecStart=`).
const ALLOWLIST_EXECUTABLE: &str = "/usr/libexec/trustiot-agent/allowlist";
/// The highest `systemd-analyze security` exposure allowed for an instance of the component; it
/// scores 1.1 on 2026-10-31 (what is left: CAP_NET_ADMIN and its ambient grant, netlink sockets,
/// the gateway's network namespace and accounts, the root directory, the clock device).
const ALLOWLIST_MAX_EXPOSURE: f64 = 1.5;

#[test]
fn only_the_agents_group_reaches_the_allowlist_socket() {
    let socket = parse(ALLOWLIST_SOCKET);
    let section = &socket["Socket"];
    for (key, value) in [
        ("ListenStream", agent::allowlist::SOCKET),
        // Root's socket in root's directory, which only root and the agent's group may connect
        // to: sysusers' `u trustiot-agent -` line also creates the group of that name.
        ("SocketUser", "root"),
        ("SocketGroup", ACCOUNT),
        ("SocketMode", "0660"),
        ("DirectoryMode", "0755"),
        ("RemoveOnStop", "yes"),
        // One instance per connection, one at a time.
        ("Accept", "yes"),
        ("MaxConnections", "1"),
        // A client that connects in a loop is slowed down, not answered with a failed socket:
        // the poll limit pauses the socket well below the trigger limit (200 in 2 s for
        // Accept=yes), which would fail it.
        ("PollLimitIntervalSec", "2s"),
        ("PollLimitBurst", "10"),
    ] {
        assert_eq!(
            section.get(key).map(Vec::as_slice),
            Some(std::slice::from_ref(&value.to_owned())),
            "{key}="
        );
    }
    assert_eq!(
        section.len(),
        10,
        "a new setting needs a check: {section:?}"
    );
    assert!(
        !section.contains_key("TriggerLimitBurst"),
        "the default trigger limit stays above the poll limit"
    );
    assert!(
        !socket.contains_key("Install"),
        "the agent's unit pulls the socket in (Laysi/trustiot-v2#865)"
    );
    // The agent does not own the socket's directory, so it cannot put another socket there.
    let directory = agent::allowlist::SOCKET
        .strip_prefix("/run/")
        .and_then(|path| path.strip_suffix("/socket"))
        .unwrap();
    assert!(
        !service("RuntimeDirectory").contains(&directory.to_owned()),
        "the agent's unit must not own /run/{directory}"
    );
}

/// Acceptance criterion 3 of Laysi/trustiot-v2#864: the agent still runs without root and without
/// a capability, and the component has only the permissions written down here.
#[test]
fn the_allowlist_component_holds_only_cap_net_admin_as_a_dynamic_account() {
    let unit = parse(ALLOWLIST_SERVICE);
    let section = &unit["Service"];
    let one = |key: &str| -> String {
        let values = section.get(key).cloned().unwrap_or_default();
        assert_eq!(values.len(), 1, "{key}= should appear once: {values:?}");
        values[0].clone()
    };
    for (key, value) in [
        ("ExecStart", ALLOWLIST_EXECUTABLE),
        ("StandardInput", "socket"),
        ("StandardOutput", "socket"),
        // Not the connection: the log goes to the journal, never to the client.
        ("StandardError", "journal"),
        ("SyslogIdentifier", "trustiot-agent-allowlist"),
        ("RuntimeMaxSec", "30s"),
        ("TimeoutStopSec", "5s"),
        ("DynamicUser", "yes"),
        ("User", "trustiot-agent-allowlist"),
        ("CapabilityBoundingSet", "CAP_NET_ADMIN"),
        ("AmbientCapabilities", "CAP_NET_ADMIN"),
        ("NoNewPrivileges", "yes"),
        ("ProtectSystem", "strict"),
        ("ProtectHome", "yes"),
        ("PrivateTmp", "yes"),
        ("PrivateDevices", "yes"),
        ("PrivateIPC", "yes"),
        ("ProtectKernelTunables", "yes"),
        ("ProtectKernelModules", "yes"),
        ("ProtectKernelLogs", "yes"),
        ("ProtectControlGroups", "yes"),
        ("ProtectClock", "yes"),
        ("ProtectHostname", "yes"),
        ("ProtectProc", "invisible"),
        ("ProcSubset", "pid"),
        // Netlink for nft; the connection and the journal stream are handed to it open.
        ("RestrictAddressFamilies", "AF_NETLINK"),
        ("IPAddressDeny", "any"),
        ("TasksMax", "4"),
        ("RestrictNamespaces", "yes"),
        ("RestrictRealtime", "yes"),
        ("RestrictSUIDSGID", "yes"),
        ("LockPersonality", "yes"),
        ("MemoryDenyWriteExecute", "yes"),
        ("SystemCallArchitectures", "native"),
        ("SystemCallErrorNumber", "EPERM"),
        ("UMask", "0077"),
    ] {
        assert_eq!(one(key), value, "{key}=");
    }
    assert_eq!(
        section["SystemCallFilter"],
        ["@system-service", "~@privileged @resources"]
    );
    // A refused or failed instance leaves no unit behind.
    assert_eq!(unit["Unit"]["CollectMode"], ["inactive-or-failed"]);
    assert_ne!(one("User"), ACCOUNT, "not the agent's account");
    assert_ne!(one("User"), "root");
    // Nothing else reaches the process (no environment, credentials, other commands), it gets no
    // group, writable path or directory of its own, and the namespaces stay the gateway's (the
    // sets are in its network namespace; README.md says why).
    for absent in [
        "Group",
        "SupplementaryGroups",
        "Environment",
        "EnvironmentFile",
        "PassEnvironment",
        "LoadCredential",
        "SetCredential",
        "ImportCredential",
        "ExecStartPre",
        "ExecStartPost",
        "ExecStop",
        "ReadWritePaths",
        "StateDirectory",
        "RuntimeDirectory",
        "CacheDirectory",
        "LogsDirectory",
        "ConfigurationDirectory",
        "PrivateNetwork",
        "PrivateUsers",
    ] {
        assert!(!section.contains_key(absent), "{absent}= is set");
    }
    assert_eq!(
        section.len(),
        37,
        "a new setting needs a check: {section:?}"
    );

    // The agent itself gains nothing: its own account, no capability.
    assert_eq!(single("User"), ACCOUNT);
    assert_eq!(single("CapabilityBoundingSet"), "");
    assert_eq!(single("AmbientCapabilities"), "");
}

/// Both units in one directory: `verify` of the socket loads the service template next to it.
fn allowlist_units() -> testkit::mqtt::ScratchDir {
    let dir = testkit::mqtt::ScratchDir::new("agent-allowlist-units");
    dir.write("trustiot-agent-allowlist.socket", ALLOWLIST_SOCKET);
    dir.write("trustiot-agent-allowlist@.service", ALLOWLIST_SERVICE);
    dir
}

#[test]
fn the_allowlist_units_pass_systemd_analyze_verify() {
    if !systemd_analyze() {
        return;
    }
    let dir = allowlist_units();
    for unit in [
        "trustiot-agent-allowlist.socket",
        "trustiot-agent-allowlist@.service",
    ] {
        let output = Command::new("systemd-analyze")
            .args(["verify", "--man=no"])
            .arg(dir.path().join(unit))
            .env("LC_ALL", "C")
            .output()
            .unwrap();
        // The executable is not installed on the test machine; any other complaint is an error.
        let stderr = String::from_utf8_lossy(&output.stderr);
        let complaints: Vec<&str> = stderr
            .lines()
            .filter(|line| !line.trim().is_empty() && !line.contains(ALLOWLIST_EXECUTABLE))
            .collect();
        assert!(complaints.is_empty(), "{unit}: {complaints:#?}");
    }
}

#[test]
fn the_allowlist_component_scores_at_most_its_allowed_exposure() {
    if !systemd_analyze() {
        return;
    }
    let dir = allowlist_units();
    // A template is scored as an instance of it.
    let output = Command::new("systemd-analyze")
        .args(["security", "--offline=true", "--no-pager"])
        .arg(dir.path().join("trustiot-agent-allowlist@.service"))
        .env("LC_ALL", "C")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let exposure: f64 = stdout
        .lines()
        .find_map(|line| line.split_once("Overall exposure level for trustiot-agent-allowlist@"))
        .and_then(|(_, rest)| rest.split_once(".service: "))
        .and_then(|(_, rest)| rest.split_whitespace().next())
        .and_then(|value| value.parse().ok())
        .unwrap_or_else(|| {
            panic!(
                "no exposure level in:\n{stdout}\n{}",
                String::from_utf8_lossy(&output.stderr)
            )
        });
    assert!(
        exposure <= ALLOWLIST_MAX_EXPOSURE,
        "exposure {exposure} is above {ALLOWLIST_MAX_EXPOSURE}:\n{stdout}"
    );
}
