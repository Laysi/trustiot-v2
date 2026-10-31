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
/// The command that loads them: the egress unit's `ExecStart=` and `ExecReload=`, and the agent
/// unit's `ExecStartPre=` (with `+`).
const EGRESS_LOAD: &str = "/usr/sbin/nft -f /usr/share/trustiot-agent/trustiot-agent-egress.nft";
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
        "ExecStartPost",
        "ExecCondition",
        "ExecReload",
        "ExecStop",
        "ExecStopPost",
    ] {
        assert!(service(absent).is_empty(), "{absent}= is set");
    }
    // The one other command is the reload of the egress rules from a fixed root-owned file
    // (Laysi/trustiot-v2#863, the_agents_egress_rules_load_again_before_every_start); it reads
    // nothing of the agent's.
    assert_eq!(service("ExecStartPre"), [format!("+{EGRESS_LOAD}")]);
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

/// The prefixes systemd allows before an `Exec*=` command (systemd.service(5), "Command lines").
fn exec_prefix(command: &str) -> String {
    command
        .chars()
        .take_while(|c| "@-:+!|".contains(*c))
        .collect()
}

#[test]
fn the_agents_egress_rules_load_again_before_every_start() {
    // Laysi/trustiot-v2#863: a `nft flush ruleset` outside systemd leaves the egress unit active,
    // so the agent's unit loads the same file itself before every start, the restarts of
    // Restart=always included. tests/egress.rs flushes the rules, kills the agent and watches the
    // restart load them again, or keep the agent stopped when they cannot load.
    let egress = egress_settings();
    assert_eq!(egress["Service"]["ExecStart"], [EGRESS_LOAD]);
    assert!(EGRESS_LOAD.ends_with(&format!(" -f {EGRESS_RULES}")));
    assert_eq!(single("ExecStartPre"), format!("+{EGRESS_LOAD}"));
    // "+" and nothing else: no "-", so a failed load fails the start and the agent does not run.
    assert_eq!(exec_prefix(&single("ExecStartPre")), "+");
    assert_eq!(single("Restart"), "always");

    // "+" lifts the sandbox for that command only. The agent's own process keeps its account, no
    // capabilities and no way to gain any, and no other command line of the unit runs elevated.
    assert_eq!(single("User"), ACCOUNT);
    assert_eq!(single("Group"), ACCOUNT);
    assert_eq!(single("CapabilityBoundingSet"), "");
    assert_eq!(single("AmbientCapabilities"), "");
    assert_eq!(single("NoNewPrivileges"), "yes");
    for (key, values) in &settings()["Service"] {
        if !key.starts_with("Exec") || key == "ExecStartPre" {
            continue;
        }
        for value in values {
            let prefix = exec_prefix(value);
            assert!(
                !prefix.contains('+') && !prefix.contains('!'),
                "{key}={value} runs with elevated privileges"
            );
        }
    }
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
