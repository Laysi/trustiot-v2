//! The MQTT broker's trust init container and sidecar (`migrate mqtt-broker-trust`,
//! Laysi/trustiot-v2#84, Laysi/trustiot-v2#535) against a fake certs service, a Secret mounted
//! the way the kubelet mounts it, and a fake `/proc` whose broker is a real process that counts
//! its SIGHUPs and SIGTERMs and whose `net/tcp` says whether it listens on the gateway port:
//!
//! - the init container installs what the certs service serves; with no answer and nothing
//!   installed, or without a certs service, or when the certs service has no usable issuing CA,
//!   it installs the placeholder that trusts no gateway; with no answer or unusable files it
//!   keeps the last good pair; it enables the gateway listener only with a server certificate
//!   and key that belong together, and otherwise lets the broker start without it;
//! - the sidecar has the broker reload once at its start and after every change of the trust
//!   files or the server certificate, and never while the server certificate would not load;
//! - once the Secret is filled, the sidecar enables the gateway listener and stops the broker
//!   once (SIGTERM) so that it starts again with it, never sends SIGHUP to a broker that does not
//!   listen on the gateway port, and never stops a broker a second time.
//!
//! That Mosquitto loads these files, applies a reload and starts without the gateway listener is
//! tested in mqtt_broker.rs.

mod support;

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use migrate::Report;
use migrate::config::MqttTrustSettings;
use migrate::mqtt_trust::{
    CA_PATH, CRL_PATH, GATEWAY_LISTENER_FILE, TrustSet, installed, once, run, validate,
};
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

/// The gateway listener's configuration as the chart's ConfigMap carries it, shortened.
const GATEWAY_LISTENER: &str = "# Gateways\nlistener 8883\nprotocol mqtt\n";

struct Pod {
    trust_dir: PathBuf,
    tls_dir: PathBuf,
    listeners_dir: PathBuf,
    gateway_listener: PathBuf,
}

impl Pod {
    /// A pod whose Secret holds a server certificate of `server_ca` and its key.
    fn new(server_ca: &Ca) -> Self {
        let pod = Self::empty();
        pki::mount_secret(&pod.tls_dir, 1, &pki::server(server_ca));
        pod
    }

    /// A pod whose Secret is still empty, as the chart declares it.
    fn empty() -> Self {
        let root = scratch_dir("mqtt-trust");
        let pod = Self {
            trust_dir: root.join("trust"),
            tls_dir: root.join("tls"),
            listeners_dir: root.join("listeners"),
            gateway_listener: root.join("config").join("gateway-listener.conf"),
        };
        std::fs::create_dir_all(&pod.tls_dir).expect("the Secret's mount");
        std::fs::create_dir_all(&pod.listeners_dir).expect("the listeners' emptyDir");
        std::fs::create_dir_all(root.join("config")).expect("the ConfigMap's mount");
        std::fs::write(&pod.gateway_listener, GATEWAY_LISTENER).expect("the listener's file");
        pod
    }

    fn settings(&self, certs_url: &str) -> MqttTrustSettings {
        MqttTrustSettings {
            certs_url: certs_url.to_owned(),
            dir: self.trust_dir.clone(),
            tls_dir: self.tls_dir.clone(),
            listeners_dir: self.listeners_dir.clone(),
            gateway_listener: self.gateway_listener.clone(),
            interval: Duration::from_secs(1),
        }
    }

    fn installed(&self) -> TrustSet {
        installed(&self.trust_dir)
            .expect("readable")
            .expect("installed")
    }

    /// The gateway listener the broker would include, if any.
    fn listener(&self) -> Option<String> {
        std::fs::read_to_string(self.listeners_dir.join(GATEWAY_LISTENER_FILE)).ok()
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
async fn the_init_container_enables_the_gateway_listener_only_with_a_usable_pair() {
    let server_ca = pki::root("Broker server CA");

    // A certificate and its key: the broker starts with the gateway listener.
    let pod = Pod::new(&server_ca);
    init(&pod.settings("")).await.expect("a start");
    assert_eq!(pod.listener().as_deref(), Some(GATEWAY_LISTENER));

    // The Secret as the chart declares it, before the first certificate, or a key of another
    // certificate: the broker starts, without the gateway listener, and still trusts the files.
    let empty = Pod::empty();
    init(&empty.settings(""))
        .await
        .expect("a start before the first certificate");
    assert_eq!(empty.listener(), None);
    assert!(empty.installed().placeholder);
    let mismatched_pod = Pod::empty();
    let mut mismatched = pki::server(&server_ca);
    mismatched.key = pki::server(&server_ca).key;
    pki::mount_secret(&mismatched_pod.tls_dir, 1, &mismatched);
    init(&mismatched_pod.settings(""))
        .await
        .expect("a start with a key of another certificate");
    assert_eq!(mismatched_pod.listener(), None);

    // A gateway listener's configuration that is missing or does not name one port: an error,
    // since the chart mounts it.
    let broken = Pod::new(&server_ca);
    std::fs::remove_file(&broken.gateway_listener).expect("remove the listener's file");
    let error = init(&broken.settings(""))
        .await
        .expect_err("no listener file");
    assert!(error.to_string().contains("gateway listener"), "{error}");
    std::fs::write(&broken.gateway_listener, "listener 8883\nlistener 8884\n")
        .expect("two listeners");
    let error = init(&broken.settings("")).await.expect_err("two listeners");
    assert!(
        error.to_string().contains("more than one listener"),
        "{error}"
    );
    assert_eq!(broken.listener(), None);
}

/// A process that appends `hup` to `count_file` on every SIGHUP and `term` on every SIGTERM, and
/// otherwise idles.
struct FakeBroker {
    child: Child,
    count_file: PathBuf,
}

impl FakeBroker {
    fn start(dir: &Path) -> Self {
        let count_file = dir.join("signals");
        let ready_file = dir.join("trapped");
        std::fs::write(&count_file, "").expect("the count file");
        let child = default_sighup(&mut Command::new("sh"))
            .args([
                "-c",
                r#"trap 'echo hup >> "$0"' HUP; trap 'echo term >> "$0"' TERM; : > "$1"; while :; do sleep 0.2; done"#,
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

    fn signals(&self, name: &str) -> usize {
        std::fs::read_to_string(&self.count_file)
            .expect("the count file")
            .lines()
            .filter(|line| *line == name)
            .count()
    }

    fn sighups(&self) -> usize {
        self.signals("hup")
    }

    fn sigterms(&self) -> usize {
        self.signals("term")
    }
}

impl Drop for FakeBroker {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// `/proc` with only this test's entries: `self`, the fake broker as `mosquitto`, which already
/// handles SIGHUP, and `net/tcp` with the broker's service port and, when `gateway` is set, the
/// gateway port listening.
fn fake_proc(broker: &FakeBroker, gateway: bool) -> PathBuf {
    let root = scratch_dir("mqtt-trust-proc");
    std::fs::create_dir(root.join("self")).expect("self");
    std::fs::create_dir(root.join("net")).expect("net");
    add_broker(&root, broker);
    set_listening(&root, gateway);
    root
}

/// The broker's entry under the fake `/proc`.
fn add_broker(proc_root: &Path, broker: &FakeBroker) {
    let dir = proc_root.join(broker.child.id().to_string());
    std::fs::create_dir(&dir).expect("the broker's entry");
    std::fs::write(dir.join("comm"), "mosquitto\n").expect("comm");
    std::fs::write(
        dir.join("status"),
        "Name:\tmosquitto\nSigCgt:\t0000000000004a03\n",
    )
    .expect("status");
}

/// The broker stopped: its entry goes.
fn remove_broker(proc_root: &Path, broker: &FakeBroker) {
    std::fs::remove_dir_all(proc_root.join(broker.child.id().to_string()))
        .expect("remove the broker's entry");
}

/// `net/tcp` as the kernel writes it: 1883 listens, 8883 has an established connection, and 8883
/// listens too when `gateway` is set (in `net/tcp6`, as Mosquitto binds both).
fn set_listening(proc_root: &Path, gateway: bool) {
    let header = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n";
    let tcp = format!(
        "{header}   0: 00000000:075B 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1883        0 1 1 0000000000000000 100 0 0 10 0\n   1: 0100007F:22B3 0100007F:9C40 01 00000000:00000000 00:00000000 00000000  1883        0 2 1 0000000000000000 20 4 30 10 -1\n"
    );
    let tcp6 = if gateway {
        format!(
            "{header}   0: 00000000000000000000000000000000:22B3 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1883        0 3 1 0000000000000000 100 0 0 10 0\n"
        )
    } else {
        header.to_owned()
    };
    std::fs::write(proc_root.join("net").join("tcp"), tcp).expect("net/tcp");
    std::fs::write(proc_root.join("net").join("tcp6"), tcp6).expect("net/tcp6");
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
    let proc_root = fake_proc(&broker, true);
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
    let proc_root = fake_proc(&broker, true);
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

#[tokio::test]
async fn the_sidecar_opens_the_gateway_port_once_the_secret_is_filled() {
    let authority = Authority::new();
    let server_ca = pki::root("Broker server CA");
    let pod = Pod::empty();
    let (serving, _) = authority.publish(1, &[]);
    let certs = FakeCerts::start(serving).await;
    let settings = pod.settings(&certs.url);
    init(&settings).await.expect("the init container");
    assert_eq!(
        pod.listener(),
        None,
        "no gateway listener before the first certificate"
    );

    // The broker started without the gateway listener.
    let first = FakeBroker::start(&scratch_dir("mqtt-trust-broker"));
    let proc_root = fake_proc(&first, false);
    let sidecar = {
        let settings = pod.settings(&certs.url);
        let proc_root = proc_root.clone();
        tokio::spawn(async move { run(&settings, &proc_root).await })
    };
    let requests = certs.requests();
    wait_for("two fetches", || certs.requests() >= requests + 4).await;
    assert_eq!(pod.listener(), None);
    assert_eq!(
        (first.sighups(), first.sigterms()),
        (0, 0),
        "a broker without the gateway listener is left alone while the Secret is empty"
    );

    // The broker certificate CronJob filled the Secret: the listener is enabled, and the broker,
    // which cannot add a listener on SIGHUP, is asked to stop once.
    pki::mount_secret(&pod.tls_dir, 1, &pki::server(&server_ca));
    wait_for("the gateway listener", || pod.listener().is_some()).await;
    assert_eq!(pod.listener().as_deref(), Some(GATEWAY_LISTENER));
    wait_for("the broker asked to stop", || first.sigterms() == 1).await;
    let requests = certs.requests();
    wait_for("two more fetches", || certs.requests() >= requests + 4).await;
    assert_eq!(
        (first.sighups(), first.sigterms()),
        (0, 1),
        "stopped once and never reloaded while it shuts down"
    );

    // The kubelet started it again, and it does not listen on the gateway port (yet, or because
    // the listener failed): reported, never stopped a second time, never reloaded.
    remove_broker(&proc_root, &first);
    let second = FakeBroker::start(&scratch_dir("mqtt-trust-broker"));
    add_broker(&proc_root, &second);
    let requests = certs.requests();
    wait_for("two more fetches", || certs.requests() >= requests + 4).await;
    assert_eq!(
        (second.sighups(), second.sigterms()),
        (0, 0),
        "a broker restarted for the listener is never stopped again"
    );

    // It listens: the usual reload, once.
    set_listening(&proc_root, true);
    wait_for("the reload of the restarted broker", || {
        second.sighups() == 1
    })
    .await;
    let requests = certs.requests();
    wait_for("two more fetches", || certs.requests() >= requests + 4).await;
    assert_eq!((second.sighups(), second.sigterms()), (1, 0));
    assert_eq!(first.sigterms(), 1);

    sidecar.abort();
    let _ = sidecar.await;
}
