//! The root CRL for gateways (Laysi/trustiot-v2#831; README.md, 「端點」): `GET /certs/root-crl`
//! (`protocol::root_crl::ROOT_CRL_PATH`) answers the root CRL the service adopted, PEM, with
//! `Cache-Control: no-store`, and 404 with a fixed text while there is none; `HEAD` answers the
//! same without a body; every other method gets 405, and nothing in the request changes the
//! answer.
//!
//! The first tests serve [`certs::http::gateway_routes`] alone over a `CaState` driven by files
//! on disk, the way the ingress reaches it on the gateway host; the last runs the real service on
//! the product's schema (a throwaway Postgres with TLS, podman or docker; skipped without either
//! unless `CI` is set) and fetches it on the API port, where the ingress routes it, and not on the
//! broker port. The root CRLs are signed here with the test root, as `certs-ca root crl` does at
//! the root ceremony.

mod support;

use std::path::{Path, PathBuf};
use std::time::Duration;

use certs::authority::CaState;
use certs::config::CaFiles;
use certs::http::{NO_ROOT_CRL, gateway_routes};
use protocol::root_crl::{ROOT_CRL_CONTENT_TYPE, ROOT_CRL_PATH};
use rcgen::{CertificateRevocationListParams, Issuer, KeyIdMethod, SerialNumber};
use reqwest::{Client, Method, Response};
use support::{Service, TestCa, now, temp_dir, wait_for};
use testkit::product::product_postgres;
use time::OffsetDateTime;

const DAY: i64 = 86_400;

/// A root CRL of `ca`'s root numbered `number`, valid for 400 days from `this_update`, revoking
/// nothing: what `certs-ca root crl` signs at the first ceremony.
fn root_crl(ca: &TestCa, number: u64, this_update: i64) -> String {
    let issuer = Issuer::from_ca_cert_pem(&ca.root_pem, &ca.root_key).expect("the root as issuer");
    let at = |seconds| OffsetDateTime::from_unix_timestamp(seconds).expect("a time");
    CertificateRevocationListParams {
        this_update: at(this_update),
        next_update: at(this_update + 400 * DAY),
        crl_number: SerialNumber::from(number),
        issuing_distribution_point: None,
        revoked_certs: Vec::new(),
        key_identifier_method: KeyIdMethod::Sha256,
    }
    .signed_by(&issuer)
    .expect("a root CRL")
    .pem()
    .expect("PEM")
}

/// The DER of the one CRL in `pem`.
fn der(pem: &str) -> Vec<u8> {
    let mut blocks = certs::pem::decode_all(pem, certs::pem::CRL).expect("PEM");
    assert_eq!(blocks.len(), 1, "one CRL: {pem}");
    blocks.remove(0)
}

/// The CA files of `ca`, with an empty root CRL file beside them.
fn files(ca: &TestCa) -> (CaFiles, PathBuf) {
    let path = ca
        .root_file
        .parent()
        .expect("the CA directory")
        .join("root-crl.pem");
    std::fs::write(&path, "").expect("write an empty root CRL file");
    let files = CaFiles {
        root_crl_file: Some(path.clone()),
        ..files_of(ca)
    };
    (files, path)
}

/// Writes `contents` to `path` the way the kubelet updates a ConfigMap volume.
fn replace(path: &Path, contents: &str) {
    let next = path.with_extension("next");
    std::fs::write(&next, contents).expect("write the new file");
    std::fs::rename(&next, path).expect("replace the file");
}

/// Serves the gateways' routes over `state` on a loopback port; returns the base URL.
async fn serve(state: CaState) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind a loopback port");
    let address = listener.local_addr().expect("the bound address");
    tokio::spawn(async move {
        axum::serve(listener, gateway_routes(state))
            .await
            .expect("serve the gateway routes");
    });
    format!("http://{address}")
}

/// Sends `GET <path>` with a cookie, a forged identity header, a query, a range and
/// `Content-Length: 1048576` to `base` over a raw connection, but none of the body, and reads
/// the answer: its head and its body (by its `Content-Length`). A handler that read the body
/// would wait for it, and this would fail after [`support::LIMIT`].
async fn unsent_body(base: &str, path: &str) -> (String, String) {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let address = base.trim_start_matches("http://");
    let mut stream = tokio::net::TcpStream::connect(address)
        .await
        .expect("connect");
    let request = format!(
        "GET {path}?number=0&format=der HTTP/1.1\r\nHost: {address}\r\nCookie: trustiot_session=forged\r\nX-Internal-Identity: forged\r\nRange: bytes=0-9\r\nContent-Length: 1048576\r\n\r\n"
    );
    stream
        .write_all(request.as_bytes())
        .await
        .expect("send the request head");
    let read = async {
        let mut received = Vec::new();
        let mut chunk = [0_u8; 4096];
        loop {
            let text = String::from_utf8_lossy(&received).into_owned();
            if let Some((head, body)) = text.split_once("\r\n\r\n") {
                let length = head
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().expect("a length"))
                    })
                    .expect("a content-length");
                if body.len() >= length {
                    return (head.to_owned(), body.to_owned());
                }
            }
            let n = stream.read(&mut chunk).await.expect("read the answer");
            assert!(
                n > 0,
                "the connection closed before the answer was complete: {text}"
            );
            received.extend_from_slice(&chunk[..n]);
        }
    };
    tokio::time::timeout(support::LIMIT, read)
        .await
        .expect("the answer to a GET whose announced body never comes")
}

/// What one answer was: status, the headers the contract names, and the body.
#[derive(Debug, PartialEq, Eq)]
struct Answer {
    status: u16,
    content_type: Option<String>,
    cache_control: Option<String>,
    allow: Option<String>,
    body: String,
}

async fn answer(response: Response) -> Answer {
    let header = |name: &str| {
        response
            .headers()
            .get(name)
            .map(|value| value.to_str().expect("an ASCII header").to_owned())
    };
    let (content_type, cache_control, allow) = (
        header("content-type"),
        header("cache-control"),
        header("allow"),
    );
    Answer {
        status: response.status().as_u16(),
        content_type,
        cache_control,
        allow,
        body: response.text().await.expect("the body"),
    }
}

async fn send(client: &Client, method: Method, url: &str) -> Answer {
    answer(
        client
            .request(method.clone(), url)
            .send()
            .await
            .unwrap_or_else(|error| panic!("{method} {url}: {error}")),
    )
    .await
}

/// The answer while there is no root CRL to hand out.
fn none() -> Answer {
    Answer {
        status: 404,
        content_type: Some("text/plain; charset=utf-8".to_owned()),
        cache_control: Some("no-store".to_owned()),
        allow: None,
        body: NO_ROOT_CRL.to_owned(),
    }
}

/// The answer carrying `pem`.
fn served(pem: &str) -> Answer {
    Answer {
        status: 200,
        content_type: Some(ROOT_CRL_CONTENT_TYPE.to_owned()),
        cache_control: Some("no-store".to_owned()),
        allow: None,
        body: pem.to_owned(),
    }
}

/// No root CRL yet: 404 with the fixed text, `GET` and `HEAD` alike. Once one is adopted: exactly
/// the PEM gateways load (the adopted CRL, the same DER as the file), whatever the request
/// carries (a cookie, a forged identity header, a query string, a range), and at once for a
/// request that announces a 1 MiB body and never sends it, so the body is not read; a newer one
/// replaces it; and after the root changes, the kept one was checked against the old root, so the
/// answer is 404 again.
#[tokio::test(flavor = "multi_thread")]
async fn the_adopted_root_crl_is_served_and_404_while_there_is_none() {
    let ca = TestCa::new(&[1]);
    let (files, path) = files(&ca);
    let state = CaState::default();
    let t = now();
    assert!(state.reload(&files, t), "the authority loads");
    let base = serve(state.clone()).await;
    let url = format!("{base}{ROOT_CRL_PATH}");
    let client = Client::new();

    assert_eq!(send(&client, Method::GET, &url).await, none());
    assert_eq!(
        send(&client, Method::HEAD, &url).await,
        Answer {
            body: String::new(),
            ..none()
        }
    );
    println!("no root CRL: GET and HEAD 404 {NO_ROOT_CRL:?}, no-store");

    let first = root_crl(&ca, 1, t - DAY);
    replace(&path, &first);
    state.reload(&files, t);
    let adopted = state.root_crl().expect("the root CRL 1 adopted");
    let got = send(&client, Method::GET, &url).await;
    assert_eq!(got, served(adopted.pem()));
    assert_eq!(der(&got.body), der(&first), "the same CRL as the file");
    assert!(
        u64::try_from(got.body.len()).expect("a length") <= ::root_crl::MAX_BYTES,
        "within the agent's limit"
    );
    assert_eq!(
        send(&client, Method::HEAD, &url).await,
        Answer {
            body: String::new(),
            ..served(adopted.pem())
        }
    );
    println!(
        "root CRL 1 adopted: GET 200 {ROOT_CRL_CONTENT_TYPE}, {} bytes, the file's DER; HEAD 200 without a body",
        got.body.len()
    );

    // Nothing in the request changes the answer, and the handler reads no body: a request that
    // announces 1 MiB of body and sends none of it gets the whole answer.
    let (head, body) = unsent_body(&base, ROOT_CRL_PATH).await;
    assert!(head.starts_with("HTTP/1.1 200 "), "{head}");
    assert!(
        head.to_ascii_lowercase()
            .contains("cache-control: no-store\r\n"),
        "{head}"
    );
    assert_eq!(body, adopted.pem());
    println!(
        "a GET announcing a 1 MiB body it never sends, with a cookie, a forged identity, a query and a range: the same 200 at once"
    );

    // The next ceremony's root CRL replaces it.
    let second = root_crl(&ca, 2, t);
    replace(&path, &second);
    state.reload(&files, t);
    let got = send(&client, Method::GET, &url).await;
    assert_eq!(der(&got.body), der(&second));
    assert_eq!(got.status, 200);
    println!("root CRL 2 adopted: served instead of 1");

    // Another root: the kept root CRL was checked against the old root, so none is handed out.
    let other = TestCa::new(&[1]);
    let other_files = CaFiles {
        root_crl_file: Some(path.clone()),
        ..files_of(&other)
    };
    assert!(state.reload(&other_files, t), "the new root loads");
    assert!(state.root_crl().is_none());
    assert_eq!(send(&client, Method::GET, &url).await, none());
    println!("after a new root without a root CRL of its own: 404 again");
}

/// `ca`'s CA files without a root CRL file.
fn files_of(ca: &TestCa) -> CaFiles {
    CaFiles {
        keys_file: ca.keys_file.clone(),
        root_certificate_file: ca.root_file.clone(),
        certificates_file: ca.certificates_file.clone(),
        root_crl_file: None,
        reload_interval: Duration::from_secs(30),
    }
}

/// Every method but `GET` and `HEAD` gets 405 with `Allow: GET,HEAD` and an empty body, with and
/// without a root CRL; the path is exact, so its variants are 404 from the router, and the gateway
/// routes serve nothing else, the `/internal` files included.
#[tokio::test(flavor = "multi_thread")]
async fn other_methods_are_refused_and_only_the_exact_path_is_served() {
    let ca = TestCa::new(&[1]);
    let (files, path) = files(&ca);
    let state = CaState::default();
    let t = now();
    state.reload(&files, t);
    let base = serve(state.clone()).await;
    let url = format!("{base}{ROOT_CRL_PATH}");
    let client = Client::new();
    let methods = ["POST", "PUT", "DELETE", "PATCH", "OPTIONS", "TRACE"];

    for adopted in [false, true] {
        if adopted {
            replace(&path, &root_crl(&ca, 1, t - DAY));
            state.reload(&files, t);
            assert!(state.root_crl().is_some());
        }
        for method in methods {
            let method = Method::from_bytes(method.as_bytes()).expect("a method");
            let got = send(&client, method.clone(), &url).await;
            assert_eq!(
                (got.status, got.allow.as_deref(), got.body.as_str()),
                (405, Some("GET,HEAD"), ""),
                "{method} with a root CRL adopted: {adopted}"
            );
            assert_eq!(got.cache_control.as_deref(), Some("no-store"), "{method}");
        }
    }
    println!(
        "{} other methods: 405, Allow: GET,HEAD, no body",
        methods.len()
    );

    for other in [
        "/certs/root-crl/",
        "/certs/root-crl/x",
        "/certs/root-crl.pem",
        "/certs/ROOT-CRL",
        "/certs/root-crl%2f",
        "/certs/",
        "/certs/renew",
        "/internal/certs/v1/crl.pem",
        "/internal/certs/v1/ca.pem",
    ] {
        let got = send(&client, Method::GET, &format!("{base}{other}")).await;
        assert_eq!((got.status, got.body.as_str()), (404, ""), "{other}");
    }
    println!("path variants, the renewal and the /internal files: 404 from the gateway routes");
}

/// The real service serves the root CRL on its API port (where the chart's route sends the gateway
/// host's `/certs/root-crl`): 404 before the ConfigMap has one, then the CRL written to the file,
/// without a restart; `HEAD` 200, `POST` 405. The broker port, which only the broker's certificate
/// CronJob reaches, does not serve it.
#[tokio::test(flavor = "multi_thread")]
async fn the_service_serves_the_root_crl_on_its_api_port() {
    let Some(product) = product_postgres(&["certs_app"]).await else {
        return;
    };
    let ca = TestCa::new(&[1]);
    let path = temp_dir("certs-root-crl-endpoint").join("root-crl.pem");
    std::fs::write(&path, "").expect("write an empty root CRL file");
    let file = path.display().to_string();
    let service = Service::start(
        &product,
        &ca,
        &[
            ("TRUSTIOT__CA__ROOT_CRL_FILE", file.as_str()),
            ("TRUSTIOT__CA__RELOAD_INTERVAL", "1s"),
        ],
    )
    .await;
    let url = format!("{}{ROOT_CRL_PATH}", service.base_url);
    let client = Client::new();
    assert_eq!(send(&client, Method::GET, &url).await, none());
    println!("before a root CRL: {:?}", none());

    let t = now();
    let first = root_crl(&ca, 1, t - DAY);
    replace(&path, &first);
    let got = wait_for("the root CRL 1 served on the API port", || {
        let (client, url) = (&client, &url);
        async move {
            let got = send(client, Method::GET, url).await;
            (got.status == 200).then_some(got)
        }
    })
    .await;
    assert_eq!(der(&got.body), der(&first));
    assert_eq!(
        (got.content_type.as_deref(), got.cache_control.as_deref()),
        (Some(ROOT_CRL_CONTENT_TYPE), Some("no-store"))
    );
    let head = send(&client, Method::HEAD, &url).await;
    assert_eq!((head.status, head.body.as_str()), (200, ""));
    let post = send(&client, Method::POST, &url).await;
    assert_eq!(
        (post.status, post.allow.as_deref()),
        (405, Some("GET,HEAD"))
    );
    println!(
        "root CRL 1: GET 200 ({} bytes, the file's DER), HEAD 200, POST 405",
        got.body.len()
    );

    let (status, _) = service.get_broker(ROOT_CRL_PATH).await;
    assert_eq!(
        status, 404,
        "the broker port serves only the broker's certificate"
    );
    println!("broker port: {ROOT_CRL_PATH} 404");
    drop(service);
}
