//! The whole `register --gateway` flow against a real TPM 2.0 implementation
//! (Laysi/trustiot-v2#616): the tool runs through the fake ssh (`support::ssh`), the uploaded or
//! installed "agent" on the fake remote is a bash relay that hands its arguments to this test,
//! and the test runs `trustiot-agent provision` itself (`agent::provision::run`) with every TPM
//! connection a new session of swtpm in a throwaway container (`testkit::swtpm`), then hands
//! back the exit status and standard output. `trustiot-agent provision install`
//! (Laysi/trustiot-v2#617) runs the same way (`agent::provision::install::run`), with the
//! standard input the relay passes on and a configuration and directories of this test, owned by
//! the test's user.
//!
//! One gateway, in order: the first registration (201) through a failed connection and a lost
//! answer, each retry reporting the same key, after which the certificates cannot be installed
//! (no agent installed: 33); `verify` shows the event; a rerun with the installed agent (200, the
//! same serial), which installs the chain and the root, and the agent's own start-up checks
//! (`agent::setup`) accept them with the TPM key; a reflash (the remote's files and the installed
//! certificates gone, a new TPM connection, the image's agent; 200, the same serial, the
//! certificates installed again); `tpm2_clear` (stopped with 20, nothing created); and
//! `--allow-new-key` (the platform's 409 `public_key_mismatch`, exit code 10).
//!
//! Without podman or docker the test skips, unless `CI` is set, where it fails.

#![cfg(target_os = "linux")]

mod support;

use std::io::{BufRead as _, BufReader, ErrorKind, Write as _};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use agent::config::Config;
use agent::provision::install::{self as agent_install, Owner, Owners};
use agent::provision::{Command, Exit, parse_args, run};
use agent::setup::Setup;
use agent::tpm::{TpmChannel, TpmTransport};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
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

/// The gateway's agent installation for `provision install`: a configuration naming
/// `etc/broker-ca.pem` and `state/gateway.pem` in a directory of this test, all owned by the
/// test's user (who stands in for root and the agent's account).
struct GatewayFiles {
    config: PathBuf,
    etc: PathBuf,
    state: PathBuf,
    owners: Owners,
}

impl GatewayFiles {
    fn new(directory: &Path) -> Self {
        std::fs::create_dir_all(directory).expect("a directory");
        let root = directory.canonicalize().expect("a canonical path");
        let etc = root.join("etc");
        let state = root.join("state");
        for path in [&etc, &state] {
            std::fs::create_dir(path).expect("a directory");
        }
        // Whatever the umask: install needs directories and a configuration only their owner
        // can write.
        for path in [&root, &etc, &state] {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("a mode");
        }
        let write = |name: &str, contents: &str| {
            let path = root.join(name);
            std::fs::write(&path, contents).expect("a file");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
                .expect("a mode");
            path
        };
        // The outer TLS's trust: any CA certificate; this test never connects.
        let mut params = rcgen::CertificateParams::new(Vec::new()).expect("params");
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "Outer CA");
        params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        let outer_key = rcgen::KeyPair::generate().expect("a key");
        let outer = write(
            "outer-ca.pem",
            &params.self_signed(&outer_key).expect("a CA").pem(),
        );
        let config = write(
            "agent.toml",
            &format!(
                "[cloud]\nurl = \"wss://gw.trustiot.example/mqtt\"\ntrust = [\"{}\"]\n\n\
                 [broker]\nca = \"{}\"\n\n\
                 [identity]\ncertificate = \"{}\"\nkey = {{ source = \"tpm\" }}\n",
                outer.display(),
                etc.join("broker-ca.pem").display(),
                state.join("gateway.pem").display(),
            ),
        );
        let metadata = std::fs::metadata(&config).expect("the configuration");
        let me = Owner {
            uid: metadata.uid(),
            gid: metadata.gid(),
        };
        Self {
            config,
            etc,
            state,
            owners: Owners {
                chain: me,
                root: me,
            },
        }
    }

    fn chain_path(&self) -> PathBuf {
        self.state.join("gateway.pem")
    }

    fn root_path(&self) -> PathBuf {
        self.etc.join("broker-ca.pem")
    }

    /// The installed files' contents and inodes, `None` for one that is missing.
    fn installed(&self) -> [Option<(u64, String)>; 2] {
        [self.chain_path(), self.root_path()].map(|path| {
            let metadata = std::fs::metadata(&path).ok()?;
            Some((
                metadata.ino(),
                std::fs::read_to_string(&path).expect("readable"),
            ))
        })
    }

    /// The agent's start-up checks of the installed files with the TPM key behind `swtpm`.
    fn agent_accepts(&self, swtpm: &Swtpm) {
        let config = Config::load(&self.config).expect("the configuration");
        let mut open = |_: &Path| {
            TpmTransport::Channel(Box::new(Connection {
                session: swtpm.session(),
                _closed: mpsc::channel().0,
            }))
        };
        if let Err(error) = Setup::new_with(&config, &mut open) {
            panic!("the agent refuses the installed files: {error}");
        }
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
    fn start(swtpm: Arc<Swtpm>, files: Arc<GatewayFiles>) -> Self {
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
                        Ok((stream, _)) => serve(&swtpm, &files, stream, &calls),
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

    /// The "agent" for the fake remote: sends its arguments and its standard input (base64),
    /// prints the answer's standard error and standard output, ends with its status.
    fn agent_script(&self) -> String {
        format!(
            "#!/bin/bash\n\
             input=$(base64 -w 0) || exit 1\n\
             exec 3<>/dev/tcp/127.0.0.1/{port} || exit 1\n\
             printf '%s\\0' \"$#\" \"$@\" \"$input\" >&3\n\
             IFS= read -r status <&3 || exit 1\n\
             IFS= read -r errors <&3 || exit 1\n\
             printf '%s' \"$errors\" | base64 -d >&2\n\
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

/// One relayed run: reads the argument count, the arguments and the standard input (base64; each
/// NUL-terminated), runs `trustiot-agent provision` or `trustiot-agent provision install`, waits
/// until every TPM session it opened is closed, and writes the status line, standard error
/// (base64, one line) and standard output.
fn serve(swtpm: &Swtpm, files: &GatewayFiles, stream: TcpStream, calls: &Mutex<Vec<Vec<String>>>) {
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
    let input = STANDARD.decode(field()).expect("base64 standard input");
    calls.lock().expect("unpoisoned").push(args.clone());
    assert_eq!(args.first().map(String::as_str), Some("provision"));
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut closed = Vec::new();
    let mut connect = || {
        let (sender, receiver) = mpsc::channel();
        closed.push(receiver);
        TpmTransport::Channel(Box::new(Connection {
            session: swtpm.session(),
            _closed: sender,
        }))
    };
    let code = if args.get(1).map(String::as_str) == Some(agent_install::SUBCOMMAND) {
        match agent_install::parse_args(args.iter().skip(2).map(Into::into)) {
            Ok(agent_install::Command::Install(mut options)) => {
                // The gateway's configuration is this test's; the command line never names one.
                options.config.clone_from(&files.config);
                agent_install::run(
                    &options,
                    &files.owners,
                    &mut |_: &Path| connect(),
                    &mut &input[..],
                    &mut stdout,
                    &mut stderr,
                )
                .code()
            }
            Ok(agent_install::Command::Help) => agent_install::Exit::Usage.code(),
            Err(message) => {
                stderr.extend_from_slice(message.as_bytes());
                agent_install::Exit::Usage.code()
            }
        }
    } else {
        assert!(input.is_empty(), "provision reads nothing");
        match parse_args(args.iter().skip(1).map(Into::into)) {
            Ok(Command::Provision(options)) => {
                run(&options, &mut connect, &mut stdout, &mut stderr).code()
            }
            Ok(Command::Help) => Exit::Usage.code(),
            Err(message) => {
                stderr.extend_from_slice(message.as_bytes());
                Exit::Usage.code()
            }
        }
    };
    for receiver in closed {
        match receiver.recv_timeout(WAIT) {
            Err(RecvTimeoutError::Disconnected) => {}
            other => panic!("the TPM thread did not close its session within {WAIT:?}: {other:?}"),
        }
    }
    let mut stream = stream;
    let _ = writeln!(stream, "{code}");
    let _ = writeln!(stream, "{}", STANDARD.encode(&stderr));
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
    let bench = Bench::start("swtpm", Vec::new()).await;
    let files = Arc::new(GatewayFiles::new(&bench.scratch.join("gateway-files")));
    let relay = Relay::start(Arc::clone(&swtpm), Arc::clone(&files));
    let script = relay.agent_script();
    let agent = bench.agent_file("trustiot-agent", script.as_bytes());
    let agent = agent.to_str().expect("UTF-8").to_owned();
    let upload = ["--upload-agent", agent.as_str()];

    // 1. The first registration: the first connection fails; provision (call 3) creates the
    //    key but its answer is lost, so it runs again. No agent is installed on the gateway, so
    //    the certificates cannot be installed (call 5 finds no trustiot-agent): 33, and the
    //    registration stands.
    bench.ssh.fail_before(1);
    bench.ssh.fail_after(3);
    let first = bench.run(&bench.register_args(HARDWARE_ID, &upload)).await;
    assert_eq!(first.code, Some(33), "{}", first.output());
    assert!(
        first
            .stdout
            .contains(&format!("registered hardware ID {HARDWARE_ID}")),
        "{}",
        first.stdout
    );
    assert!(
        first.stderr.contains("no trustiot-agent installed"),
        "{}",
        first.stderr
    );
    assert_eq!(answered(&bench), [201]);
    assert_eq!(bench.ssh.count(), 5);
    assert_eq!(bench.ssh.call(5).status, Some(127));
    assert_eq!(files.installed(), [None, None], "nothing was installed");
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
    //    found by the inspection, so provision does not run; the installed agent checks the
    //    chain and the root with its TPM key and installs them, and its start-up checks accept
    //    them.
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
    assert!(
        rerun
            .stdout
            .contains("installed the certificate chain and the root certificate"),
        "{}",
        rerun.stdout
    );
    assert_eq!(
        bench.ssh.count(),
        calls + 2,
        "only the inspection and the installation"
    );
    assert_eq!(answered(&bench), [201, 200]);
    let installed = files.installed();
    let [Some((_, chain)), Some((_, root))] = &installed else {
        panic!("both files are installed: {installed:?}");
    };
    let record = bench
        .journal()
        .load(HARDWARE_ID)
        .expect("readable")
        .expect("a record");
    let registration = record.registration.expect("a registration");
    assert_eq!(
        support::registry::pem_der(root),
        support::registry::pem_der(&registration.root_certificate),
        "broker.ca is the registration's root"
    );
    assert_eq!(
        chain.matches("-----BEGIN CERTIFICATE-----").count(),
        2,
        "identity.certificate is the certificate and the issuing CA: {chain}"
    );
    files.agent_accepts(&swtpm);

    // 4. A reflash: the gateway's files are gone (the installed certificates too), the TPM
    //    connection is new and the TPM keeps its key; the image brings the agent back. With
    //    --upload-agent for provisioning: 200, the same serial, and the image's agent installs
    //    the certificates again.
    bench.ssh.reset_remote();
    std::fs::remove_file(files.chain_path()).expect("removed");
    std::fs::remove_file(files.root_path()).expect("removed");
    swtpm.disconnect();
    bench.ssh.install_remote("trustiot-agent", &script);
    let reflashed = bench.run(&bench.register_args(HARDWARE_ID, &upload)).await;
    assert_eq!(reflashed.code, Some(0), "{}", reflashed.output());
    assert!(
        reflashed.stdout.contains(&format!("serial {serial}")),
        "{}",
        reflashed.stdout
    );
    assert!(
        reflashed
            .stdout
            .contains("installed the certificate chain and the root certificate"),
        "{}",
        reflashed.stdout
    );
    assert_eq!(answered(&bench), [201, 200, 200]);
    assert_eq!(
        files.installed().map(|file| file.map(|(_, text)| text)),
        installed.clone().map(|file| file.map(|(_, text)| text)),
        "the same files again"
    );
    files.agent_accepts(&swtpm);
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

    // What the agent was asked, every time: provision with the ID and the handle only, or
    // provision install with the ID only.
    let handle = format!("{DEFAULT_HANDLE:#010x}");
    let (installs, provisions): (Vec<_>, Vec<_>) = relay
        .calls()
        .into_iter()
        .partition(|args| args.get(1).map(String::as_str) == Some("install"));
    assert_eq!(installs.len(), 2, "steps 3 and 4");
    for args in installs {
        assert_eq!(args, ["provision", "install", "--hardware-id", HARDWARE_ID]);
    }
    for args in provisions {
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
            forced.code
        ],
        answered(&bench),
        bench.ssh.count(),
        relay.calls().len(),
        swtpm.persistent_handles()
    );
}
