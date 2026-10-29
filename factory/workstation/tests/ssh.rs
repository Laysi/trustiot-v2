//! `register --gateway` through a fake `ssh` first on the tool's `PATH` (Laysi/trustiot-v2#616):
//! what crosses to the gateway (the arguments, the environment and the standard input of every
//! ssh call), the remote cleanup, values refused before ssh runs, retries of ssh's own failures,
//! and the stops with their exit codes. The gateway's agent is a shell stand-in that prints
//! reports the test made (`support::ssh::CANNED_AGENT`); tests/pipeline_swtpm.rs runs the real
//! `trustiot-agent provision` against a software TPM instead.

#![cfg(target_os = "linux")]

mod support;

use std::collections::HashSet;

use protocol::GatewayId;
use protocol::provisioning::KeyHandle;
use support::bench::{Bench, DESTINATION};
use support::ssh::{CANNED_AGENT, FAKE_SUDO, GatewayKeys};
use workstation::gateway::{self, KEPT_ENVIRONMENT, Mode, Request, SSH_OPTIONS, UploadedAgent};
use workstation::journal::State;

/// Variables a shell adds to its own environment, which `env` then lists.
const SHELL_VARIABLES: [&str; 4] = ["PWD", "OLDPWD", "SHLVL", "_"];

fn handle() -> KeyHandle {
    KeyHandle::new(gateway::DEFAULT_HANDLE).expect("a handle")
}

/// The argument vector ssh must get for `mode`: the fixed options, `--`, the destination and
/// the remote command.
fn expected_argv(hardware_id: &str, mode: Mode, sudo: bool, upload: Option<&str>) -> Vec<String> {
    let id = GatewayId::new(hardware_id).expect("an ID");
    let command = gateway::remote_command(
        &Request {
            hardware_id: &id,
            handle: handle(),
            mode,
        },
        sudo,
        upload,
    )
    .expect("a command");
    let mut argv: Vec<String> = SSH_OPTIONS
        .iter()
        .map(|&option| option.to_owned())
        .collect();
    argv.extend(["--".to_owned(), DESTINATION.to_owned(), command]);
    argv
}

/// Every ssh call's environment holds only the kept variables (and what the fake's shell adds).
fn assert_environment_whitelisted(bench: &Bench) {
    let allowed: HashSet<&str> = KEPT_ENVIRONMENT
        .iter()
        .chain(SHELL_VARIABLES.iter())
        .copied()
        .collect();
    for call in bench.ssh.calls() {
        for name in call.env_names() {
            assert!(
                allowed.contains(name.as_str()),
                "ssh got the variable {name}"
            );
        }
        assert!(
            call.env.iter().any(|entry| entry.starts_with("PATH=")),
            "ssh keeps PATH"
        );
    }
}

/// A bench with an uploadable canned agent: its path and bytes.
fn canned_agent(bench: &Bench) -> (String, Vec<u8>) {
    let path = bench.agent_file("trustiot-agent", CANNED_AGENT.as_bytes());
    (
        path.to_str().expect("UTF-8").to_owned(),
        CANNED_AGENT.as_bytes().to_vec(),
    )
}

#[tokio::test]
async fn only_the_agent_the_hardware_id_and_the_handle_cross_to_the_gateway() {
    let bench = Bench::start("ssh-crosses", Vec::new()).await;
    let keys = GatewayKeys::new();
    bench.ssh.leave_reports(
        &keys.report("tg-0001", None),
        &keys.report("tg-0001", Some(true)),
    );
    let (agent, bytes) = canned_agent(&bench);
    let sha256 = UploadedAgent::from_bytes(bytes.clone()).sha256().to_owned();

    let run = bench
        .run(&bench.register_args("tg-0001", &["--upload-agent", &agent]))
        .await;
    assert_eq!(run.code, Some(0), "{}", run.output());
    assert!(
        run.stdout.contains("the TPM key at 0x81000100 was created"),
        "{}",
        run.stdout
    );
    assert!(
        run.stdout.contains("registered hardware ID tg-0001"),
        "{}",
        run.stdout
    );
    // A TPM without an EK certificate is accepted with a notice (README, "EK 憑證").
    assert!(run.stderr.contains("no EK certificate"), "{}", run.stderr);

    let calls = bench.ssh.calls();
    assert_eq!(calls.len(), 2, "an inspection, then provision");
    assert_eq!(
        calls[0].argv,
        expected_argv("tg-0001", Mode::Inspect, false, Some(&sha256))
    );
    assert_eq!(
        calls[1].argv,
        expected_argv("tg-0001", Mode::Provision, false, Some(&sha256))
    );
    for call in &calls {
        assert_eq!(call.destination, DESTINATION);
        assert_eq!(
            call.stdin, bytes,
            "standard input is the agent, nothing else"
        );
        assert_eq!(call.status, Some(0));
    }
    assert_environment_whitelisted(&bench);
    bench.assert_nothing_secret_crossed();
    assert_eq!(
        bench.ssh.agent_calls(),
        [
            "provision --hardware-id tg-0001 --handle 0x81000100 --inspect",
            "provision --hardware-id tg-0001 --handle 0x81000100",
        ]
    );
    assert!(
        bench.ssh.remote_tmp_entries().is_empty(),
        "the remote cleanup removed the upload: {:?}",
        bench.ssh.remote_tmp_entries()
    );

    let record = bench
        .journal()
        .load("tg-0001")
        .expect("readable")
        .expect("a record");
    assert_eq!(record.state, State::Registered);
    assert_eq!(record.public_key_sha256, keys.public_key_sha256());
    let provisioned = bench
        .events("tg-0001")
        .into_iter()
        .find(|event| event["event"] == "provisioned")
        .expect("a provisioned line");
    assert_eq!(provisioned["key_created"], true);
    assert_eq!(provisioned["ek_certificate"], "absent");
    assert_eq!(provisioned["uploaded_agent_sha256"], sha256.as_str());
}

#[tokio::test]
async fn the_installed_agent_runs_without_an_upload_and_sudo_never_asks() {
    let bench = Bench::start("ssh-installed", Vec::new()).await;
    let keys = GatewayKeys::new();
    bench.ssh.leave_reports(
        &keys.report("tg-0002", Some(false)),
        &keys.report("tg-0002", Some(false)),
    );
    bench.ssh.install_remote("trustiot-agent", CANNED_AGENT);
    bench.ssh.install_remote("sudo", FAKE_SUDO);

    let run = bench
        .run(&bench.register_args("tg-0002", &["--remote-sudo"]))
        .await;
    assert_eq!(run.code, Some(0), "{}", run.output());
    assert!(
        run.stdout.contains("was already there and reused"),
        "{}",
        run.stdout
    );
    let calls = bench.ssh.calls();
    assert_eq!(
        calls.len(),
        1,
        "the inspection found the key: no provision run"
    );
    assert_eq!(
        calls[0].argv,
        expected_argv("tg-0002", Mode::Inspect, true, None)
    );
    assert!(calls[0].stdin.is_empty(), "nothing on standard input");
    assert_eq!(
        bench.ssh.read_remote_file("sudo-calls").trim(),
        "trustiot-agent provision --hardware-id tg-0002 --handle 0x81000100 --inspect"
    );
    assert_environment_whitelisted(&bench);
    bench.assert_nothing_secret_crossed();
}

#[tokio::test]
async fn values_that_could_change_the_remote_command_are_refused_before_ssh_runs() {
    let bench = Bench::start("ssh-injection", Vec::new()).await;
    let hostile = [
        "tg 0001",
        "tg;reboot",
        "tg'0001",
        "tg\"0001",
        "$(reboot)",
        "tg$(reboot)",
        "`reboot`",
        "tg\n0001",
        "tg|reboot",
        "tg&reboot",
        "tg>x",
        "-tg",
        "tg*",
        "",
    ];
    for id in hostile {
        let run = bench.run(&bench.register_args(id, &[])).await;
        assert_eq!(run.code, Some(2), "hardware ID {id:?}: {}", run.output());
    }
    for handle in [
        "0x81000100;reboot",
        "0x81000100 0",
        "$(reboot)",
        "'0x81000100'",
        "0x81000100\n",
        // Valid persistent handles the gateway key must never take.
        "0x81000001",
        "0x81800000",
    ] {
        let run = bench
            .run(&bench.register_args("tg-0001", &["--handle", handle]))
            .await;
        assert_eq!(run.code, Some(2), "handle {handle:?}: {}", run.output());
    }
    for destination in [
        "-oProxyCommand=reboot",
        "gw;reboot",
        "gw reboot",
        "gw'x",
        "$(reboot)",
        "a@b@c",
        "@gw",
        "gw@",
        "",
    ] {
        let mut arguments = bench.register_args("tg-0001", &[]);
        let at = arguments
            .iter()
            .position(|argument| argument == DESTINATION)
            .expect("the destination");
        arguments[at] = destination.to_owned();
        let run = bench.run(&arguments).await;
        assert_eq!(
            run.code,
            Some(2),
            "destination {destination:?}: {}",
            run.output()
        );
    }
    // Options that do not go together.
    for extra in [
        &["--csr", "request.pem"][..],
        &["--ek", "ek.pem"][..],
        &["--allow-new-key", "yes"][..],
    ] {
        let run = bench.run(&bench.register_args("tg-0001", extra)).await;
        assert_eq!(run.code, Some(2), "{extra:?}: {}", run.output());
    }
    assert_eq!(bench.ssh.count(), 0, "ssh never ran");
    assert!(bench.registry.seen().is_empty(), "nothing was registered");
}

#[tokio::test]
async fn ssh_options_without_gateway_are_refused() {
    let bench = Bench::start("ssh-only", Vec::new()).await;
    for extra in [
        &["--handle", "0x81000100"][..],
        &["--upload-agent", "agent"][..],
        &["--remote-sudo"][..],
        &["--allow-new-key"][..],
    ] {
        let mut arguments = vec![
            "register".to_owned(),
            "--hardware-id".to_owned(),
            "tg-0001".to_owned(),
            "--csr".to_owned(),
            "request.pem".to_owned(),
            "--ek".to_owned(),
            "ek.pem".to_owned(),
            "--root-sha256".to_owned(),
            bench.root_sha256.clone(),
        ];
        arguments.extend(extra.iter().map(|argument| (*argument).to_owned()));
        let run = bench.run(&arguments).await;
        assert_eq!(run.code, Some(2), "{extra:?}: {}", run.output());
        assert!(run.stderr.contains("needs --gateway"), "{}", run.stderr);
    }
    assert_eq!(bench.ssh.count(), 0);
}

#[tokio::test]
async fn ssh_failures_are_tried_again_and_every_run_reports_the_same() {
    let bench = Bench::start("ssh-retry", Vec::new()).await;
    let keys = GatewayKeys::new();
    bench.ssh.leave_reports(
        &keys.report("tg-0003", None),
        &keys.report("tg-0003", Some(true)),
    );
    let (agent, bytes) = canned_agent(&bench);
    // 1: the connection fails; 2: the inspection runs but its answer is lost; 3: the
    // inspection; 4: provision runs but its answer is lost; 5: provision again.
    bench.ssh.fail_before(1);
    bench.ssh.fail_after(2);
    bench.ssh.fail_after(4);

    let run = bench
        .run(&bench.register_args("tg-0003", &["--upload-agent", &agent]))
        .await;
    assert_eq!(run.code, Some(0), "{}", run.output());
    let calls = bench.ssh.calls();
    assert_eq!(calls.len(), 5);
    assert_eq!(
        calls.iter().map(|call| call.status).collect::<Vec<_>>(),
        [None, Some(0), Some(0), Some(0), Some(0)],
        "the first call never reached the remote command"
    );
    let sha256 = UploadedAgent::from_bytes(bytes.clone()).sha256().to_owned();
    let inspect = expected_argv("tg-0003", Mode::Inspect, false, Some(&sha256));
    let provision = expected_argv("tg-0003", Mode::Provision, false, Some(&sha256));
    for (number, call) in calls.iter().enumerate() {
        let expected = if number < 3 { &inspect } else { &provision };
        assert_eq!(&call.argv, expected, "call {}", number + 1);
        assert_eq!(call.stdin, bytes, "every attempt uploads the same agent");
    }
    assert_eq!(calls[1].stdout, calls[2].stdout, "the same inspection");
    assert_eq!(
        calls[3].stdout, calls[4].stdout,
        "the same provision report"
    );
    assert_eq!(bench.ssh.agent_calls().len(), 4);
    assert!(bench.ssh.remote_tmp_entries().is_empty());
    assert!(
        run.stderr.contains("trying again"),
        "the retries are logged: {}",
        run.stderr
    );
    assert_eq!(bench.registry.registrations(), 1);
    bench.assert_nothing_secret_crossed();
}

#[tokio::test]
async fn a_gateway_that_stays_unreachable_stops_with_23() {
    let bench = Bench::start("ssh-unreachable", Vec::new()).await;
    let (agent, _) = canned_agent(&bench);
    for call in 1..=gateway::SSH_BACKOFF.max_attempts {
        bench.ssh.fail_before(usize::try_from(call).expect("fits"));
    }
    let run = bench
        .run(&bench.register_args("tg-0004", &["--upload-agent", &agent]))
        .await;
    assert_eq!(run.code, Some(23), "{}", run.output());
    assert!(
        run.stderr
            .contains("could not reach the gateway through ssh"),
        "{}",
        run.stderr
    );
    assert!(run.stderr.contains("Connection refused"), "{}", run.stderr);
    assert_eq!(
        bench.ssh.count(),
        usize::try_from(gateway::SSH_BACKOFF.max_attempts).expect("fits")
    );
    assert!(bench.ssh.agent_calls().is_empty());
    assert!(bench.registry.seen().is_empty(), "nothing was registered");
    let last = bench.events("tg-0004").pop().expect("a log line");
    assert_eq!(last["event"], "provision_stopped");
    assert_eq!(last["stop"], "gateway_unreachable");
}

#[tokio::test]
async fn a_failure_on_the_gateway_stops_with_22_and_its_output_is_not_used() {
    let bench = Bench::start("ssh-failed", Vec::new()).await;
    let keys = GatewayKeys::new();
    let (agent, _) = canned_agent(&bench);
    let register = |id: &str| bench.register_args(id, &["--upload-agent", &agent]);

    // The agent fails (another object at the handle) but prints a report all the same.
    bench.ssh.leave_reports(
        &keys.report("tg-0005", None),
        &keys.report("tg-0005", Some(true)),
    );
    bench.ssh.write_remote_file("status-provision", b"5");
    let run = bench.run(&register("tg-0005")).await;
    assert_eq!(run.code, Some(22), "{}", run.output());
    assert!(run.stderr.contains("ended with status 5"), "{}", run.stderr);

    // The upload arrives damaged: the agent never runs.
    bench.ssh.reset_remote();
    let before = bench.ssh.count();
    bench.ssh.corrupt(before + 1);
    let run = bench.run(&register("tg-0006")).await;
    assert_eq!(run.code, Some(22), "{}", run.output());
    assert!(
        run.stderr
            .contains("the uploaded agent does not have the SHA-256"),
        "{}",
        run.stderr
    );
    assert_eq!(
        bench.ssh.call(before + 1).status,
        Some(gateway::CHECKSUM_MISMATCH)
    );
    assert!(bench.ssh.agent_calls().is_empty(), "the agent never ran");
    assert!(bench.ssh.remote_tmp_entries().is_empty());

    // The session is hung up while the agent runs: the upload is removed all the same.
    bench.ssh.reset_remote();
    bench.ssh.leave_reports(
        &keys.report("tg-0007", None),
        &keys.report("tg-0007", Some(true)),
    );
    bench.ssh.write_remote_file("hup", b"");
    let run = bench.run(&register("tg-0007")).await;
    assert_eq!(run.code, Some(22), "{}", run.output());
    assert_eq!(bench.ssh.calls().last().expect("a call").status, Some(129));
    assert!(
        bench.ssh.remote_tmp_entries().is_empty(),
        "the trap removed the upload: {:?}",
        bench.ssh.remote_tmp_entries()
    );

    assert!(bench.registry.seen().is_empty(), "nothing was registered");
    for id in ["tg-0005", "tg-0006", "tg-0007"] {
        let last = bench.events(id).pop().expect("a log line");
        assert_eq!(last["stop"], "provision_failed", "{id}");
    }
}

#[tokio::test]
async fn reports_that_break_a_reader_obligation_are_refused_with_21() {
    let bench = Bench::start("ssh-refused", Vec::new()).await;
    let keys = GatewayKeys::new();
    let (agent, _) = canned_agent(&bench);
    let present = keys.report("tg-0008", Some(false));
    let mut other_id = present.clone();
    other_id["hardware_id"] = "tg-9999".into();
    let mut other_subject = present.clone();
    other_subject["key"]["certificate_request"] = keys.request("tg-9999").into();
    let mut other_key = present.clone();
    other_key["key"]["public_key"] = GatewayKeys::new().key.public_key_pem().into();
    let cases = [
        ("another hardware ID", serde_json::to_vec(&other_id)),
        ("another CSR subject", serde_json::to_vec(&other_subject)),
        ("another public key", serde_json::to_vec(&other_key)),
        ("not JSON", Ok(b"usage: trustiot-agent provision".to_vec())),
        (
            "more than 16 KiB",
            Ok(vec![b' '; protocol::provisioning::MAX_REPORT_BYTES + 1]),
        ),
    ];
    for (what, report) in cases {
        bench
            .ssh
            .write_remote_file("report-inspect.json", &report.expect("JSON"));
        let run = bench
            .run(&bench.register_args("tg-0008", &["--upload-agent", &agent]))
            .await;
        assert_eq!(run.code, Some(21), "{what}: {}", run.output());
        assert!(run.stderr.contains("is refused"), "{what}: {}", run.stderr);
        let last = bench.events("tg-0008").pop().expect("a log line");
        assert_eq!(last["stop"], "report_refused", "{what}");
    }
    // A gateway that keeps writing: the call is ended at the limit and the report refused, not
    // left blocked on a full pipe until its deadline (which would end as unreachable, 23).
    bench.ssh.write_remote_file("flood", b"");
    let run = bench
        .run(&bench.register_args("tg-0008", &["--upload-agent", &agent]))
        .await;
    assert_eq!(run.code, Some(21), "a flood: {}", run.output());
    assert!(run.stderr.contains("larger than"), "{}", run.stderr);
    assert_eq!(bench.ssh.agent_calls().len(), 6, "one attempt, not retried");
    assert!(bench.registry.seen().is_empty(), "nothing was registered");
}

#[tokio::test]
async fn a_known_gateway_whose_tpm_has_no_key_stops_with_20() {
    let bench = Bench::start("ssh-cleared", Vec::new()).await;
    let keys = GatewayKeys::new();
    let (agent, _) = canned_agent(&bench);
    let register = |extra: &[&str]| {
        let mut arguments = vec!["--upload-agent", agent.as_str()];
        arguments.extend_from_slice(extra);
        bench.register_args("tg-0010", &arguments)
    };
    bench.ssh.leave_reports(
        &keys.report("tg-0010", None),
        &keys.report("tg-0010", Some(true)),
    );
    let first = bench.run(&register(&[])).await;
    assert_eq!(first.code, Some(0), "{}", first.output());
    let original = keys.public_key_sha256();

    // The TPM is cleared: the inspection finds no key, and provision is never run.
    let cleared = GatewayKeys {
        key: rcgen::KeyPair::generate().expect("a new key"),
        ek: keys.ek,
    };
    bench.ssh.reset_remote();
    bench.ssh.leave_reports(
        &cleared.report("tg-0010", None),
        &cleared.report("tg-0010", Some(true)),
    );
    let stopped = bench.run(&register(&[])).await;
    assert_eq!(stopped.code, Some(20), "{}", stopped.output());
    assert!(
        stopped.stderr.contains("--allow-new-key"),
        "{}",
        stopped.stderr
    );
    assert_eq!(
        bench.ssh.agent_calls(),
        ["provision --hardware-id tg-0010 --handle 0x81000100 --inspect"],
        "nothing was created"
    );
    assert_eq!(
        bench.registry.seen().len(),
        1,
        "only the first registration"
    );

    // With --allow-new-key the new key is sent, and the platform refuses it.
    let refused = bench.run(&register(&["--allow-new-key"])).await;
    assert_eq!(refused.code, Some(10), "{}", refused.output());
    assert_eq!(
        bench.registry.seen().last().expect("a request").answered,
        409
    );
    let record = bench
        .journal()
        .load("tg-0010")
        .expect("readable")
        .expect("a record");
    assert_eq!(
        record.public_key_sha256, original,
        "the record keeps the registered key"
    );
}
