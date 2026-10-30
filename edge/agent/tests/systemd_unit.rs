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
/// The highest `systemd-analyze security` exposure allowed; the unit scores 1.5 on 2026-10-21
/// (what is left: network access, the TPM device, the tss group, /proc/meminfo).
const MAX_EXPOSURE: f64 = 2.0;

/// The unit's settings: section, then key, then every value in order (a key may repeat).
fn settings() -> BTreeMap<String, BTreeMap<String, Vec<String>>> {
    let mut sections: BTreeMap<String, BTreeMap<String, Vec<String>>> = BTreeMap::new();
    let mut section = String::new();
    for line in UNIT.lines().map(str::trim) {
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
        ("RestrictAddressFamilies", "AF_UNIX AF_INET AF_INET6"),
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

fn unit_file() -> testkit::mqtt::ScratchDir {
    let dir = testkit::mqtt::ScratchDir::new("agent-unit");
    dir.write("trustiot-agent.service", UNIT);
    dir
}

#[test]
fn the_unit_passes_systemd_analyze_verify() {
    if !systemd_analyze() {
        return;
    }
    let dir = unit_file();
    let output = Command::new("systemd-analyze")
        .args(["verify", "--man=no"])
        .arg(dir.path().join("trustiot-agent.service"))
        .env("LC_ALL", "C")
        .output()
        .unwrap();
    // The executable is not installed on the test machine; any other complaint is an error.
    let stderr = String::from_utf8_lossy(&output.stderr);
    let complaints: Vec<&str> = stderr
        .lines()
        .filter(|line| !line.trim().is_empty() && !line.contains("/usr/bin/trustiot-agent"))
        .collect();
    assert!(complaints.is_empty(), "{complaints:#?}");
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
