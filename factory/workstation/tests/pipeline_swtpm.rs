//! The whole `register --gateway` flow against a real TPM 2.0 implementation
//! (Laysi/trustiot-v2#616): the tool runs through the fake ssh (`support::ssh`), the uploaded or
//! installed "agent" on the fake remote is a bash relay that hands its arguments to this test,
//! and the test runs `trustiot-agent provision` itself (`agent::provision::run`) with every TPM
//! connection a new session of swtpm in a throwaway container (`testkit::swtpm`), then hands
//! back the exit status and standard output.
//!
//! One gateway, in order: the first registration (201) through a failed connection and a lost
//! answer, each retry reporting the same key; `verify` shows the event; a rerun with the
//! installed agent (200, the same serial); a reflash (the remote's files gone, a new TPM
//! connection; 200, the same serial); `tpm2_clear` (stopped with 20, nothing created); and
//! `--allow-new-key` (the platform's 409 `public_key_mismatch`, exit code 10); then the key that
//! run created, on the same TPM with the same endorsement key (stopped with 20, nothing sent).
//!
//! Without podman or docker the test skips, unless `CI` is set, where it fails.

#![cfg(target_os = "linux")]

mod support;

use std::io::{BufRead as _, BufReader, ErrorKind, Write as _};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use agent::provision::{Command, Exit, parse_args, run};
use agent::tpm::{TpmChannel, TpmTransport};
use protocol::provisioning::{GatewayKey, ProvisioningReport};
use sha2::{Digest as _, Sha256};
use support::bench::Bench;
use testkit::swtpm::{SRK_HANDLE, Session, Swtpm};
use workstation::gateway::DEFAULT_HANDLE;
use workstation::journal::State;

const HARDWARE_ID: &str = "tg-swtpm-0001";

/// How long the relay waits for a TPM session to close, or for the next byte of a request:
/// milliseconds unless the code is wrong.
const WAIT: Duration = Duration::from_secs(60);

/// How often the relay looks for a new connection or the end of the test.
const POLL: Duration = Duration::from_millis(20);

/// One TPM connection of a run: a session that tells the relay when it is closed.
struct Connection {
    session: Session,
    _closed: mpsc::Sender<()>,
}

impl TpmChannel for Connection {
    fn transact(&mut self, command: &[u8]) -> std::io::Result<Vec<u8>> {
        self.session.transact(command)
    }
}

/// Runs `trustiot-agent` for the bash relay on the fake remote: one request per connection.
struct Relay {
    port: u16,
    stop: Arc<AtomicBool>,
    calls: Arc<Mutex<Vec<Vec<String>>>>,
    thread: Option<JoinHandle<()>>,
}

impl Relay {
    fn start(swtpm: Arc<Swtpm>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bound");
        listener.set_nonblocking(true).expect("non-blocking");
        let port = listener.local_addr().expect("an address").port();
        let stop = Arc::new(AtomicBool::new(false));
        let calls = Arc::new(Mutex::new(Vec::new()));
        let thread = {
            let stop = Arc::clone(&stop);
            let calls = Arc::clone(&calls);
            std::thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((stream, _)) => serve(&swtpm, stream, &calls),
                        Err(error) if error.kind() == ErrorKind::WouldBlock => {
                            std::thread::sleep(POLL);
                        }
                        Err(error) => panic!("the relay cannot accept: {error}"),
                    }
                }
            })
        };
        Self {
            port,
            stop,
            calls,
            thread: Some(thread),
        }
    }

    /// The "agent" for the fake remote: sends its arguments, prints the answer, ends with its
    /// status.
    fn agent_script(&self) -> String {
        format!(
            "#!/bin/bash\n\
             exec 3<>/dev/tcp/127.0.0.1/{port} || exit 1\n\
             printf '%s\\0' \"$#\" \"$@\" >&3\n\
             IFS= read -r status <&3 || exit 1\n\
             cat <&3\n\
             exit \"$status\"\n",
            port = self.port
        )
    }

    /// The arguments of every run so far.
    fn calls(&self) -> Vec<Vec<String>> {
        self.calls.lock().expect("unpoisoned").clone()
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take()
            && let Err(panic) = thread.join()
            && !std::thread::panicking()
        {
            std::panic::resume_unwind(panic);
        }
    }
}

/// One relayed run: reads the argument count and the arguments (each NUL-terminated), runs
/// `trustiot-agent provision`, waits until every TPM session it opened is closed, and writes
/// the status line and standard output.
fn serve(swtpm: &Swtpm, stream: TcpStream, calls: &Mutex<Vec<Vec<String>>>) {
    stream.set_nonblocking(false).expect("blocking");
    stream.set_read_timeout(Some(WAIT)).expect("a timeout");
    let mut reader = BufReader::new(stream.try_clone().expect("a clone"));
    let mut field = || {
        let mut bytes = Vec::new();
        reader.read_until(0, &mut bytes).expect("a field");
        assert_eq!(bytes.pop(), Some(0), "a NUL-terminated field");
        String::from_utf8(bytes).expect("UTF-8")
    };
    let count: usize = field().parse().expect("the argument count");
    let args: Vec<String> = (0..count).map(|_| field()).collect();
    calls.lock().expect("unpoisoned").push(args.clone());
    assert_eq!(args.first().map(String::as_str), Some("provision"));
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut closed = Vec::new();
    let exit = match parse_args(args.iter().skip(1).map(Into::into)) {
        Ok(Command::Provision(options)) => {
            let mut connect = || {
                let (sender, receiver) = mpsc::channel();
                closed.push(receiver);
                TpmTransport::Channel(Box::new(Connection {
                    session: swtpm.session(),
                    _closed: sender,
                }))
            };
            run(&options, &mut connect, &mut stdout, &mut stderr)
        }
        Ok(Command::Help) => Exit::Usage,
        Err(message) => {
            stderr.extend_from_slice(message.as_bytes());
            Exit::Usage
        }
    };
    for receiver in closed {
        match receiver.recv_timeout(WAIT) {
            Err(RecvTimeoutError::Disconnected) => {}
            other => panic!("the TPM thread did not close its session within {WAIT:?}: {other:?}"),
        }
    }
    let mut stream = stream;
    let _ = writeln!(stream, "{}", exit.code());
    let _ = stream.write_all(&stdout);
    let _ = stream.flush();
    let _ = stream.shutdown(std::net::Shutdown::Both);
}

/// The report call `n` of the fake ssh printed.
fn report_of(bench: &Bench, n: usize) -> ProvisioningReport {
    serde_json::from_slice(&bench.ssh.call(n).stdout).expect("a report")
}

/// The report's public key and whether the run created it.
fn key_of(report: &ProvisioningReport) -> (String, bool) {
    match &report.key {
        GatewayKey::Present {
            created,
            public_key,
            ..
        } => (public_key.clone(), *created),
        GatewayKey::Absent => panic!("the key is absent: {report:?}"),
    }
}

/// The SHA-256 of a PEM public key's DER, lower-case hex.
fn spki_sha256(pem: &str) -> String {
    let der = support::registry::pem_der(pem);
    Sha256::digest(der)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn answered(bench: &Bench) -> Vec<u16> {
    bench
        .registry
        .seen()
        .iter()
        .map(|request| request.answered)
        .collect()
}

#[tokio::test]
async fn a_gateway_registers_reruns_and_stops_after_tpm2_clear() {
    let Some(swtpm) = Swtpm::start() else {
        return;
    };
    swtpm.create_standard_srk();
    let swtpm = Arc::new(swtpm);
    let relay = Relay::start(Arc::clone(&swtpm));
    let bench = Bench::start("swtpm", Vec::new()).await;
    let script = relay.agent_script();
    let agent = bench.agent_file("trustiot-agent", script.as_bytes());
    let agent = agent.to_str().expect("UTF-8").to_owned();
    let upload = ["--upload-agent", agent.as_str()];

    // 1. The first registration: the first connection fails; provision (call 3) creates the
    //    key but its answer is lost, so it runs again.
    bench.ssh.fail_before(1);
    bench.ssh.fail_after(3);
    let first = bench.run(&bench.register_args(HARDWARE_ID, &upload)).await;
    assert_eq!(first.code, Some(0), "{}", first.output());
    assert!(
        first
            .stdout
            .contains(&format!("registered hardware ID {HARDWARE_ID}")),
        "{}",
        first.stdout
    );
    assert_eq!(answered(&bench), [201]);
    assert_eq!(bench.ssh.count(), 4);
    assert_eq!(bench.ssh.call(1).status, None, "never reached the gateway");
    let inspected = report_of(&bench, 2);
    assert!(matches!(inspected.key, GatewayKey::Absent));
    let (lost, lost_created) = key_of(&report_of(&bench, 3));
    let (kept, kept_created) = key_of(&report_of(&bench, 4));
    assert!(lost_created, "the lost run created the key");
    assert!(!kept_created, "its retry found it");
    assert_eq!(lost, kept, "each retry reports the same key");
    assert_eq!(
        report_of(&bench, 3).endorsement_key,
        report_of(&bench, 4).endorsement_key
    );
    assert_eq!(
        inspected.endorsement_key,
        report_of(&bench, 4).endorsement_key
    );
    assert_eq!(swtpm.persistent_handles(), [SRK_HANDLE, DEFAULT_HANDLE]);
    let record = bench
        .journal()
        .load(HARDWARE_ID)
        .expect("readable")
        .expect("a record");
    assert_eq!(record.state, State::Registered);
    assert_eq!(record.public_key_sha256, spki_sha256(&kept));
    let serial = record.registration.expect("a registration").serial;

    // 2. verify shows the event of the registration.
    let verified = bench.run(&bench.verify_args(HARDWARE_ID)).await;
    assert_eq!(verified.code, Some(0), "{}", verified.output());
    assert!(
        verified.stdout.contains(&format!("serial {serial}")),
        "{}",
        verified.stdout
    );
    assert!(
        verified
            .stdout
            .contains("it matches this workstation's record"),
        "{}",
        verified.stdout
    );

    // 3. A rerun with the agent installed on the gateway: 200 and the same serial; the key is
    //    found by the inspection, so provision does not run.
    bench.ssh.install_remote("trustiot-agent", &script);
    let calls = bench.ssh.count();
    let rerun = bench.run(&bench.register_args(HARDWARE_ID, &[])).await;
    assert_eq!(rerun.code, Some(0), "{}", rerun.output());
    assert!(
        rerun.stdout.contains("already there and reused"),
        "{}",
        rerun.stdout
    );
    assert!(
        rerun.stdout.contains(&format!("serial {serial}")),
        "{}",
        rerun.stdout
    );
    assert_eq!(bench.ssh.count(), calls + 1, "only the inspection");
    assert_eq!(answered(&bench), [201, 200]);

    // 4. A reflash: the gateway's files are gone (no installed agent) and the TPM connection
    //    is new; the TPM keeps its key.
    bench.ssh.reset_remote();
    swtpm.disconnect();
    let reflashed = bench.run(&bench.register_args(HARDWARE_ID, &upload)).await;
    assert_eq!(reflashed.code, Some(0), "{}", reflashed.output());
    assert!(
        reflashed.stdout.contains(&format!("serial {serial}")),
        "{}",
        reflashed.stdout
    );
    assert_eq!(answered(&bench), [201, 200, 200]);
    assert!(bench.ssh.remote_tmp_entries().is_empty());

    // 5. tpm2_clear: a new device (decision 12). The inspection finds no key, the tool stops
    //    with 20 and creates nothing.
    let _ = swtpm.tools("tpm2_clear");
    assert!(swtpm.persistent_handles().is_empty());
    let calls = bench.ssh.count();
    let cleared = bench.run(&bench.register_args(HARDWARE_ID, &upload)).await;
    assert_eq!(cleared.code, Some(20), "{}", cleared.output());
    assert!(
        cleared.stderr.contains("--allow-new-key"),
        "{}",
        cleared.stderr
    );
    assert_eq!(bench.ssh.count(), calls + 1, "only the inspection");
    assert!(swtpm.persistent_handles().is_empty(), "nothing was created");
    assert_eq!(answered(&bench), [201, 200, 200], "nothing was sent");

    // 6. --allow-new-key: the new key is created and sent; the platform refuses the hardware
    //    ID registered with another key (409 public_key_mismatch) and the tool ends with 10.
    let forced = bench
        .run(&bench.register_args(HARDWARE_ID, &[upload[0], upload[1], "--allow-new-key"]))
        .await;
    assert_eq!(forced.code, Some(10), "{}", forced.output());
    assert_eq!(answered(&bench), [201, 200, 200, 409]);
    assert_eq!(swtpm.persistent_handles(), [DEFAULT_HANDLE]);
    let record = bench
        .journal()
        .load(HARDWARE_ID)
        .expect("readable")
        .expect("a record");
    assert_eq!(record.state, State::Registered);
    assert_eq!(
        record.registration.expect("a registration").serial,
        serial,
        "the record keeps the registration"
    );

    // 7. The same TPM (tpm2_clear kept its endorsement key) now holds the key step 6 created,
    //    not the recorded one: handled like no key, 20, and nothing is sent.
    let calls = bench.ssh.count();
    let other_key = bench.run(&bench.register_args(HARDWARE_ID, &upload)).await;
    assert_eq!(other_key.code, Some(20), "{}", other_key.output());
    assert!(
        other_key
            .stderr
            .contains("has another key than the recorded one"),
        "{}",
        other_key.stderr
    );
    assert_eq!(bench.ssh.count(), calls + 1, "only the inspection");
    assert_eq!(answered(&bench), [201, 200, 200, 409], "nothing was sent");

    // What the agent was asked, every time: provision with the ID and the handle only.
    let handle = format!("{DEFAULT_HANDLE:#010x}");
    for args in relay.calls() {
        assert_eq!(
            args.get(..5).expect("five arguments"),
            [
                "provision",
                "--hardware-id",
                HARDWARE_ID,
                "--handle",
                handle.as_str()
            ]
        );
        assert!(args.len() == 5 || (args.len() == 6 && args[5] == "--inspect"));
    }
    assert!(bench.ssh.remote_tmp_entries().is_empty());
    bench.assert_nothing_secret_crossed();
    println!(
        "evidence: exit codes {:?}; platform answers {:?}; ssh calls {}; agent runs {}; \
         persistent handles after --allow-new-key {:?}",
        [
            first.code,
            rerun.code,
            reflashed.code,
            cleared.code,
            forced.code,
            other_key.code
        ],
        answered(&bench),
        bench.ssh.count(),
        relay.calls().len(),
        swtpm.persistent_handles()
    );
}
