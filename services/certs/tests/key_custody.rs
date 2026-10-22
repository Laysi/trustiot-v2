//! The issuing CA private keys never leave the certs service (Laysi/trustiot-v2#78 acceptance
//! 3), checked two ways:
//!
//! - in the code: only the places listed in [`ALLOWED`] may turn a private key into bytes or
//!   text, or read one from text, and each file exactly as often as listed; any other use of
//!   those calls in the service, the key crate or the migration tool fails the test until it is
//!   reviewed and listed;
//! - at run time: the real service, with its log at `trace`, OTLP export on, a key set whose
//!   newest key still waits for a certificate, a reload, and two broken key files, prints and
//!   serves nothing that contains any piece of a private key: not the PKCS#8 document (any 24
//!   characters of its base64 that cover the private scalar; the rest is the algorithm and the
//!   public key, which the CA certificates publish), not the 32-byte private scalar (hex,
//!   base64, decimal bytes).
//!
//! The run-time test starts a throwaway Postgres with TLS (podman or docker); skipped without
//! either unless `CI` is set.

mod support;

use std::path::{Path, PathBuf};

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE_NO_PAD};
use certs::http::{CHAIN_PATH, CRL_PATH};
use serde_json::Value;
use support::{FakeCollector, Service, TestCa, wait_for};
use testkit::product::product_postgres;

/// Calls that turn a private key into bytes or text, or make one from text.
const SENSITIVE: &[&str] = &[
    "serialize_pem",
    "serialize_der",
    "serialized_der",
    "secret_pkcs8_der",
    "from_pem",
    "to_json",
    "into_signing_key",
    "StoredPrivateKey",
];

/// Where each sensitive call may appear (repository paths), on how many lines of that file, and
/// why. A count that no longer matches fails the test, so a new use in a listed file is reviewed
/// like one in any other file.
const ALLOWED: &[(&str, &str, usize, &str)] = &[
    (
        "crates/ca-keys/src/lib.rs",
        "serialized_der",
        1,
        "to_json writes the key set for the Secret (tools/migrate)",
    ),
    (
        "crates/ca-keys/src/lib.rs",
        "to_json",
        1,
        "the key set's document for the Secret",
    ),
    (
        "crates/ca-keys/src/lib.rs",
        "StoredPrivateKey",
        4,
        "the document's private key field, wiped on drop, without Debug",
    ),
    (
        "crates/ca-keys/src/lib.rs",
        "into_signing_key",
        1,
        "hands the key to the issuing CA, which only signs with it",
    ),
    (
        "services/certs/src/authority.rs",
        "into_signing_key",
        1,
        "the issuing CA takes its key into rcgen's issuer, which only signs with it",
    ),
    (
        "services/certs/src/bin/certs-ca.rs",
        "serialize_pem",
        1,
        "the offline ceremony writes the new root key to stdout, into age",
    ),
    (
        "services/certs/src/bin/certs-ca.rs",
        "from_pem",
        1,
        "the offline ceremony reads the root key from stdin, from age (root sign, root check)",
    ),
    (
        "tools/migrate/src/ca_keys.rs",
        "to_json",
        2,
        "the init container writes the key set into the Secret (and compares it with what it read)",
    ),
    (
        "tools/migrate/src/signing_keys.rs",
        "to_json",
        2,
        "the identity service's signing keys (crates/identity-keys) into their own Secret; no CA key",
    ),
];

/// The directories scanned: every non-test Rust source that handles the CA keys.
const SCANNED: &[&str] = &[
    "services/certs/src",
    "crates/ca-keys/src",
    "tools/migrate/src",
];

fn repository() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("the repository root")
}

fn rust_files(dir: &Path, found: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("read a source directory") {
        let path = entry.expect("a directory entry").path();
        if path.is_dir() {
            rust_files(&path, found);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            found.push(path);
        }
    }
}

/// The lines of `text` that are not test code, with their 0-based numbers. An inline test module
/// (`#[cfg(test)]` then `mod … {` up to its closing `}` in column 0) is left out; a
/// `#[cfg(test)] mod tests;` declaration leaves out only itself, since the module lives in its
/// own `tests.rs` (skipped by the caller). Any other `#[cfg(test)]` item is scanned like code.
fn non_test_lines(text: &str) -> Vec<(usize, &str)> {
    let lines: Vec<&str> = text.lines().collect();
    let mut code = Vec::new();
    let mut index = 0;
    while let Some(line) = lines.get(index) {
        if line.trim() == "#[cfg(test)]" {
            let next = lines.get(index + 1).map_or("", |next| next.trim());
            if next.starts_with("mod ") && next.ends_with(';') {
                index += 2;
                continue;
            }
            if next.starts_with("mod ") && next.ends_with('{') {
                index += 2;
                while lines.get(index).is_some_and(|line| *line != "}") {
                    index += 1;
                }
                index += 1;
                continue;
            }
        }
        code.push((index, *line));
        index += 1;
    }
    code
}

#[test]
fn the_scan_leaves_out_test_modules_only() {
    let text = "use a;\n#[cfg(test)]\nmod tests;\nfn b() { to_json() }\n#[cfg(test)]\nfn c() {}\n\
                #[cfg(test)]\nmod more {\n    fn d() { to_json() }\n}\nfn e() {}\n";
    let kept: Vec<&str> = non_test_lines(text)
        .into_iter()
        .map(|(_, line)| line)
        .collect();
    assert_eq!(
        kept,
        [
            "use a;",
            "fn b() { to_json() }",
            "#[cfg(test)]",
            "fn c() {}",
            "fn e() {}"
        ]
    );
}

#[test]
fn only_reviewed_code_turns_a_private_key_into_bytes() {
    let root = repository();
    let mut files = Vec::new();
    for dir in SCANNED {
        rust_files(&root.join(dir), &mut files);
    }
    files.sort();
    let mut seen: Vec<(String, &str)> = Vec::new();
    let mut scanned_lines = 0;
    let mut violations = Vec::new();
    for file in &files {
        let relative = file
            .strip_prefix(&root)
            .expect("under the repository")
            .to_string_lossy()
            .replace('\\', "/");
        // Unit tests may do anything with throwaway keys.
        if relative.ends_with("/tests.rs") {
            continue;
        }
        let text = std::fs::read_to_string(file).expect("read a source file");
        for (number, line) in non_test_lines(&text) {
            scanned_lines += 1;
            let trimmed = line.trim_start();
            if trimmed.starts_with("//") {
                continue;
            }
            for token in SENSITIVE {
                if !line.contains(token) {
                    continue;
                }
                if ALLOWED
                    .iter()
                    .any(|(path, allowed, _, _)| *path == relative && allowed == token)
                {
                    seen.push((relative.clone(), token));
                } else {
                    violations.push(format!(
                        "{relative}:{}: {token}: {}",
                        number + 1,
                        line.trim()
                    ));
                }
            }
        }
    }
    assert!(
        violations.is_empty(),
        "unreviewed private key handling (review it, then list it in ALLOWED with the \
         reason):\n{}",
        violations.join("\n")
    );
    // Every listed use exists exactly as often as listed, so the list stays exact.
    for (path, token, count, reason) in ALLOWED {
        let found = seen
            .iter()
            .filter(|(file, seen)| file == path && seen == token)
            .count();
        assert_eq!(
            found, *count,
            "{path} uses {token} on {found} lines, ALLOWED says {count} ({reason}); review the \
             change, then correct the count"
        );
        println!("allowed: {path}: {token} x{count}: {reason}");
    }
    println!(
        "scanned {} files, {scanned_lines} lines of non-test code",
        files.len()
    );
}

/// The private key material of a key set document, as needles to search for: text, and raw
/// bytes (each private scalar, and 24-byte pieces of each PKCS#8 document).
///
/// Only pieces of a PKCS#8 document that cover a byte of its private scalar count: the document
/// also carries the algorithm and the public key, which every issuing CA certificate publishes
/// (`ca.pem`), so a piece of those is public data, not a leak.
struct Needles(Vec<String>, Vec<Vec<u8>>);

/// Whether the bytes `start..start + len` of a document overlap its private scalar
/// `scalar..scalar + 32`.
fn covers_scalar(start: usize, len: usize, scalar: usize) -> bool {
    start < scalar + 32 && scalar < start + len
}

impl Needles {
    fn of(keys_json: &str) -> Self {
        let document: Value = serde_json::from_str(keys_json).expect("the key set");
        let mut needles = Vec::new();
        let mut raw = Vec::new();
        for key in document["keys"].as_array().expect("keys") {
            let encoded = key["private_key"].as_str().expect("a private key");
            let der = STANDARD.decode(encoded).expect("base64");
            // The private scalar: ECPrivateKey { version 1, privateKey OCTET STRING (32) }.
            let at = der
                .windows(5)
                .position(|window| window == [0x02, 0x01, 0x01, 0x04, 0x20])
                .expect("an ECPrivateKey")
                + 5;
            let scalar = &der[at..at + 32];
            // Any 24 characters of the PKCS#8 base64 that cover the scalar, as in PEM lines too:
            // the window at character 4k is bytes 3k..3k + 18 of the document.
            let chars: Vec<char> = encoded.chars().collect();
            for (index, window) in chars.windows(24).enumerate().step_by(4) {
                if covers_scalar(index / 4 * 3, 18, at) {
                    needles.push(window.iter().collect());
                }
            }
            raw.push(scalar.to_vec());
            for (start, piece) in der.windows(24).enumerate().step_by(8) {
                if covers_scalar(start, 24, at) {
                    raw.push(piece.to_vec());
                }
            }
            let hex: String = scalar.iter().map(|byte| format!("{byte:02x}")).collect();
            needles.push(hex.clone());
            needles.push(hex.to_uppercase());
            needles.push(STANDARD.encode(scalar));
            needles.push(STANDARD_NO_PAD.encode(scalar));
            needles.push(URL_SAFE_NO_PAD.encode(scalar));
            needles.push(
                scalar[..8]
                    .iter()
                    .map(u8::to_string)
                    .collect::<Vec<_>>()
                    .join(", "),
            );
            needles.push(
                scalar[..8]
                    .iter()
                    .map(u8::to_string)
                    .collect::<Vec<_>>()
                    .join(","),
            );
        }
        Self(needles, raw)
    }

    /// The needles `text` contains.
    fn found_in(&self, text: &str) -> Vec<&str> {
        self.0
            .iter()
            .filter(|needle| text.contains(needle.as_str()))
            .map(String::as_str)
            .collect()
    }

    /// The needles `bytes` contains, as text or raw.
    fn found_in_bytes(&self, bytes: &[u8]) -> Vec<String> {
        let text = String::from_utf8_lossy(bytes);
        let mut found: Vec<String> = self
            .found_in(&text)
            .into_iter()
            .map(str::to_owned)
            .collect();
        for needle in &self.1 {
            if bytes
                .windows(needle.len())
                .any(|window| window == needle.as_slice())
            {
                found.push(format!("{} raw bytes", needle.len()));
            }
        }
        found
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn no_private_key_leaves_through_logs_endpoints_or_telemetry() {
    let Some(product) = product_postgres(&["certs_app"]).await else {
        return;
    };
    // Generations 1 and 2 have certificates; generation 3 waits for its ceremony.
    let mut ca = TestCa::new(&[1, 2, 3]);
    ca.issuing_pems.truncate(2);
    ca.write_all();
    let needles = Needles::of(&ca.keys_json);
    // Positive control: the needles are found where the keys are.
    assert!(needles.found_in(&ca.keys_json).len() >= needles.0.len() / 2);
    let raw_der = STANDARD
        .decode(
            serde_json::from_str::<Value>(&ca.keys_json).expect("json")["keys"][0]["private_key"]
                .as_str()
                .expect("a key"),
        )
        .expect("base64");
    assert!(!needles.found_in_bytes(&raw_der).is_empty());

    let collector = FakeCollector::start();
    let endpoint = collector.url();
    let service = Service::start(
        &product,
        &ca,
        &[
            ("TRUSTIOT__TELEMETRY__LOG_FILTER", "trace"),
            ("TRUSTIOT__TELEMETRY__OTLP_ENDPOINT", endpoint.as_str()),
            ("OTEL_METRIC_EXPORT_INTERVAL", "300"),
            ("TRUSTIOT__CA__RELOAD_INTERVAL", "1s"),
            ("TRUSTIOT__CRL__CHECK_INTERVAL", "1s"),
            ("TRUSTIOT__CRL__VALIDITY", "20s"),
            ("TRUSTIOT__CRL__RENEW_BEFORE", "12s"),
            ("TRUSTIOT__CRL__ALERT_BEFORE", "4s"),
        ],
    )
    .await;
    let mut bodies = Vec::new();
    let crls = wait_for("the published CRLs", || async {
        let (status, body) = service.get(CRL_PATH).await;
        (status == 200).then_some(body)
    })
    .await;
    bodies.push(crls);
    for path in [
        CHAIN_PATH,
        "/",
        "/internal/certs/v1/keys",
        "/internal/certs/v1/issuing-ca-keys.json",
        "/internal/certs/v1/crl.pem/../../../../keys",
    ] {
        bodies.push(service.get(path).await.1);
    }
    for path in ["/healthz", "/readyz", "/livez", "/metrics"] {
        bodies.push(service.get_health(path).await.1);
    }

    // A reload of the same keys, then two broken key files: a request swapped between two keys
    // (the keys parse, the requests do not match them) and a truncated key.
    let original = ca.keys_json.clone();
    std::fs::write(&ca.keys_file, format!("{original}\n")).expect("rewrite the keys file");
    wait_for("the reload of the rewritten keys file", || async {
        (service.stdout.messages("loaded the issuing CA").len() >= 2).then_some(())
    })
    .await;
    let mut swapped: Value = serde_json::from_str(&original).expect("json");
    let first_request = swapped["keys"][0]["request"].clone();
    swapped["keys"][0]["request"] = swapped["keys"][1]["request"].clone();
    swapped["keys"][1]["request"] = first_request;
    let mut truncated: Value = serde_json::from_str(&original).expect("json");
    let mut key = truncated["keys"][0]["private_key"]
        .as_str()
        .expect("a key")
        .to_owned();
    key.truncate(key.len() - 8);
    truncated["keys"][0]["private_key"] = Value::String(key);
    for (index, broken) in [swapped, truncated].into_iter().enumerate() {
        let before = service.stdout.messages("cannot load the issuing CA").len();
        std::fs::write(&ca.keys_file, broken.to_string()).expect("write a broken keys file");
        let refused = wait_for("the broken keys file refused", || async {
            let lines = service.stdout.messages("cannot load the issuing CA");
            (lines.len() > before).then(|| lines[before].clone())
        })
        .await;
        println!("broken keys file {}: {refused}", index + 1);
    }
    // The service keeps signing with what it had.
    std::fs::write(&ca.keys_file, &original).expect("restore the keys file");
    let (status, _) = service.get(CRL_PATH).await;
    assert_eq!(status, 200);

    // Let a few exports reach the collector, then stop the service and look at everything.
    wait_for("logs and metrics over OTLP", || async {
        (!collector.bodies("/v1/logs").is_empty() && collector.bodies("/v1/metrics").len() >= 2)
            .then_some(())
    })
    .await;
    let output = service.output();
    drop(service);
    let lines = output.lines().count();
    assert!(lines > 20, "the service logged {lines} lines");

    let mut leaks = Vec::new();
    for found in needles.found_in_bytes(output.as_bytes()) {
        leaks.push(format!("log: {found}"));
    }
    for (index, body) in bodies.iter().enumerate() {
        for found in needles.found_in_bytes(body.as_bytes()) {
            leaks.push(format!("response {index}: {found}"));
        }
    }
    let mut exported = 0;
    for path in ["/v1/logs", "/v1/traces", "/v1/metrics"] {
        for body in collector.bodies(path) {
            exported += 1;
            for found in needles.found_in_bytes(&body) {
                leaks.push(format!("OTLP {path}: {found}"));
            }
        }
    }
    assert!(
        leaks.is_empty(),
        "private key material left the service: {leaks:?}"
    );
    println!(
        "searched {} needles in {lines} log lines, {} responses and {exported} OTLP exports: \
         none found",
        needles.0.len(),
        bodies.len()
    );
}
