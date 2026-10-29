//! `register --gateway` through a fake `ssh` first on the tool's `PATH` (Laysi/trustiot-v2#616):
//! what crosses to the gateway (the arguments, the environment and the standard input of every
//! ssh call), the remote cleanup, values refused before ssh runs, retries of ssh's own failures,
//! and the stops with their exit codes; then the installation of the certificates on the gateway
//! (Laysi/trustiot-v2#617): its standard input is the chain and the root and nothing else, it
//! runs the installed agent even with `--upload-agent`, and its stops have their exit codes. The
//! gateway's agent is a shell stand-in that prints reports the test made
//! (`support::ssh::CANNED_AGENT`); tests/pipeline_swtpm.rs runs the real `trustiot-agent
//! provision` against a software TPM instead.

#![cfg(target_os = "linux")]

mod support;

use std::collections::HashSet;
use std::path::Path;

use protocol::GatewayId;
use protocol::provisioning::KeyHandle;
use support::bench::{Bench, DESTINATION};
use support::ssh::{CANNED_AGENT, Call, FAKE_SUDO, GatewayKeys};
use workstation::gateway::{
    self, HOST_KEY_OPTIONS, KEPT_ENVIRONMENT, Mode, Request, SSH_OPTIONS, UploadedAgent,
};
use workstation::journal::State;

/// Variables a shell adds to its own environment, which `env` then lists.
const SHELL_VARIABLES: [&str; 4] = ["PWD", "OLDPWD", "SHLVL", "_"];

fn handle() -> KeyHandle {
    KeyHandle::new(gateway::DEFAULT_HANDLE).expect("a handle")
}

/// The options before `--` of every call for `hardware_id`: the fixed options, the host key
/// bound to the hardware ID in the tool's own file, and `accept-new` (the fake's configuration
/// leaves `StrictHostKeyChecking` at `ask`).
fn expected_options(bench: &Bench, hardware_id: &str) -> Vec<String> {
    let mut options: Vec<String> = SSH_OPTIONS
        .iter()
        .map(|&option| option.to_owned())
        .collect();
    options.extend([
        "-o".to_owned(),
        format!("HostKeyAlias=trustiot-{hardware_id}"),
        "-o".to_owned(),
        format!("UserKnownHostsFile={}", bench.known_hosts().as_str()),
    ]);
    options.extend(HOST_KEY_OPTIONS.iter().map(|&option| option.to_owned()));
    options.extend([
        "-o".to_owned(),
        "StrictHostKeyChecking=accept-new".to_owned(),
    ]);
    options
}

/// The argument vector ssh must get for `mode`: the options, `--`, the destination and the
/// remote command.
fn expected_argv(
    bench: &Bench,
    hardware_id: &str,
    mode: Mode,
    sudo: bool,
    upload: Option<&str>,
) -> Vec<String> {
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
    let mut argv = expected_options(bench, hardware_id);
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

/// The argument vector ssh must get for `provision install` (Laysi/trustiot-v2#617): the same
/// options as every provision call, host key binding included.
fn expected_install_argv(bench: &Bench, hardware_id: &str, sudo: bool) -> Vec<String> {
    let mut argv = expected_options(bench, hardware_id);
    argv.extend([
        "--".to_owned(),
        DESTINATION.to_owned(),
        gateway::install_command(hardware_id, sudo).expect("a command"),
    ]);
    argv
}

/// The DER of every `CERTIFICATE` block in `text`, ignoring anything around them.
fn certificates_in(text: &str) -> Vec<Vec<u8>> {
    text.split("-----BEGIN CERTIFICATE-----")
        .skip(1)
        .map(|part| {
            let body = part
                .split("-----END CERTIFICATE-----")
                .next()
                .expect("a block");
            support::registry::pem_der(body)
        })
        .collect()
}

/// `stdin` is `CERTIFICATE` blocks and nothing else, not even a line between them: their DER.
fn only_certificates(stdin: &[u8]) -> Vec<Vec<u8>> {
    let mut rest = std::str::from_utf8(stdin).expect("text");
    let mut blocks = Vec::new();
    while !rest.is_empty() {
        let body = rest
            .strip_prefix("-----BEGIN CERTIFICATE-----\n")
            .unwrap_or_else(|| panic!("only certificate blocks: {rest:?}"));
        let (body, after) = body
            .split_once("-----END CERTIFICATE-----\n")
            .expect("the block's end");
        assert!(!body.contains("-----"), "{body}");
        blocks.push(support::registry::pem_der(body));
        rest = after;
    }
    blocks
}

/// The `provision install` call `call` was for `hardware_id`, and its standard input is exactly
/// the recorded registration's certificate, issuing CA and root, as PEM, and nothing else
/// (Laysi/trustiot-v2#617, acceptance criterion 5).
fn assert_install_call(bench: &Bench, call: &Call, hardware_id: &str, sudo: bool) {
    assert_eq!(call.argv, expected_install_argv(bench, hardware_id, sudo));
    assert_eq!(call.destination, DESTINATION);
    let record = bench
        .journal()
        .load(hardware_id)
        .expect("readable")
        .expect("a record");
    let registration = record.registration.expect("a registration");
    let chain = certificates_in(&registration.certificate_chain);
    assert_eq!(chain.len(), 2, "the certificate and the issuing CA");
    assert_eq!(chain[0], certificates_in(&registration.certificate)[0]);
    let mut expected = chain;
    expected.extend(certificates_in(&registration.root_certificate));
    assert_eq!(expected.len(), 3);
    assert_eq!(
        only_certificates(&call.stdin),
        expected,
        "standard input is the chain and the root, nothing else"
    );
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
    // The certificates go to the agent installed on the gateway, never to the upload.
    bench.ssh.install_remote("trustiot-agent", CANNED_AGENT);

    let run = bench
        .run(&bench.register_args("tg-0001", &["--upload-agent", &agent]))
        .await;
    assert_eq!(run.code, Some(0), "{}", run.output());
    assert!(
        run.stdout
            .contains("installed the certificate chain and the root certificate"),
        "{}",
        run.stdout
    );
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
    assert_eq!(
        calls.len(),
        3,
        "an inspection, provision, then the installation"
    );
    assert_eq!(
        calls[0].argv,
        expected_argv(&bench, "tg-0001", Mode::Inspect, false, Some(&sha256))
    );
    assert_eq!(
        calls[1].argv,
        expected_argv(&bench, "tg-0001", Mode::Provision, false, Some(&sha256))
    );
    for call in &calls[..2] {
        assert_eq!(call.destination, DESTINATION);
        assert_eq!(
            call.stdin, bytes,
            "standard input is the agent, nothing else"
        );
    }
    assert_install_call(&bench, &calls[2], "tg-0001", false);
    assert_eq!(
        bench.ssh.read_remote_file("install-stdin").as_bytes(),
        calls[2].stdin,
        "the installed agent got all of it"
    );
    for call in &calls {
        assert_eq!(call.status, Some(0));
    }
    // Before each run, the installation's too, ssh -G with the same options (without
    // accept-new, which it decides).
    let mut config_call: Vec<String> = SSH_OPTIONS
        .iter()
        .map(|&option| option.to_owned())
        .collect();
    config_call.extend([
        "-o".to_owned(),
        "HostKeyAlias=trustiot-tg-0001".to_owned(),
        "-o".to_owned(),
        format!("UserKnownHostsFile={}", bench.known_hosts().as_str()),
    ]);
    config_call.extend(HOST_KEY_OPTIONS.iter().map(|&option| option.to_owned()));
    config_call.extend(["-G".to_owned(), "--".to_owned(), DESTINATION.to_owned()]);
    assert_eq!(
        bench.ssh.config_calls(),
        [config_call.clone(), config_call.clone(), config_call]
    );
    assert_environment_whitelisted(&bench);
    bench.assert_nothing_secret_crossed();
    assert_eq!(
        bench.ssh.agent_calls(),
        [
            "provision --hardware-id tg-0001 --handle 0x81000100 --inspect",
            "provision --hardware-id tg-0001 --handle 0x81000100",
            "provision install --hardware-id tg-0001",
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
        2,
        "the inspection found the key: no provision run, then the installation"
    );
    assert_eq!(
        calls[0].argv,
        expected_argv(&bench, "tg-0002", Mode::Inspect, true, None)
    );
    assert!(calls[0].stdin.is_empty(), "nothing on standard input");
    assert_install_call(&bench, &calls[1], "tg-0002", true);
    assert_eq!(
        bench.ssh.read_remote_file("sudo-calls").trim(),
        "trustiot-agent provision --hardware-id tg-0002 --handle 0x81000100 --inspect\n\
         trustiot-agent provision install --hardware-id tg-0002"
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
async fn an_ssh_configuration_that_skips_the_host_key_check_is_refused() {
    let bench = Bench::start("ssh-host-keys", Vec::new()).await;
    let keys = GatewayKeys::new();
    let (agent, _) = canned_agent(&bench);
    let upload = ["--upload-agent", agent.as_str()];
    let refused = [
        (
            "stricthostkeychecking false\n",
            false,
            "StrictHostKeyChecking no",
        ),
        ("userknownhostsfile /dev/null\n", true, "/dev/null"),
        (
            "userknownhostsfile ~/.ssh/known_hosts\n",
            true,
            "not only in the tool's own",
        ),
        (
            "knownhostscommand /usr/local/bin/host-keys %H\n",
            false,
            "KnownHostsCommand",
        ),
        (
            "globalknownhostsfile /etc/ssh/ssh_known_hosts\n",
            true,
            "system's files",
        ),
        ("verifyhostkeydns true\n", true, "VerifyHostKeyDNS"),
    ];
    for (lines, first, expected) in refused {
        bench.ssh.set_config(Some(lines), first);
        let run = bench.run(&bench.register_args("tg-0012", &upload)).await;
        assert_eq!(run.code, Some(1), "{lines}: {}", run.output());
        assert!(
            run.stderr
                .contains("would not check the gateway's host key")
                && run.stderr.contains(expected),
            "{lines}: {}",
            run.stderr
        );
        bench.ssh.set_config(None, first);
    }
    // Settings that never end are cut off at the limit instead of filling the memory.
    bench.ssh.flood_config(true);
    let run = bench.run(&bench.register_args("tg-0012", &upload)).await;
    assert_eq!(run.code, Some(1), "{}", run.output());
    assert!(run.stderr.contains("printed more than"), "{}", run.stderr);
    bench.ssh.flood_config(false);
    assert_eq!(bench.ssh.count(), 0, "nothing ran on the gateway");
    assert!(bench.registry.seen().is_empty(), "nothing was registered");

    // StrictHostKeyChecking yes is kept: no accept-new is added, the installation's call
    // included.
    bench
        .ssh
        .set_config(Some("stricthostkeychecking true\n"), false);
    bench.ssh.leave_reports(
        &keys.report("tg-0012", None),
        &keys.report("tg-0012", Some(true)),
    );
    bench.ssh.install_remote("trustiot-agent", CANNED_AGENT);
    let run = bench.run(&bench.register_args("tg-0012", &upload)).await;
    assert_eq!(run.code, Some(0), "{}", run.output());
    let mut kept = expected_options(&bench, "tg-0012");
    kept.truncate(kept.len() - 2);
    assert_eq!(bench.ssh.count(), 3, "inspect, provision, install");
    for call in bench.ssh.calls() {
        assert_eq!(call.argv.get(..kept.len()), Some(kept.as_slice()));
        assert_eq!(call.argv.get(kept.len()).map(String::as_str), Some("--"));
    }

    // A workstation directory whose path ssh would split or expand is refused before ssh runs.
    let before = bench.ssh.config_calls().len();
    for bad in ["with space", "per%cent"] {
        let mut arguments = bench.register_args("tg-0013", &upload);
        let dir = arguments
            .iter()
            .position(|argument| argument == "--dir")
            .expect("--dir")
            + 1;
        arguments[dir] = bench.scratch.join(bad).to_str().expect("UTF-8").to_owned();
        let run = bench.run(&arguments).await;
        assert_eq!(run.code, Some(2), "{bad}: {}", run.output());
        assert!(run.stderr.contains("--dir"), "{bad}: {}", run.stderr);
    }
    assert_eq!(bench.ssh.config_calls().len(), before, "ssh -G never ran");
}

#[tokio::test]
async fn an_agent_file_others_could_replace_is_never_uploaded() {
    let bench = Bench::start("ssh-agent-file", Vec::new()).await;
    let (agent, _) = canned_agent(&bench);
    for mode in [0o775, 0o757, 0o777] {
        support::set_mode(Path::new(&agent), mode);
        let run = bench
            .run(&bench.register_args("tg-0011", &["--upload-agent", &agent]))
            .await;
        assert_eq!(run.code, Some(1), "{mode:o}: {}", run.output());
        assert!(
            run.stderr
                .contains("must not be writable by the group or others"),
            "{mode:o}: {}",
            run.stderr
        );
    }
    assert_eq!(bench.ssh.count(), 0, "ssh never ran");
    support::set_mode(Path::new(&agent), 0o755);
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
    bench.ssh.install_remote("trustiot-agent", CANNED_AGENT);
    // 1: the connection fails; 2: the inspection runs but its answer is lost; 3: the
    // inspection; 4: provision runs but its answer is lost; 5: provision again; 6: the
    // installation runs but its answer is lost; 7: the installation again.
    bench.ssh.fail_before(1);
    bench.ssh.fail_after(2);
    bench.ssh.fail_after(4);
    bench.ssh.fail_after(6);

    let run = bench
        .run(&bench.register_args("tg-0003", &["--upload-agent", &agent]))
        .await;
    assert_eq!(run.code, Some(0), "{}", run.output());
    let calls = bench.ssh.calls();
    assert_eq!(calls.len(), 7);
    assert_eq!(
        calls.iter().map(|call| call.status).collect::<Vec<_>>(),
        [None, Some(0), Some(0), Some(0), Some(0), Some(0), Some(0)],
        "the first call never reached the remote command"
    );
    let sha256 = UploadedAgent::from_bytes(bytes.clone()).sha256().to_owned();
    let inspect = expected_argv(&bench, "tg-0003", Mode::Inspect, false, Some(&sha256));
    let provision = expected_argv(&bench, "tg-0003", Mode::Provision, false, Some(&sha256));
    for (number, call) in calls.iter().enumerate().take(5) {
        let expected = if number < 3 { &inspect } else { &provision };
        assert_eq!(&call.argv, expected, "call {}", number + 1);
        assert_eq!(call.stdin, bytes, "every attempt uploads the same agent");
    }
    for call in &calls[5..] {
        assert_install_call(&bench, call, "tg-0003", false);
    }
    assert_eq!(calls[1].stdout, calls[2].stdout, "the same inspection");
    assert_eq!(
        calls[3].stdout, calls[4].stdout,
        "the same provision report"
    );
    assert_eq!(bench.ssh.agent_calls().len(), 6);
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
    bench.ssh.install_remote("trustiot-agent", CANNED_AGENT);
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

/// What the run said about the installation and what the log's last line for `hardware_id` is.
fn assert_not_installed(bench: &Bench, run: &support::Run, hardware_id: &str, stop: &str) {
    assert!(
        run.stderr.contains(&format!(
            "hardware ID {hardware_id} is registered (recorded), but its certificates are not \
             installed on the gateway"
        )),
        "{}",
        run.stderr
    );
    assert!(
        run.stdout.contains("registered hardware ID")
            || run
                .stdout
                .contains("the platform already had the registration"),
        "the registration's own line comes first: {}",
        run.stdout
    );
    let record = bench
        .journal()
        .load(hardware_id)
        .expect("readable")
        .expect("a record");
    assert_eq!(record.state, State::Registered, "the registration stands");
    let last = bench.events(hardware_id).pop().expect("a log line");
    assert_eq!(last["event"], "install_stopped");
    assert_eq!(last["stop"], stop);
}

/// A gateway's `sudo` that runs everything with `-n` except the installation, for which the ssh
/// user has no rule: what `sudo -n` and `sudo -n -l` print and return then.
const SUDO_WITHOUT_INSTALL_RULE: &str = r#"#!/bin/sh
if [ "$1" != -n ]; then echo "sudo: a password is required" >&2; exit 1; fi
shift
case " $* " in
    *" install "*) echo "sudo: a password is required" >&2; exit 1 ;;
esac
exec "$@"
"#;

#[tokio::test]
async fn the_installation_after_the_registration_stops_with_its_own_exit_codes() {
    let bench = Bench::start("ssh-install", Vec::new()).await;
    let keys = GatewayKeys::new();
    let present = keys.report("tg-0011", Some(false));
    bench.ssh.leave_reports(&present, &present);
    bench.ssh.install_remote("trustiot-agent", CANNED_AGENT);
    let register = bench.register_args("tg-0011", &[]);

    // The gateway refuses the certificates (its 8): 32.
    bench.ssh.write_remote_file("status-install", b"8");
    let refused = bench.run(&register).await;
    assert_eq!(refused.code, Some(32), "{}", refused.output());
    assert!(refused.stderr.contains("refused"), "{}", refused.stderr);
    assert_not_installed(&bench, &refused, "tg-0011", "refused_by_gateway");

    // The agent's installation cannot be used yet (its 9): 33.
    bench.ssh.write_remote_file("status-install", b"9");
    let not_ready = bench.run(&register).await;
    assert_eq!(not_ready.code, Some(33), "{}", not_ready.output());
    assert_not_installed(&bench, &not_ready, "tg-0011", "gateway_not_ready");

    // An agent without provision install (its 2): 33, the advice to update it.
    bench.ssh.write_remote_file("status-install", b"2");
    let old = bench.run(&register).await;
    assert_eq!(old.code, Some(33), "{}", old.output());
    assert!(
        old.stderr.contains("does not know provision install"),
        "{}",
        old.stderr
    );

    // No agent installed: --upload-agent provisions, but the upload never gets the
    // certificates; 127 is 33 with the advice to install the agent.
    bench.ssh.reset_remote();
    bench.ssh.leave_reports(&present, &present);
    let (agent, bytes) = canned_agent(&bench);
    let before = bench.ssh.count();
    let bare = bench
        .run(&bench.register_args("tg-0011", &["--upload-agent", &agent]))
        .await;
    assert_eq!(bare.code, Some(33), "{}", bare.output());
    assert!(
        bare.stderr.contains("no trustiot-agent installed"),
        "{}",
        bare.stderr
    );
    assert_not_installed(&bench, &bare, "tg-0011", "gateway_not_ready");
    let calls = bench.ssh.calls();
    let install = calls.last().expect("the installation's call");
    assert_eq!(
        calls.len(),
        before + 2,
        "the inspection, then the installation"
    );
    assert_eq!(install.status, Some(127));
    assert_install_call(&bench, install, "tg-0011", false);
    assert_ne!(install.stdin, bytes, "the upload is not sent again");
    assert_eq!(
        bench.ssh.agent_calls(),
        ["provision --hardware-id tg-0011 --handle 0x81000100 --inspect"],
        "the uploaded agent only inspected"
    );
    assert!(bench.ssh.remote_tmp_entries().is_empty());

    // sudo on the gateway runs the inspection but has no rule for the installation: 33, the
    // advice names the sudoers rule rather than a rerun.
    bench.ssh.install_remote("trustiot-agent", CANNED_AGENT);
    bench.ssh.install_remote("sudo", SUDO_WITHOUT_INSTALL_RULE);
    let no_rule = bench
        .run(&bench.register_args("tg-0011", &["--remote-sudo"]))
        .await;
    assert_eq!(no_rule.code, Some(33), "{}", no_rule.output());
    assert!(
        no_rule.stderr.contains("sudoers rule") && no_rule.stderr.contains("Laysi/trustiot-v2#219"),
        "{}",
        no_rule.stderr
    );
    assert!(
        !no_rule
            .stderr
            .contains("Running the same command again is safe"),
        "{}",
        no_rule.stderr
    );
    assert_not_installed(&bench, &no_rule, "tg-0011", "sudo_refused");

    // sudo has the rule and the agent itself fails (its 1): 34, not a sudo refusal.
    bench.ssh.install_remote("sudo", FAKE_SUDO);
    bench.ssh.write_remote_file("status-install", b"1");
    let agent_failed = bench
        .run(&bench.register_args("tg-0011", &["--remote-sudo"]))
        .await;
    assert_eq!(agent_failed.code, Some(34), "{}", agent_failed.output());
    assert!(
        !agent_failed.stderr.contains("sudoers"),
        "{}",
        agent_failed.stderr
    );
    assert_not_installed(&bench, &agent_failed, "tg-0011", "failed");
    bench.ssh.write_remote_file("status-install", b"0");

    // The gateway stays unreachable for every attempt of the installation: 34, and the
    // registration stands.
    let first_install = bench.ssh.count() + 2;
    for call in 0..gateway::SSH_BACKOFF.max_attempts {
        bench
            .ssh
            .fail_before(first_install + usize::try_from(call).expect("fits"));
    }
    let unreachable = bench.run(&register).await;
    assert_eq!(unreachable.code, Some(34), "{}", unreachable.output());
    assert!(
        unreachable
            .stderr
            .contains("could not reach the gateway through ssh"),
        "{}",
        unreachable.stderr
    );
    assert_not_installed(&bench, &unreachable, "tg-0011", "failed");

    // The same command once the gateway is ready: 200, installed.
    let installed = bench.run(&register).await;
    assert_eq!(installed.code, Some(0), "{}", installed.output());
    assert!(
        installed
            .stdout
            .contains("installed the certificate chain and the root certificate"),
        "{}",
        installed.stdout
    );
    let last = bench.events("tg-0011").pop().expect("a log line");
    assert_eq!(last["event"], "installed");
    assert_install_call(
        &bench,
        bench.ssh.calls().last().expect("a call"),
        "tg-0011",
        false,
    );

    // The gateway's agent renewed the certificate since: it keeps that one (`renewed`), which
    // is success, said as such, never advice to remove it.
    bench.ssh.write_remote_file(
        "report-install",
        b"/var/lib/trustiot-agent/gateway.pem: renewed\n/etc/trustiot/broker-ca.pem: unchanged\n",
    );
    let kept = bench.run(&register).await;
    assert_eq!(kept.code, Some(0), "{}", kept.output());
    assert!(
        kept.stdout
            .contains("kept the certificate its agent renewed"),
        "{}",
        kept.stdout
    );
    assert!(
        !kept.output().contains("remove that file"),
        "{}",
        kept.output()
    );
    let last = bench.events("tg-0011").pop().expect("a log line");
    assert_eq!(last["event"], "installed");
    assert_eq!(last["kept_renewed"], true);
    assert_eq!(bench.registry.registrations(), 1, "one registration");
    assert!(
        bench
            .registry
            .seen()
            .iter()
            .skip(1)
            .all(|request| request.answered == 200),
        "every rerun got the same registration"
    );
    assert_environment_whitelisted(&bench);
    bench.assert_nothing_secret_crossed();
}

#[tokio::test]
async fn a_known_hardware_id_on_another_tpm_stops_with_27_even_with_allow_new_key() {
    let bench = Bench::start("ssh-another-tpm", Vec::new()).await;
    let keys = GatewayKeys::new();
    let (agent, _) = canned_agent(&bench);
    let register = |extra: &[&str]| {
        let mut arguments = vec!["--upload-agent", agent.as_str()];
        arguments.extend_from_slice(extra);
        bench.register_args("tg-0011", &arguments)
    };
    bench.ssh.leave_reports(
        &keys.report("tg-0011", None),
        &keys.report("tg-0011", Some(true)),
    );
    bench.ssh.install_remote("trustiot-agent", CANNED_AGENT);
    let first = bench.run(&register(&[])).await;
    assert_eq!(first.code, Some(0), "{}", first.output());
    let record = bench
        .journal()
        .load("tg-0011")
        .expect("readable")
        .expect("a record");

    // ssh reaches another gateway: its report echoes the hardware ID it was given, but its TPM
    // has another endorsement key, with a key at the handle or without one.
    let other = GatewayKeys::new();
    for (what, inspect) in [
        ("with a key", other.report("tg-0011", Some(false))),
        ("without a key", other.report("tg-0011", None)),
    ] {
        for extra in [&[][..], &["--allow-new-key"][..]] {
            bench.ssh.reset_remote();
            bench
                .ssh
                .leave_reports(&inspect, &other.report("tg-0011", Some(true)));
            let stopped = bench.run(&register(extra)).await;
            assert_eq!(
                stopped.code,
                Some(27),
                "{what} {extra:?}: {}",
                stopped.output()
            );
            assert!(
                stopped.stderr.contains("this is another TPM"),
                "{what} {extra:?}: {}",
                stopped.stderr
            );
            assert_eq!(
                bench.ssh.agent_calls(),
                ["provision --hardware-id tg-0011 --handle 0x81000100 --inspect"],
                "{what} {extra:?}: nothing was created"
            );
            let last = bench.events("tg-0011").pop().expect("a log line");
            assert_eq!(last["event"], "provision_stopped", "{what} {extra:?}");
            assert_eq!(last["stop"], "another_tpm", "{what} {extra:?}");
            assert_eq!(
                last["recorded_endorsement_key_sha256"],
                record.endorsement_key_sha256.as_str(),
                "{what} {extra:?}"
            );
            assert_ne!(
                last["endorsement_key_sha256"],
                record.endorsement_key_sha256.as_str(),
                "{what} {extra:?}"
            );
        }
    }
    assert_eq!(
        bench.registry.seen().len(),
        1,
        "only the first registration"
    );
    let kept = bench
        .journal()
        .load("tg-0011")
        .expect("readable")
        .expect("a record");
    assert_eq!(kept.endorsement_key_sha256, record.endorsement_key_sha256);
    assert_eq!(kept.public_key_sha256, record.public_key_sha256);
    assert_eq!(kept.state, State::Registered);
}

#[tokio::test]
async fn a_known_tpm_with_another_key_stops_with_20() {
    let bench = Bench::start("ssh-other-key", Vec::new()).await;
    let keys = GatewayKeys::new();
    let (agent, _) = canned_agent(&bench);
    let register = |extra: &[&str]| {
        let mut arguments = vec!["--upload-agent", agent.as_str()];
        arguments.extend_from_slice(extra);
        bench.register_args("tg-0012", &arguments)
    };
    bench.ssh.leave_reports(
        &keys.report("tg-0012", None),
        &keys.report("tg-0012", Some(true)),
    );
    bench.ssh.install_remote("trustiot-agent", CANNED_AGENT);
    let first = bench.run(&register(&[])).await;
    assert_eq!(first.code, Some(0), "{}", first.output());
    let original = keys.public_key_sha256();

    // The same TPM (the same endorsement key) has another key at the handle than the recorded
    // one: it was cleared and given a key since.
    let rekeyed = GatewayKeys {
        key: rcgen::KeyPair::generate().expect("a new key"),
        ek: keys.ek,
    };
    bench.ssh.reset_remote();
    bench.ssh.leave_reports(
        &rekeyed.report("tg-0012", Some(false)),
        &rekeyed.report("tg-0012", Some(false)),
    );
    let stopped = bench.run(&register(&[])).await;
    assert_eq!(stopped.code, Some(20), "{}", stopped.output());
    assert!(
        stopped
            .stderr
            .contains("has another key than the recorded one"),
        "{}",
        stopped.stderr
    );
    assert!(
        stopped.stderr.contains("--allow-new-key"),
        "{}",
        stopped.stderr
    );
    assert_eq!(
        bench.ssh.agent_calls(),
        ["provision --hardware-id tg-0012 --handle 0x81000100 --inspect"],
        "only the inspection"
    );
    let last = bench.events("tg-0012").pop().expect("a log line");
    assert_eq!(last["stop"], "key_absent");
    assert_eq!(last["recorded_public_key_sha256"], original.as_str());
    assert_eq!(
        last["public_key_sha256"],
        rekeyed.public_key_sha256().as_str()
    );
    assert_eq!(
        bench.registry.seen().len(),
        1,
        "only the first registration"
    );

    // With --allow-new-key the key is sent, and the platform refuses it.
    let refused = bench.run(&register(&["--allow-new-key"])).await;
    assert_eq!(refused.code, Some(10), "{}", refused.output());
    assert_eq!(
        bench.registry.seen().last().expect("a request").answered,
        409
    );
    let record = bench
        .journal()
        .load("tg-0012")
        .expect("readable")
        .expect("a record");
    assert_eq!(
        record.public_key_sha256, original,
        "the record keeps the registered key"
    );
}
