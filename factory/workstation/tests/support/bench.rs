//! A workstation ready for `register --gateway` (Laysi/trustiot-v2#616): a key, a certificate, a
//! platform administrator's session, a `--headers-file`, the registration stand-in
//! ([`super::registry`]) and the fake ssh ([`super::ssh`]) first on the tool's `PATH`.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use contract::certs::{
    WORKSTATION_CERTIFICATE_HEADER, WORKSTATION_NONCE_HEADER, WORKSTATION_SIGNATURE_HEADER,
    WORKSTATION_TIMESTAMP_HEADER,
};
use rcgen::KeyPair;
use sha2::{Digest as _, Sha256};
use workstation::config::Origin;
use workstation::identity::{self, WorkstationDirectory};
use workstation::journal::Journal;
use workstation::secret::SessionCookie;
use workstation::session::{Session, SessionStore};

use super::registry::{self, Action, Registry};
use super::ssh::FakeSsh;
use super::{Environment, FakeSecrets, Profile, Run, Scratch, TestCa, pem_body_lines, run};

/// The ssh destination every test gives.
pub const DESTINATION: &str = "factory@gateway-under-test";

fn now() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("after 1970")
            .as_secs(),
    )
    .expect("fits")
}

pub struct Bench {
    pub scratch: Scratch,
    pub environment: Environment,
    pub secrets: FakeSecrets,
    /// A value the stand-in puts in a `Set-Cookie` of every answer.
    pub planted: String,
    pub directory: WorkstationDirectory,
    pub registry: Registry,
    pub ssh: FakeSsh,
    /// `--headers-file`.
    pub headers_file: PathBuf,
    /// `--root-sha256`: the stand-in's root.
    pub root_sha256: String,
    certificate: String,
}

impl Bench {
    /// A bench whose stand-in answers registrations with `script`, then [`Action::Answer`].
    pub async fn start(name: &str, script: Vec<Action>) -> Self {
        let scratch = Scratch::new(name);
        let mut environment = Environment::in_scratch(&scratch);
        let secrets = FakeSecrets::new();
        let planted = FakeSecrets::new().cookie;
        let directory = WorkstationDirectory::new(scratch.join("ws"));
        identity::init(&directory).expect("init");
        let key = KeyPair::from_pem(&std::fs::read_to_string(directory.key()).expect("the key"))
            .expect("a key");
        let certificate = TestCa::new().issue(&key, &Profile::workstation("line-a-ws-01"));
        identity::install_certificate(&directory, certificate.as_bytes(), now())
            .expect("installed");
        let registry = Registry::start(
            &secrets.cookie,
            registry::pem_der(&certificate),
            &planted,
            script,
        )
        .await;
        let store = SessionStore::new(
            environment
                .runtime
                .as_ref()
                .expect("a runtime directory")
                .join("trustiot-workstation"),
        );
        store
            .save(&Session {
                origin: Origin::parse(&registry.url).expect("a loopback origin"),
                cookie: SessionCookie::parse(&secrets.cookie).expect("a cookie"),
            })
            .expect("the session saved");
        let headers_file = scratch.join("access-headers");
        std::fs::write(&headers_file, secrets.headers_file()).expect("the headers file");
        super::set_mode(&headers_file, 0o600);
        let root_sha256 = Sha256::digest(registry::pem_der(registry.root_pem()))
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let ssh = FakeSsh::install(&scratch);
        environment.path = Some(ssh.path());
        Self {
            scratch,
            environment,
            secrets,
            planted,
            directory,
            registry,
            ssh,
            headers_file,
            root_sha256,
            certificate,
        }
    }

    pub fn journal(&self) -> Journal {
        Journal::new(self.directory.path())
    }

    /// Writes `bytes` as an executable under the scratch directory, for `--upload-agent`.
    pub fn agent_file(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let path = self.scratch.join(name);
        std::fs::write(&path, bytes).expect("the agent file");
        super::set_mode(&path, 0o755);
        path
    }

    /// `register --hardware-id <id> --gateway DESTINATION --root-sha256 <root> --dir <dir>
    /// --headers-file <file>`, then `extra`.
    pub fn register_args(&self, hardware_id: &str, extra: &[&str]) -> Vec<String> {
        let path = |path: &Path| path.to_str().expect("a UTF-8 path").to_owned();
        let mut arguments = vec![
            "register".to_owned(),
            "--hardware-id".to_owned(),
            hardware_id.to_owned(),
            "--gateway".to_owned(),
            DESTINATION.to_owned(),
            "--root-sha256".to_owned(),
            self.root_sha256.clone(),
            "--dir".to_owned(),
            path(self.directory.path()),
            "--headers-file".to_owned(),
            path(&self.headers_file),
        ];
        arguments.extend(extra.iter().map(|argument| (*argument).to_owned()));
        arguments
    }

    /// `verify --hardware-id <id> --dir <dir> --headers-file <file>`.
    pub fn verify_args(&self, hardware_id: &str) -> Vec<String> {
        vec![
            "verify".to_owned(),
            "--hardware-id".to_owned(),
            hardware_id.to_owned(),
            "--dir".to_owned(),
            self.directory.path().to_str().expect("UTF-8").to_owned(),
            "--headers-file".to_owned(),
            self.headers_file.to_str().expect("UTF-8").to_owned(),
        ]
    }

    /// Runs the tool with `arguments`.
    pub async fn run(&self, arguments: &[String]) -> Run {
        let arguments: Vec<&str> = arguments.iter().map(String::as_str).collect();
        run(&self.environment, &arguments, "").await
    }

    /// The log's event names for `hardware_id`, in order.
    pub fn event_names(&self, hardware_id: &str) -> Vec<String> {
        self.events(hardware_id)
            .iter()
            .map(|event| event["event"].as_str().expect("an event").to_owned())
            .collect()
    }

    /// The log's lines for `hardware_id`, in order.
    pub fn events(&self, hardware_id: &str) -> Vec<serde_json::Value> {
        let text = std::fs::read_to_string(self.journal().log_path()).unwrap_or_default();
        text.lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("a JSON line"))
            .filter(|value| value["hardware_id"] == hardware_id)
            .collect()
    }

    /// What must never reach the gateway: the session cookie, a cookie the platform set, the
    /// login code, the Access service token, a line of the workstation key or certificate, the
    /// workstation header names and every signature and nonce sent to the platform.
    pub fn never_to_the_gateway(&self) -> Vec<(&'static str, String)> {
        let mut forbidden: Vec<(&'static str, String)> = vec![
            ("the session cookie", self.secrets.cookie.clone()),
            ("a Set-Cookie value of an answer", self.planted.clone()),
            ("the login code", self.secrets.code.clone()),
            ("the Access client id", self.secrets.access_id.clone()),
            (
                "the Access client secret",
                self.secrets.access_secret.clone(),
            ),
            ("an Access header name", "cf-access-client".to_owned()),
            (
                "the certificate header name",
                WORKSTATION_CERTIFICATE_HEADER.to_owned(),
            ),
            (
                "the signature header name",
                WORKSTATION_SIGNATURE_HEADER.to_owned(),
            ),
            ("the nonce header name", WORKSTATION_NONCE_HEADER.to_owned()),
            (
                "the timestamp header name",
                WORKSTATION_TIMESTAMP_HEADER.to_owned(),
            ),
            (
                "the workstation directory",
                self.directory.path().display().to_string(),
            ),
        ];
        let key = std::fs::read_to_string(self.directory.key()).expect("the key");
        forbidden.extend(
            pem_body_lines(&key)
                .into_iter()
                .map(|line| ("a line of the workstation key", line)),
        );
        forbidden.extend(
            pem_body_lines(&self.certificate)
                .into_iter()
                .map(|line| ("a line of the workstation certificate", line)),
        );
        for request in self.registry.seen() {
            for (what, name) in [
                ("a signature sent", WORKSTATION_SIGNATURE_HEADER),
                ("a nonce sent", WORKSTATION_NONCE_HEADER),
                (
                    "the certificate header sent",
                    WORKSTATION_CERTIFICATE_HEADER,
                ),
            ] {
                let value = request.header(name);
                if !value.is_empty() {
                    forbidden.push((what, value.to_owned()));
                }
            }
        }
        forbidden
    }

    /// Fails if anything of [`Self::never_to_the_gateway`] is in an ssh call's arguments,
    /// environment or standard input.
    pub fn assert_nothing_secret_crossed(&self) {
        let forbidden = self.never_to_the_gateway();
        let calls = self.ssh.calls();
        assert!(!calls.is_empty(), "ssh was called");
        for (number, call) in calls.iter().enumerate() {
            let stdin = String::from_utf8_lossy(&call.stdin);
            let places = [
                ("arguments", call.argv.join("\n")),
                ("environment", call.env.join("\n")),
                ("standard input", stdin.into_owned()),
            ];
            for (place, text) in &places {
                for (what, value) in &forbidden {
                    assert!(!value.is_empty(), "{what} is set");
                    assert!(
                        !text.contains(value.as_str()),
                        "{what} reached the gateway in call {}'s {place}",
                        number + 1
                    );
                }
            }
        }
    }
}
