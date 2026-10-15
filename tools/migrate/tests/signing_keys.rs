//! The internal identity signing keys (Laysi/trustiot-v2#404): the identity pod's init container
//! (`migrate identity-signing-keys`) against the fake Kubernetes API of support/secrets.rs, with
//! a fake clock.
//!
//! - A fresh install fills the empty Secret with one key that signs at once and publishes its
//!   public key in the ConfigMap; a second start changes nothing and patches nothing.
//! - A higher generation adds a key whose public key is published before it signs, and which
//!   signs only `propagation_seconds` later; the older key goes once its tokens have expired; a
//!   lower generation (a rollback) changes nothing.
//! - Revocation removes older generations at once and the new key signs at once.
//! - A run stopped halfway is finished by the next; two runs at once agree; a run that read an
//!   older Secret never takes a newer key out of the ConfigMap.
//! - A Secret someone else created or restored is not trusted, and its foreign metadata goes.
//! - A person's key (before Laysi/trustiot-v2#404) is imported under its key id.
//! - Missing objects and refused access stop the run with the way out.
//!
//! No report, error or `Debug` output holds a private key.

mod support;

use std::path::PathBuf;

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use ed25519_dalek::pkcs8::EncodePrivateKey as _;
use ed25519_dalek::pkcs8::spki::der::pem::LineEnding;
use identity_keys::{JWKS_FILE, SIGNING_KEYS_FILE, SigningKeys, TOKEN_EXPIRY_SECS};
use migrate::config::KeySettings;
use migrate::passwords::UID_ANNOTATION;
use migrate::signing_keys::converge_at;
use migrate::{Error, Report};
use serde_json::{Value, json};
use support::scratch_dir;
use support::secrets::FakeSecrets;

const SECRET: &str = "trustiot-identity-signing-keys";
const CONFIG_MAP: &str = "trustiot-internal-identity-keys";
const T0: i64 = 1_791_590_400;
const PROPAGATION: i64 = 600;

async fn fake() -> FakeSecrets {
    let fake = FakeSecrets::start().await;
    fake.declare(SECRET);
    fake.declare_config_map(CONFIG_MAP);
    fake
}

fn settings(fake: &FakeSecrets, generation: &str) -> KeySettings {
    fake.key_settings(SECRET, CONFIG_MAP, generation)
}

async fn run_at(settings: &KeySettings, now: i64) -> Result<Report, Error> {
    let mut report = Report::default();
    converge_at(settings, &mut report, || now).await?;
    Ok(report)
}

async fn converge(settings: &KeySettings, now: i64) -> Report {
    run_at(settings, now).await.expect("the run succeeds")
}

/// The key set the Secret holds.
fn stored_keys(fake: &FakeSecrets) -> SigningKeys {
    let text = fake.value(SECRET, SIGNING_KEYS_FILE).expect("a key set");
    SigningKeys::parse(&text).expect("a valid key set")
}

/// The key ids the ConfigMap publishes.
fn published(fake: &FakeSecrets) -> Vec<String> {
    let text = fake
        .config_map_value(CONFIG_MAP, JWKS_FILE)
        .expect("a JWK Set");
    let set: Value = serde_json::from_str(&text).expect("JSON");
    set["keys"]
        .as_array()
        .expect("keys")
        .iter()
        .map(|key| {
            assert!(key.get("d").is_none(), "a private member was published");
            key["kid"].as_str().expect("kid").to_owned()
        })
        .collect()
}

fn kids(keys: &SigningKeys) -> Vec<String> {
    keys.key_ids().map(str::to_owned).collect()
}

/// The key that signs at `now` with every published key accepted.
fn signer(keys: &SigningKeys, now: i64) -> Option<String> {
    keys.signing_key(now, 0, |_| true)
        .map(|key| key.kid().to_owned())
}

fn patches(fake: &FakeSecrets) -> usize {
    fake.seen()
        .iter()
        .filter(|seen| seen.line.starts_with("PATCH"))
        .count()
}

/// Fails when any text holds a private key of `keys` (base64url, as the document stores it) or
/// the Secret's document (base64, as the API carries it).
fn assert_no_private_key(fake: &FakeSecrets, texts: &[String]) {
    let document = fake.value(SECRET, SIGNING_KEYS_FILE).unwrap_or_default();
    let set: Value = serde_json::from_str(&document).unwrap_or(json!({"keys": []}));
    let mut secrets: Vec<String> = set["keys"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|key| key["d"].as_str().map(str::to_owned))
        .collect();
    if !document.is_empty() {
        secrets.push(STANDARD.encode(&document));
    }
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
async fn a_fresh_install_gets_one_key_that_signs_at_once() {
    let fake = fake().await;
    let settings = settings(&fake, "1");
    let report = converge(&settings, T0).await;

    let keys = stored_keys(&fake);
    assert_eq!(keys.keys().len(), 1);
    let key = &keys.keys()[0];
    assert_eq!(key.generation(), 1);
    assert_eq!((key.published_at(), key.signs_from()), (Some(T0), Some(T0)));
    assert_eq!(published(&fake), kids(&keys));
    assert_eq!(signer(&keys, T0).as_deref(), Some(key.kid()));
    let secret = fake.get(SECRET).expect("the Secret");
    assert_eq!(secret.annotations.get(UID_ANNOTATION), Some(&secret.uid));
    assert_eq!(
        report.changes,
        [
            format!(
                "generated signing key {} (generation 1) in Secret {SECRET}",
                key.kid()
            ),
            format!(
                "published the public keys {} in ConfigMap {CONFIG_MAP}",
                key.kid()
            ),
            format!(
                "published signing key {}; it signs from {T0} (Unix time) in Secret {SECRET}",
                key.kid()
            ),
        ]
    );
    assert_no_private_key(&fake, &texts(&report));

    // Every request: authorized, merge patches with the field manager, only these two objects.
    for seen in fake.seen() {
        assert!(seen.authorized, "{}", seen.line);
        assert!(
            seen.line.contains(&format!("/secrets/{SECRET}"))
                || seen.line.contains(&format!("/configmaps/{CONFIG_MAP}")),
            "{}",
            seen.line
        );
        if seen.line.starts_with("PATCH") {
            assert!(
                seen.line.contains("?fieldManager=trustiot-migrate"),
                "{}",
                seen.line
            );
            assert_eq!(seen.content_type, "application/merge-patch+json");
        }
    }

    // A second start, an hour later: nothing to do, nothing written.
    let before = patches(&fake);
    let report = converge(&settings, T0 + 3600).await;
    assert!(report.changes.is_empty(), "{:?}", report.changes);
    assert_eq!(patches(&fake), before);
    assert!(stored_keys(&fake).same_as(&keys));
}

/// Acceptance (Laysi/trustiot-v2#404): a rotation publishes first, signs after the propagation
/// time, and retires the old key only after its tokens have expired; a rollback does nothing.
#[tokio::test]
async fn a_rotation_publishes_first_and_retires_later() {
    let fake = fake().await;
    converge(&settings(&fake, "1"), T0).await;
    let old = kids(&stored_keys(&fake)).remove(0);

    let bump = T0 + 86_400;
    converge(&settings(&fake, "2"), bump).await;
    let keys = stored_keys(&fake);
    assert_eq!(keys.keys().len(), 2);
    let new = keys.keys()[1].kid().to_owned();
    assert_eq!(keys.keys()[1].published_at(), Some(bump));
    assert_eq!(keys.keys()[1].signs_from(), Some(bump + PROPAGATION));
    assert_eq!(published(&fake), [old.clone(), new.clone()]);
    assert_eq!(signer(&keys, bump + PROPAGATION - 1), Some(old.clone()));
    assert_eq!(signer(&keys, bump + PROPAGATION), Some(new.clone()));

    // A rollback renders generation 1 again: nothing changes.
    let before = patches(&fake);
    let report = converge(&settings(&fake, "1"), bump + 10).await;
    assert!(report.changes.is_empty(), "{:?}", report.changes);
    assert_eq!(patches(&fake), before);

    // The old key stays until its tokens have expired, with a propagation time of margin.
    let retire_at = bump + 2 * PROPAGATION + TOKEN_EXPIRY_SECS;
    converge(&settings(&fake, "2"), retire_at - 1).await;
    assert_eq!(published(&fake), [old.clone(), new.clone()]);
    let report = converge(&settings(&fake, "2"), retire_at).await;
    assert_eq!(kids(&stored_keys(&fake)), std::slice::from_ref(&new));
    assert_eq!(published(&fake), std::slice::from_ref(&new));
    assert!(
        report
            .changes
            .iter()
            .any(|change| change.starts_with(&format!("retired signing key {old}"))),
        "{:?}",
        report.changes
    );
}

#[tokio::test]
async fn revocation_replaces_every_older_key_at_once() {
    let fake = fake().await;
    converge(&settings(&fake, "1"), T0).await;
    converge(&settings(&fake, "2"), T0 + 100).await;
    let revoked = kids(&stored_keys(&fake));

    let mut revoking = settings(&fake, "3");
    revoking.revoke_before_generation = "3".to_owned();
    let report = converge(&revoking, T0 + 200).await;
    let keys = stored_keys(&fake);
    assert_eq!(keys.keys().len(), 1);
    let new = keys.keys()[0].kid().to_owned();
    assert!(!revoked.contains(&new));
    assert_eq!(keys.keys()[0].signs_from(), Some(T0 + 200));
    assert_eq!(published(&fake), [new]);
    for kid in &revoked {
        assert!(
            report
                .changes
                .iter()
                .any(|change| change.starts_with(&format!("revoked signing key {kid}"))),
            "{:?}",
            report.changes
        );
    }
}

/// A run that stopped after writing the new key into the Secret (here: the ConfigMap was gone)
/// leaves a key nobody signs with; the next run publishes it.
#[tokio::test]
async fn a_run_stopped_halfway_is_finished_by_the_next() {
    let fake = fake().await;
    converge(&settings(&fake, "1"), T0).await;
    fake.delete_config_map(CONFIG_MAP);

    let error = run_at(&settings(&fake, "2"), T0 + 100)
        .await
        .expect_err("no ConfigMap");
    let message = error.to_string();
    assert!(message.contains(CONFIG_MAP), "{message}");
    assert!(message.contains("helm upgrade"), "{message}");
    let keys = stored_keys(&fake);
    assert_eq!(keys.keys().len(), 2);
    assert!(keys.has_unpublished());
    assert_eq!(
        signer(&keys, i64::MAX).as_deref(),
        Some(keys.keys()[0].kid()),
        "the unpublished key never signs"
    );

    // helm upgrade declares the ConfigMap again; the next start publishes and schedules the key.
    fake.declare_config_map(CONFIG_MAP);
    let later = T0 + 500;
    converge(&settings(&fake, "2"), later).await;
    let keys = stored_keys(&fake);
    assert!(!keys.has_unpublished());
    assert_eq!(keys.keys()[1].signs_from(), Some(later + PROPAGATION));
    assert_eq!(published(&fake), kids(&keys));
}

#[tokio::test]
async fn two_runs_at_once_agree_on_one_key() {
    let fake = fake().await;
    let one = settings(&fake, "1");
    let two = settings(&fake, "1");
    let (first, second) = tokio::join!(run_at(&one, T0), run_at(&two, T0));
    first.expect("the first run");
    second.expect("the second run");
    let keys = stored_keys(&fake);
    assert_eq!(keys.keys().len(), 1);
    assert!(!keys.has_unpublished());
    assert_eq!(published(&fake), kids(&keys));
}

/// Run A reads the Secret with one key; before it reads the ConfigMap, run B (a newer
/// generation) puts a second key into both objects. A finds the ConfigMap unlike its own view,
/// sees that the Secret has changed, and starts over instead of writing its older view over B's.
#[tokio::test]
async fn an_older_view_never_unpublishes_a_newer_key() {
    let fake = fake().await;
    converge(&settings(&fake, "1"), T0).await;
    let (secret_one, config_map_one) = (fake.get(SECRET), fake.config_map(CONFIG_MAP));
    // What B leaves behind.
    converge(&settings(&fake, "2"), T0 + 50).await;
    let (secret_two, config_map_two) = (fake.get(SECRET), fake.config_map(CONFIG_MAP));
    let newer = kids(&stored_keys(&fake));
    assert_eq!(newer.len(), 2);
    // Back to before B, then B acts between A's read of the Secret and its read of the
    // ConfigMap.
    fake.edit(SECRET, |secret| *secret = secret_one.expect("the Secret"));
    fake.edit_config_map(CONFIG_MAP, |config_map| {
        *config_map = config_map_one.expect("the ConfigMap");
    });
    fake.before_next(
        &format!(
            "GET /api/v1/namespaces/{}/configmaps/",
            support::secrets::NAMESPACE
        ),
        move |secrets, config_maps| {
            secrets.insert(SECRET.to_owned(), secret_two.expect("the Secret"));
            config_maps.insert(
                CONFIG_MAP.to_owned(),
                config_map_two.expect("the ConfigMap"),
            );
        },
    );
    let before = patches(&fake);
    converge(&settings(&fake, "1"), T0 + 60).await;
    assert_eq!(published(&fake), newer);
    assert_eq!(kids(&stored_keys(&fake)), newer);
    assert_eq!(patches(&fake), before, "A wrote nothing");
}

/// Run B (a revocation) has replaced the key in the Secret but not yet published it when run A,
/// which read the Secret before B, finds the ConfigMap showing exactly A's keys. A does not take
/// that as done: the Secret changed since A read it, so A starts over and publishes what the
/// Secret holds now, and the identity pod never starts on public keys its Secret no longer has.
#[tokio::test]
async fn a_published_view_counts_only_while_the_secret_is_unchanged() {
    let fake = fake().await;
    converge(&settings(&fake, "1"), T0).await;
    let (secret_one, config_map_one) = (fake.get(SECRET), fake.config_map(CONFIG_MAP));
    let older = kids(&stored_keys(&fake));
    // What B leaves in the Secret.
    let mut revoking = settings(&fake, "2");
    revoking.revoke_before_generation = "2".to_owned();
    converge(&revoking, T0 + 50).await;
    let secret_two = fake.get(SECRET);
    let newer = kids(&stored_keys(&fake));
    assert_eq!(newer.len(), 1);
    assert!(!older.contains(&newer[0]));
    // Back to before B; B's Secret lands between A's read of the Secret and its read of the
    // ConfigMap, which still shows A's keys.
    fake.edit(SECRET, |secret| *secret = secret_one.expect("the Secret"));
    fake.edit_config_map(CONFIG_MAP, |config_map| {
        *config_map = config_map_one.expect("the ConfigMap");
    });
    assert_eq!(published(&fake), older);
    fake.before_next(
        &format!(
            "GET /api/v1/namespaces/{}/configmaps/",
            support::secrets::NAMESPACE
        ),
        move |secrets, _| {
            secrets.insert(SECRET.to_owned(), secret_two.expect("the Secret"));
        },
    );
    converge(&settings(&fake, "1"), T0 + 60).await;
    assert_eq!(kids(&stored_keys(&fake)), newer);
    assert_eq!(published(&fake), newer);
}

#[tokio::test]
async fn a_secret_not_filled_by_the_init_container_is_not_trusted() {
    let fake = fake().await;
    // Someone created the Secret with keys of their choosing and metadata of their own.
    let planted = {
        let request = identity_keys::Request {
            generation: 9,
            revoke_before: 0,
            legacy: None,
        };
        let mut keys = SigningKeys::default()
            .plan(&request, T0, |generation| {
                Ok(identity_keys::SigningKey::from_seed(&[7; 32], generation))
            })
            .expect("plan")
            .keys;
        keys.publish(T0, PROPAGATION);
        keys
    };
    let planted_kid = kids(&planted).remove(0);
    let document = planted.to_json().to_string();
    fake.edit(SECRET, |secret| {
        secret
            .data
            .insert(SIGNING_KEYS_FILE.to_owned(), document.clone().into_bytes());
        secret.data.insert("extra".to_owned(), b"planted".to_vec());
        secret.annotations.insert(
            "replicator.example/replicate-to".to_owned(),
            "elsewhere".to_owned(),
        );
        secret.labels.insert("copy-me".to_owned(), "yes".to_owned());
        secret.finalizers.push("example.com/hold".to_owned());
    });

    let report = converge(&settings(&fake, "1"), T0 + 10).await;
    let keys = stored_keys(&fake);
    assert_eq!(keys.keys().len(), 1);
    assert_ne!(keys.keys()[0].kid(), planted_kid);
    assert_eq!(keys.keys()[0].signs_from(), Some(T0 + 10));
    assert_eq!(published(&fake), kids(&keys));
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
    let before = kids(&stored_keys(&fake));
    fake.recreate(SECRET);
    converge(&settings(&fake, "1"), T0 + 20).await;
    let after = kids(&stored_keys(&fake));
    assert_eq!(after.len(), 1);
    assert_ne!(after, before);
    assert_eq!(published(&fake), after);
}

#[tokio::test]
async fn an_unusable_key_set_is_replaced() {
    let fake = fake().await;
    converge(&settings(&fake, "1"), T0).await;
    fake.write(SECRET, SIGNING_KEYS_FILE, Some(b"{\"keys\": 7}"));
    let report = converge(&settings(&fake, "1"), T0 + 10).await;
    let keys = stored_keys(&fake);
    assert_eq!(keys.keys().len(), 1);
    assert_eq!(keys.keys()[0].signs_from(), Some(T0 + 10));
    assert!(
        report
            .changes
            .iter()
            .any(|change| change.contains("is not usable")),
        "{:?}",
        report.changes
    );
}

fn legacy_files(seed: u8, kid: &str) -> (PathBuf, String) {
    let pem = ed25519_dalek::SigningKey::from_bytes(&[seed; 32])
        .to_pkcs8_pem(LineEnding::LF)
        .expect("PKCS#8 PEM");
    let file = scratch_dir("legacy-key").join("signing-key.pem");
    std::fs::write(&file, pem.as_bytes()).expect("write the key");
    let x = URL_SAFE_NO_PAD.encode(
        ed25519_dalek::SigningKey::from_bytes(&[seed; 32])
            .verifying_key()
            .as_bytes(),
    );
    let public_keys =
        json!({"keys": [{"kty": "OKP", "crv": "Ed25519", "kid": kid, "x": x}]}).to_string();
    (file, public_keys)
}

/// The move from a person's key (docs/architecture/secrets.md, 「從人建立的私鑰搬過來」).
#[tokio::test]
async fn a_person_made_key_is_imported_then_replaced() {
    let fake = fake().await;
    let (file, public_keys) = legacy_files(5, "2026-10-a");
    let mut step1 = settings(&fake, "1");
    step1.legacy_key_file = Some(file.clone());
    step1.legacy_public_keys = Some(public_keys.clone());

    // Step 1: imported under its key id, signing at once; nothing else while it is configured.
    let report = converge(&step1, T0).await;
    let keys = stored_keys(&fake);
    assert_eq!(kids(&keys), ["2026-10-a"]);
    assert_eq!(keys.keys()[0].generation(), 0);
    assert_eq!(signer(&keys, T0).as_deref(), Some("2026-10-a"));
    assert_eq!(published(&fake), ["2026-10-a"]);
    assert_no_private_key(&fake, &texts(&report));
    step1.generation = "5".to_owned();
    let report = converge(&step1, T0 + 86_400).await;
    assert!(report.changes.is_empty(), "{:?}", report.changes);

    // Step 2: without it, keyGeneration 1 is above the imported generation 0.
    converge(&settings(&fake, "1"), T0 + 90_000).await;
    let keys = stored_keys(&fake);
    assert_eq!(keys.keys().len(), 2);
    assert_eq!(keys.keys()[0].kid(), "2026-10-a");
    assert_eq!(keys.keys()[1].signs_from(), Some(T0 + 90_000 + PROPAGATION));

    // A key the chart does not list is refused, never imported under a guessed id.
    let other = fake_with_legacy(6, &public_keys).await;
    let message = other.to_string();
    assert!(
        message.contains("not in internalIdentity.publicKeys"),
        "{message}"
    );
}

async fn fake_with_legacy(seed: u8, public_keys: &str) -> Error {
    let fake = fake().await;
    let (file, _) = legacy_files(seed, "unused");
    let mut settings = settings(&fake, "1");
    settings.legacy_key_file = Some(file);
    settings.legacy_public_keys = Some(public_keys.to_owned());
    let error = run_at(&settings, T0).await.expect_err("refused");
    assert!(fake.value(SECRET, SIGNING_KEYS_FILE).is_none());
    error
}

#[tokio::test]
async fn missing_objects_and_refused_access_stop_the_run_with_the_way_out() {
    let fake = fake().await;
    fake.delete(SECRET);
    let message = run_at(&settings(&fake, "1"), T0)
        .await
        .expect_err("no Secret")
        .to_string();
    assert!(message.contains(SECRET), "{message}");
    assert!(
        message.contains("internalIdentity.keyGeneration"),
        "{message}"
    );

    let fake = self::fake().await;
    fake.refuse_with(Some(403));
    let message = run_at(&settings(&fake, "1"), T0)
        .await
        .expect_err("refused")
        .to_string();
    assert!(message.contains("HTTP 403"), "{message}");
    assert!(
        message.contains("identity service's ServiceAccount"),
        "{message}"
    );

    // An admission webhook that rewrites what is written: the run stops instead of leaving the
    // identity service with keys the services do not have.
    let fake = self::fake().await;
    fake.rewrite_patched_values(Some(b"{\"keys\":[]}"));
    let message = run_at(&settings(&fake, "1"), T0)
        .await
        .expect_err("rewritten")
        .to_string();
    assert!(message.contains("another key set"), "{message}");

    // A Secret being deleted is not filled.
    let fake = self::fake().await;
    fake.edit(SECRET, |secret| {
        secret.deletion_timestamp = Some("2026-10-15T00:00:00Z".to_owned());
    });
    let message = run_at(&settings(&fake, "1"), T0)
        .await
        .expect_err("being deleted")
        .to_string();
    assert!(message.contains("being deleted"), "{message}");
}

#[tokio::test]
async fn the_set_never_grows_past_eight_keys() {
    let fake = fake().await;
    // Nine generations in quick succession: each new key replaces the one still waiting, so the
    // set stays at two keys and the run never fails.
    for generation in 1..=9_u32 {
        converge(
            &settings(&fake, &generation.to_string()),
            T0 + i64::from(generation),
        )
        .await;
    }
    let keys = stored_keys(&fake);
    assert_eq!(keys.keys().len(), 2);
    assert_eq!(keys.keys()[1].generation(), 9);
    assert_eq!(published(&fake), kids(&keys));
}
