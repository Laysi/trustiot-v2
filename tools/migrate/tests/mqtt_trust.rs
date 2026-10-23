//! The MQTT broker's trust init container and sidecar (`migrate mqtt-broker-trust`,
//! Laysi/trustiot-v2#84) against a fake certs service, a Secret mounted the way the kubelet
//! mounts it, and a fake `/proc` whose broker is a real process that counts its SIGHUPs:
//!
//! - the init container installs what the certs service serves; with no answer and nothing
//!   installed, or without a certs service, or when the certs service has no usable issuing CA,
//!   it installs the placeholder that trusts no gateway; with no answer or unusable files it
//!   keeps the last good pair; it refuses a server certificate whose key does not match;
//! - the sidecar has the broker reload once at its start and after every change of the trust
//!   files or the server certificate, and never while the server certificate would not load.
//!
//! That Mosquitto loads these files and applies a reload is tested in mqtt_broker.rs.

mod support;

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use migrate::Report;
use migrate::config::MqttTrustSettings;
use migrate::mqtt_trust::{CA_PATH, CRL_PATH, TrustSet, installed, once, run, validate};
use support::pki::{self, Ca};
use support::{default_sighup, scratch_dir};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpListener;

/// Generous for a loaded machine; the sidecar runs every second here.
const WAIT: Duration = Duration::from_secs(120);

/// What the fake certs service answers for each path: status and body.
#[derive(Clone)]
struct Answers {
    ca: (u16, String),
    crl: (u16, String),
}

/// A certs service that serves `Answers` and counts requests.
#[derive(Clone)]
struct FakeCerts {
    url: String,
    answers: Arc<Mutex<Answers>>,
    requests: Arc<Mutex<usize>>,
}

impl FakeCerts {
    async fn start(answers: Answers) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let fake = Self {
            url: format!(
                "http://127.0.0.1:{}",
                listener.local_addr().expect("address").port()
            ),
            answers: Arc::new(Mutex::new(answers)),
            requests: Arc::default(),
        };
        let serving = fake.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let fake = serving.clone();
                tokio::spawn(async move {
                    let mut head = Vec::new();
                    let mut buffer = [0_u8; 1024];
                    while !head.windows(4).any(|window| window == b"\r\n\r\n") {
                        match stream.read(&mut buffer).await {
                            Ok(0) | Err(_) => return,
                            Ok(read) => head.extend_from_slice(&buffer[..read]),
                        }
                    }
                    let line = String::from_utf8_lossy(&head)
                        .lines()
                        .next()
                        .unwrap_or_default()
                        .to_owned();
                    let answers = fake.answers.lock().expect("answers").clone();
                    *fake.requests.lock().expect("requests") += 1;
                    let (status, body) = if line.starts_with(&format!("GET {CA_PATH} ")) {
                        answers.ca
                    } else if line.starts_with(&format!("GET {CRL_PATH} ")) {
                        answers.crl
                    } else {
                        (404, String::new())
                    };
                    let response = format!(
                        "HTTP/1.1 {status} X\r\ncontent-type: application/x-pem-file\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        fake
    }

    fn answer(&self, answers: Answers) {
        *self.answers.lock().expect("answers") = answers;
    }

    fn requests(&self) -> usize {
        *self.requests.lock().expect("requests")
    }
}

/// The gateway CA as the certs service has it, with a CRL that revokes nothing yet.
struct Authority {
    root: Ca,
    issuing: Ca,
}

impl Authority {
    fn new() -> Self {
        let root = pki::root("Test root CA");
        let issuing = pki::intermediate("Test issuing CA 1", &root);
        Self { root, issuing }
    }

    fn chain(&self) -> String {
        format!("{}{}", self.root.pem, self.issuing.pem)
    }

    /// A new CRL (ECDSA signatures differ every time): what the certs service serves, and the
    /// files the broker should get from it.
    fn publish(&self, crl_number: u64, revoked: &[u64]) -> (Answers, TrustSet) {
        let crl = pki::crl(&self.issuing, crl_number, revoked);
        let set = validate(&self.chain(), &crl).expect("valid");
        let answers = Answers {
            ca: (200, self.chain()),
            crl: (200, crl),
        };
        (answers, set)
    }
}

fn unavailable() -> Answers {
    Answers {
        ca: (503, "not ready".to_owned()),
        crl: (503, "not ready".to_owned()),
    }
}

struct Pod {
    trust_dir: PathBuf,
    tls_dir: PathBuf,
}

impl Pod {
    fn new(server_ca: &Ca) -> Self {
        let root = scratch_dir("mqtt-trust");
        let pod = Self {
            trust_dir: root.join("trust"),
            tls_dir: root.join("tls"),
        };
        std::fs::create_dir_all(&pod.tls_dir).expect("the Secret's mount");
        pki::mount_secret(&pod.tls_dir, 1, &pki::server(server_ca));
        pod
    }

    fn settings(&self, certs_url: &str) -> MqttTrustSettings {
        MqttTrustSettings {
            certs_url: certs_url.to_owned(),
            dir: self.trust_dir.clone(),
            tls_dir: self.tls_dir.clone(),
            interval: Duration::from_secs(1),
        }
    }

    fn installed(&self) -> TrustSet {
        installed(&self.trust_dir)
            .expect("readable")
            .expect("installed")
    }
}

async fn init(settings: &MqttTrustSettings) -> Result<(), migrate::Error> {
    once(settings, &mut Report::default()).await
}

#[tokio::test]
async fn the_init_container_installs_the_certs_services_files_or_the_placeholder() {
    let authority = Authority::new();
    let pod = Pod::new(&pki::root("Broker server CA"));
    let certs = FakeCerts::start(unavailable()).await;
    let settings = pod.settings(&certs.url);

    // A fresh volume and a certs service that is not ready: the placeholder, so the broker
    // starts and trusts no gateway.
    init(&settings).await.expect("a first start");
    assert!(pod.installed().placeholder);

    let (serving, first) = authority.publish(1, &[]);
    certs.answer(serving);
    init(&settings).await.expect("a start");
    assert_eq!(pod.installed(), first);

    // Not ready, an error status, an unreachable service, or files that do not load: keep the
    // last good pair.
    let garbage = Answers {
        ca: (200, authority.chain()),
        crl: (
            200,
            "-----BEGIN X509 CRL-----\nAAAA\n-----END X509 CRL-----\n".to_owned(),
        ),
    };
    let wrong_status = Answers {
        ca: (500, String::new()),
        crl: (500, String::new()),
    };
    for answers in [unavailable(), garbage, wrong_status] {
        certs.answer(answers);
        init(&settings).await.expect("a start");
        assert_eq!(pod.installed(), first);
    }
    init(&pod.settings("http://127.0.0.1:9"))
        .await
        .expect("a start without an answer");
    assert_eq!(pod.installed(), first);

    // An empty crl.pem: the certs service has no usable issuing CA, so no gateway is trusted.
    certs.answer(Answers {
        ca: (200, authority.root.pem.clone()),
        crl: (200, String::new()),
    });
    init(&settings).await.expect("a start");
    assert!(pod.installed().placeholder);

    // No certs service in the release: the placeholder too.
    let (serving, second) = authority.publish(2, &[7]);
    certs.answer(serving);
    init(&settings).await.expect("a start");
    assert_eq!(pod.installed(), second);
    init(&pod.settings(""))
        .await
        .expect("a start without certs");
    assert!(pod.installed().placeholder);
}

#[tokio::test]
async fn the_init_container_refuses_a_server_key_of_another_certificate() {
    let server_ca = pki::root("Broker server CA");
    let pod = Pod::new(&server_ca);
    let mut mismatched = pki::server(&server_ca);
    mismatched.key = pki::server(&server_ca).key;
    pki::mount_secret(&pod.tls_dir, 2, &mismatched);
    let error = init(&pod.settings("")).await.expect_err("a mismatched key");
    assert!(error.to_string().contains("does not belong"), "{error}");

    let empty = scratch_dir("mqtt-trust-no-secret");
    let error = init(&MqttTrustSettings {
        certs_url: String::new(),
        dir: empty.join("trust"),
        tls_dir: empty.join("tls"),
        interval: Duration::from_secs(1),
    })
    .await
    .expect_err("no Secret");
    assert!(error.to_string().contains("tls.crt"), "{error}");
}

/// A process that appends a line to `count_file` on every SIGHUP and otherwise idles.
struct FakeBroker {
    child: Child,
    count_file: PathBuf,
}

impl FakeBroker {
    fn start(dir: &Path) -> Self {
        let count_file = dir.join("sighups");
        let ready_file = dir.join("trapped");
        std::fs::write(&count_file, "").expect("the count file");
        let child = default_sighup(&mut Command::new("sh"))
            .args([
                "-c",
                r#"trap 'echo hup >> "$0"' HUP; : > "$1"; while :; do sleep 0.2; done"#,
            ])
            .arg(&count_file)
            .arg(&ready_file)
            .spawn()
            .expect("sh runs");
        let broker = Self { child, count_file };
        // Until the trap is set, SIGHUP would end the shell; the fake /proc already says it is
        // handled, so the sidecar may signal at once.
        let started = Instant::now();
        while !ready_file.exists() {
            assert!(
                started.elapsed() < WAIT,
                "timed out waiting for the fake broker to trap SIGHUP"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        broker
    }

    fn sighups(&self) -> usize {
        std::fs::read_to_string(&self.count_file)
            .expect("the count file")
            .lines()
            .count()
    }
}

impl Drop for FakeBroker {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// `/proc` with only this test's entries: `self`, and the fake broker as `mosquitto`, which
/// already handles SIGHUP.
fn fake_proc(broker: &FakeBroker) -> PathBuf {
    let root = scratch_dir("mqtt-trust-proc");
    std::fs::create_dir(root.join("self")).expect("self");
    let dir = root.join(broker.child.id().to_string());
    std::fs::create_dir(&dir).expect("the broker's entry");
    std::fs::write(dir.join("comm"), "mosquitto\n").expect("comm");
    std::fs::write(
        dir.join("status"),
        "Name:\tmosquitto\nSigCgt:\t0000000000004a03\n",
    )
    .expect("status");
    root
}

async fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
    let started = Instant::now();
    while !done() {
        assert!(started.elapsed() < WAIT, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test]
async fn the_sidecar_reloads_the_broker_after_every_change_it_can_load() {
    let authority = Authority::new();
    let server_ca = pki::root("Broker server CA");
    let pod = Pod::new(&server_ca);
    let (serving, _) = authority.publish(1, &[]);
    let certs = FakeCerts::start(serving).await;
    let settings = pod.settings(&certs.url);
    init(&settings).await.expect("the init container");
    let broker = FakeBroker::start(&scratch_dir("mqtt-trust-broker"));
    let proc_root = fake_proc(&broker);
    let sidecar = {
        let settings = pod.settings(&certs.url);
        let proc_root = proc_root.clone();
        tokio::spawn(async move { run(&settings, &proc_root).await })
    };

    // Once at its start, then nothing while nothing changes.
    wait_for("the reload at the sidecar's start", || {
        broker.sighups() == 1
    })
    .await;
    let requests = certs.requests();
    wait_for("two more fetches", || certs.requests() >= requests + 4).await;
    assert_eq!(broker.sighups(), 1, "no reload without a change");

    // A new CRL.
    let (serving, second) = authority.publish(2, &[42]);
    certs.answer(serving);
    wait_for("the reload after a new CRL", || broker.sighups() == 2).await;
    assert_eq!(pod.installed(), second);

    // A new server certificate in the Secret.
    pki::mount_secret(&pod.tls_dir, 2, &pki::server(&server_ca));
    wait_for("the reload after a new server certificate", || {
        broker.sighups() == 3
    })
    .await;

    // A server key that does not match: a new CRL is installed, but the broker is not reloaded,
    // because it would stop on the key. Fixed, both load in one reload.
    let mut mismatched = pki::server(&server_ca);
    mismatched.key = pki::server(&server_ca).key;
    pki::mount_secret(&pod.tls_dir, 3, &mismatched);
    let (serving, third) = authority.publish(3, &[42, 43]);
    certs.answer(serving);
    wait_for("the new CRL on disk", || {
        installed(&pod.trust_dir).ok().flatten().as_ref() == Some(&third)
    })
    .await;
    let requests = certs.requests();
    wait_for("two more fetches", || certs.requests() >= requests + 4).await;
    assert_eq!(
        broker.sighups(),
        3,
        "no reload with a key that does not load"
    );
    pki::mount_secret(&pod.tls_dir, 4, &pki::server(&server_ca));
    wait_for("the reload after the key was fixed", || {
        broker.sighups() == 4
    })
    .await;

    sidecar.abort();
    let _ = sidecar.await;
    assert!(
        pod.trust_dir.join("current").join("ca.pem").exists(),
        "the broker's files stay after the sidecar stops"
    );
}

#[tokio::test]
async fn the_sidecar_waits_for_a_broker_that_handles_sighup() {
    let pod = Pod::new(&pki::root("Broker server CA"));
    let settings = pod.settings("");
    init(&settings).await.expect("the init container");
    let broker = FakeBroker::start(&scratch_dir("mqtt-trust-broker"));
    let proc_root = fake_proc(&broker);
    let status = proc_root.join(broker.child.id().to_string()).join("status");
    // Still starting: SIGHUP would end it.
    std::fs::write(&status, "SigCgt:\t0000000000000000\n").expect("status");
    let sidecar = {
        let proc_root = proc_root.clone();
        tokio::spawn(async move { run(&pod.settings(""), &proc_root).await })
    };
    // The sidecar looks at the broker every second; give it a few looks.
    let started = Instant::now();
    wait_for("a few seconds of a starting broker", || {
        started.elapsed() >= Duration::from_secs(3)
    })
    .await;
    assert_eq!(
        broker.sighups(),
        0,
        "never signalled before it handles SIGHUP"
    );
    std::fs::write(&status, "SigCgt:\t0000000000004a03\n").expect("status");
    wait_for("the reload once it handles SIGHUP", || {
        broker.sighups() == 1
    })
    .await;
    sidecar.abort();
    let _ = sidecar.await;
}
