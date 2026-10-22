//! The certs service's issuing CA keys (Laysi/trustiot-v2#78): the certs pod's init container
//! (`migrate certs-ca-keys`) against the fake Kubernetes API of support/secrets.rs.
//!
//! - A fresh install fills the empty Secret with one key and publishes its certificate signing
//!   request in the ConfigMap; a second start changes nothing and patches nothing.
//! - A higher generation adds a key and its request and keeps the older one; a lower generation
//!   (a rollback) changes nothing; `remove_before_generation` removes older keys and their
//!   requests.
//! - A run stopped halfway is finished by the next; two runs at once agree on one key.
//! - A Secret someone else created or restored is not trusted, and its foreign metadata goes.
//! - Missing objects, refused access and a rewritten write stop the run with the way out.
//!
//! No report, error or `Debug` output holds a private key, and the ConfigMap holds requests only.

#![allow(clippy::string_slice, reason = "prefixes of ASCII test data (base64)")]

mod support;

use std::collections::BTreeMap;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use ca_keys::{CaKeys, KEYS_FILE, request_file};
use migrate::ca_keys::converge_at;
use migrate::config::CaKeySettings;
use migrate::passwords::UID_ANNOTATION;
use migrate::{Error, Report};
use serde_json::Value;
use support::secrets::FakeSecrets;

const SECRET: &str = "trustiot-certs-ca-keys";
const CONFIG_MAP: &str = "trustiot-certs-ca-requests";
const T0: i64 = 1_791_590_400;

async fn fake() -> FakeSecrets {
    let fake = FakeSecrets::start().await;
    fake.declare(SECRET);
    fake.declare_config_map(CONFIG_MAP);
    fake
}

fn settings(fake: &FakeSecrets, generation: &str, remove_before: &str) -> CaKeySettings {
    let keys = fake.key_settings(SECRET, CONFIG_MAP, generation);
    CaKeySettings {
        api_url: keys.api_url,
        namespace: keys.namespace,
        token_file: keys.token_file,
        ca_file: keys.ca_file,
        secret: keys.secret,
        config_map: keys.config_map,
        generation: keys.generation,
        remove_before_generation: remove_before.to_owned(),
        wait: keys.wait,
    }
}

async fn run_at(settings: &CaKeySettings, now: i64) -> Result<Report, Error> {
    let mut report = Report::default();
    converge_at(settings, &mut report, now).await?;
    Ok(report)
}

async fn converge(settings: &CaKeySettings, now: i64) -> Report {
    run_at(settings, now).await.expect("the run succeeds")
}

fn stored_keys(fake: &FakeSecrets) -> CaKeys {
    let text = fake.value(SECRET, KEYS_FILE).expect("a key set");
    CaKeys::parse(&text).expect("a valid key set")
}

fn ids(keys: &CaKeys) -> Vec<String> {
    keys.key_ids().map(str::to_owned).collect()
}

fn generations(keys: &CaKeys) -> Vec<u64> {
    keys.keys().iter().map(ca_keys::CaKey::generation).collect()
}

/// The ConfigMap's data as text.
fn published(fake: &FakeSecrets) -> BTreeMap<String, String> {
    fake.config_map(CONFIG_MAP)
        .expect("the ConfigMap")
        .data
        .into_iter()
        .map(|(key, value)| (key, String::from_utf8(value).expect("UTF-8")))
        .collect()
}

fn patches(fake: &FakeSecrets) -> usize {
    fake.seen()
        .iter()
        .filter(|seen| seen.line.starts_with("PATCH"))
        .count()
}

/// Fails when any text holds a private key of the stored set (base64, as the document stores
/// it) or the Secret's document (base64, as the API carries it).
fn assert_no_private_key(fake: &FakeSecrets, texts: &[String]) {
    let document = fake.value(SECRET, KEYS_FILE).unwrap_or_default();
    let set: Value = serde_json::from_str(&document).unwrap_or_default();
    let mut secrets: Vec<String> = set["keys"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|key| {
            key["private_key"]
                .as_str()
                .map(|text| text[..32].to_owned())
        })
        .collect();
    if !document.is_empty() {
        secrets.push(STANDARD.encode(&document)[..64].to_owned());
    }
    assert!(!secrets.is_empty() || document.is_empty());
    for text in texts {
        for secret in &secrets {
            assert!(!text.contains(secret), "a private key reached: {text}");
        }
    }
}

fn texts(report: &Report) -> Vec<String> {
    let mut texts = report.changes.clone();
    texts.push(format!("{report:?}"));
    texts
}

#[tokio::test]
async fn a_fresh_install_gets_one_key_and_its_request() {
    let fake = fake().await;
    let report = converge(&settings(&fake, "1", "0"), T0).await;

    let keys = stored_keys(&fake);
    assert_eq!(generations(&keys), [1]);
    assert_eq!(keys.keys()[0].created_at(), T0);
    assert_eq!(published(&fake), keys.requests());
    assert!(published(&fake).contains_key(&request_file(1)));
    let secret = fake.get(SECRET).expect("the Secret");
    assert_eq!(secret.annotations.get(UID_ANNOTATION), Some(&secret.uid));
    assert_no_private_key(&fake, &texts(&report));
    // The ConfigMap holds requests, never a key.
    let config_map = format!("{:?}", published(&fake));
    assert_no_private_key(&fake, &[config_map]);
    assert!(!format!("{keys:?}").contains("private"), "{keys:?}");
    println!("fresh install: {:?}", report.changes);

    // A second start changes nothing and writes nothing.
    let before = patches(&fake);
    let again = converge(&settings(&fake, "1", "0"), T0 + 60).await;
    assert!(again.changes.is_empty(), "{:?}", again.changes);
    assert_eq!(patches(&fake), before);
    assert_eq!(ids(&stored_keys(&fake)), ids(&keys));
}

#[tokio::test]
async fn generations_add_keys_and_removal_takes_them_out() {
    let fake = fake().await;
    converge(&settings(&fake, "1", "0"), T0).await;
    let first = ids(&stored_keys(&fake));

    let report = converge(&settings(&fake, "2", "0"), T0 + 100).await;
    let keys = stored_keys(&fake);
    assert_eq!(generations(&keys), [1, 2]);
    assert_eq!(ids(&keys)[0], first[0], "the older key stays");
    assert_eq!(published(&fake), keys.requests());
    assert_no_private_key(&fake, &texts(&report));

    // A rollback of the chart's values makes no key.
    let rollback = converge(&settings(&fake, "1", "0"), T0 + 200).await;
    assert!(rollback.changes.is_empty(), "{:?}", rollback.changes);
    assert_eq!(generations(&stored_keys(&fake)), [1, 2]);

    // Retiring generation 1 (or answering its compromise): its key and its request go.
    let removed = converge(&settings(&fake, "2", "2"), T0 + 300).await;
    let keys = stored_keys(&fake);
    assert_eq!(generations(&keys), [2]);
    assert_eq!(
        published(&fake).keys().cloned().collect::<Vec<_>>(),
        [request_file(2)]
    );
    assert!(
        removed
            .changes
            .iter()
            .any(|change| change.contains("removed issuing CA key") && change.contains(&first[0])),
        "{:?}",
        removed.changes
    );

    // Removing every key at once adds the requested generation's.
    converge(&settings(&fake, "3", "3"), T0 + 400).await;
    assert_eq!(generations(&stored_keys(&fake)), [3]);
    assert_eq!(published(&fake), stored_keys(&fake).requests());
}

#[tokio::test]
async fn a_run_stopped_halfway_is_finished_by_the_next() {
    let fake = fake().await;
    converge(&settings(&fake, "1", "0"), T0).await;
    fake.delete_config_map(CONFIG_MAP);

    let message = run_at(&settings(&fake, "2", "0"), T0 + 100)
        .await
        .expect_err("no ConfigMap")
        .to_string();
    assert!(message.contains(CONFIG_MAP), "{message}");
    assert!(message.contains("helm upgrade"), "{message}");
    assert_eq!(generations(&stored_keys(&fake)), [1, 2]);

    fake.declare_config_map(CONFIG_MAP);
    converge(&settings(&fake, "2", "0"), T0 + 200).await;
    assert_eq!(published(&fake), stored_keys(&fake).requests());

    // Someone edits the ConfigMap: the next start puts back exactly the requests.
    fake.edit_config_map(CONFIG_MAP, |config_map| {
        config_map.data.insert(request_file(1), b"edited".to_vec());
        config_map.data.insert("planted".to_owned(), b"x".to_vec());
    });
    converge(&settings(&fake, "2", "0"), T0 + 300).await;
    assert_eq!(published(&fake), stored_keys(&fake).requests());
}

#[tokio::test]
async fn two_runs_at_once_agree_on_one_key() {
    let fake = fake().await;
    let one = settings(&fake, "1", "0");
    let two = settings(&fake, "1", "0");
    let (a, b) = tokio::join!(run_at(&one, T0), run_at(&two, T0));
    a.expect("the first run");
    b.expect("the second run");
    let keys = stored_keys(&fake);
    assert_eq!(generations(&keys), [1]);
    assert_eq!(published(&fake), keys.requests());

    // Another writer changes the Secret before each of the next two patches: the run reads
    // again and still converges.
    fake.interfere(2);
    converge(&settings(&fake, "2", "0"), T0 + 100).await;
    let keys = stored_keys(&fake);
    assert_eq!(generations(&keys), [1, 2]);
    assert_eq!(published(&fake), keys.requests());
}

#[tokio::test]
async fn a_secret_not_filled_by_the_init_container_is_not_trusted() {
    let fake = fake().await;
    // Someone created the Secret with a key of their choosing and metadata of their own.
    let planted = CaKeys::default()
        .plan(
            ca_keys::Request {
                generation: 9,
                remove_before: 0,
            },
            T0,
            ca_keys::CaKey::generate,
        )
        .expect("plan")
        .keys;
    let planted_id = ids(&planted).remove(0);
    let document = planted.to_json().to_string();
    fake.edit(SECRET, |secret| {
        secret
            .data
            .insert(KEYS_FILE.to_owned(), document.clone().into_bytes());
        secret.data.insert("extra".to_owned(), b"planted".to_vec());
        secret.annotations.insert(
            "replicator.example/replicate-to".to_owned(),
            "elsewhere".to_owned(),
        );
        secret.labels.insert("copy-me".to_owned(), "yes".to_owned());
        secret.finalizers.push("example.com/hold".to_owned());
    });

    let report = converge(&settings(&fake, "1", "0"), T0 + 10).await;
    let keys = stored_keys(&fake);
    assert_eq!(generations(&keys), [1]);
    assert_ne!(ids(&keys)[0], planted_id);
    // The planted key's request is never offered for signing.
    assert_eq!(published(&fake), keys.requests());
    assert!(!published(&fake).contains_key(&request_file(9)));
    let secret = fake.get(SECRET).expect("the Secret");
    assert_eq!(secret.annotations.get(UID_ANNOTATION), Some(&secret.uid));
    assert!(
        !secret
            .annotations
            .contains_key("replicator.example/replicate-to")
    );
    assert!(!secret.labels.contains_key("copy-me"));
    assert!(secret.finalizers.is_empty());
    assert!(!secret.data.contains_key("extra"));
    assert!(
        report
            .changes
            .iter()
            .any(|change| change.contains("not the object the init container filled")),
        "{:?}",
        report.changes
    );

    // Restored from a backup or recreated: the same data under another uid is not trusted.
    let before = ids(&stored_keys(&fake));
    fake.recreate(SECRET);
    converge(&settings(&fake, "1", "0"), T0 + 20).await;
    let after = ids(&stored_keys(&fake));
    assert_eq!(after.len(), 1);
    assert_ne!(after, before);
    assert_eq!(published(&fake), stored_keys(&fake).requests());

    // A document this version cannot read in the bound Secret (a newer format after a rollback,
    // a damaged edit) stops the run without writing: replacing it would discard the keys the
    // running pods sign with. Removing the key on purpose starts a new set.
    let writes = patches(&fake);
    for unreadable in [&b"{\"keys\":7}"[..], b"not base64 at all", b"\xff\xfe"] {
        fake.write(SECRET, KEYS_FILE, Some(unreadable));
        let message = run_at(&settings(&fake, "1", "0"), T0 + 30)
            .await
            .expect_err("an unreadable bound key set")
            .to_string();
        assert!(message.contains(SECRET), "{message}");
        assert!(message.contains("keep signing"), "{message}");
        assert!(
            message.contains(&format!("remove the key {KEYS_FILE}")),
            "{message}"
        );
        assert_eq!(patches(&fake), writes, "nothing written: {message}");
        assert_eq!(
            fake.get(SECRET)
                .expect("the Secret")
                .data
                .get(KEYS_FILE)
                .map(Vec::as_slice),
            Some(unreadable)
        );
        println!("unreadable bound key set: {message}");
    }
    fake.write(SECRET, KEYS_FILE, None);
    let report = converge(&settings(&fake, "1", "0"), T0 + 40).await;
    assert!(
        report
            .changes
            .iter()
            .any(|change| change.contains(&format!("held no {KEYS_FILE}"))),
        "{:?}",
        report.changes
    );
    assert_eq!(generations(&stored_keys(&fake)), [1]);
    assert_eq!(published(&fake), stored_keys(&fake).requests());
}

#[tokio::test]
async fn missing_objects_and_refused_access_stop_the_run_with_the_way_out() {
    let fake = fake().await;
    fake.delete(SECRET);
    let message = run_at(&settings(&fake, "1", "0"), T0)
        .await
        .expect_err("no Secret")
        .to_string();
    assert!(message.contains(SECRET), "{message}");
    assert!(message.contains("helm upgrade"), "{message}");

    let fake = self::fake().await;
    fake.refuse_with(Some(403));
    let message = run_at(&settings(&fake, "1", "0"), T0)
        .await
        .expect_err("refused")
        .to_string();
    assert!(message.contains("HTTP 403"), "{message}");
    assert!(
        message.contains("certs service's ServiceAccount"),
        "{message}"
    );

    // An admission webhook that rewrites what is written: the run stops instead of publishing
    // requests for keys the certs service does not have.
    let fake = self::fake().await;
    fake.rewrite_patched_values(Some(b"{\"keys\":[]}"));
    let message = run_at(&settings(&fake, "1", "0"), T0)
        .await
        .expect_err("rewritten")
        .to_string();
    assert!(message.contains("another key set"), "{message}");

    // A Secret being deleted is not filled.
    let fake = self::fake().await;
    fake.edit(SECRET, |secret| {
        secret.deletion_timestamp = Some("2026-10-21T00:00:00Z".to_owned());
    });
    let message = run_at(&settings(&fake, "1", "0"), T0)
        .await
        .expect_err("being deleted")
        .to_string();
    assert!(message.contains("being deleted"), "{message}");

    // A generation below the removal floor is refused before anything is read.
    let fake = self::fake().await;
    let message = run_at(&settings(&fake, "1", "2"), T0)
        .await
        .expect_err("removes the requested key")
        .to_string();
    assert!(message.contains("below"), "{message}");
    assert_eq!(patches(&fake), 0);
}

#[tokio::test]
async fn the_set_never_grows_past_four_keys() {
    let fake = fake().await;
    for generation in 1..=4 {
        converge(&settings(&fake, &generation.to_string(), "0"), T0).await;
    }
    assert_eq!(generations(&stored_keys(&fake)), [1, 2, 3, 4]);
    let message = run_at(&settings(&fake, "5", "0"), T0)
        .await
        .expect_err("too many")
        .to_string();
    assert!(message.contains("more than 4"), "{message}");
    converge(&settings(&fake, "5", "2"), T0).await;
    assert_eq!(generations(&stored_keys(&fake)), [2, 3, 4, 5]);
}
