//! The generated passwords against a fake Kubernetes API (support/secrets.rs): a first run fills
//! the empty Secrets the chart declares, a second run changes nothing, a new password generation
//! rotates every key, a lost or unusable key is regenerated alone, a concurrent change of a
//! Secret is never overwritten, and a missing Secret or refused access stops the run with the way
//! out. Nothing a report, an error or `Debug` shows contains a password. No containers needed.

mod support;

use std::collections::BTreeMap;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use migrate::passwords::{GENERATION_ANNOTATION, Passwords, UID_ANNOTATION, converge};
use migrate::{Error, Report};
use support::secrets::FakeSecrets;

const IDENTITY: &str = "trustiot-identity-password";
const ASSETS: &str = "trustiot-assets-password";
const COLLECTOR: &str = "trustiot-otel-collector-password";

/// Longer than several retries (every 2 s) on a loaded machine.
const WAIT: Duration = Duration::from_secs(120);

/// The fake with three empty Secrets, as the chart declares them: two services (one with two
/// login roles in its Secret) and the collector.
async fn fake() -> FakeSecrets {
    let fake = FakeSecrets::start().await;
    for secret in [IDENTITY, ASSETS, COLLECTOR] {
        fake.declare(secret);
    }
    fake
}

fn settings(fake: &FakeSecrets, generation: &str) -> migrate::config::PasswordSettings {
    fake.settings(
        generation,
        &[
            ("identity_app", IDENTITY),
            ("assets_app", ASSETS),
            ("assets_platform", ASSETS),
        ],
        Some(("trustiot_collector", COLLECTOR)),
    )
}

/// Every value the fake holds, as text and as the base64 the API sends.
fn every_value(fake: &FakeSecrets) -> Vec<String> {
    let mut values = Vec::new();
    for secret in [IDENTITY, ASSETS, COLLECTOR] {
        for value in fake.get(secret).expect("declared").data.values() {
            values.push(String::from_utf8(value.clone()).expect("UTF-8"));
            values.push(STANDARD.encode(value));
        }
    }
    values
}

/// Fails when any text the run produced contains a stored value.
fn assert_no_value_in(fake: &FakeSecrets, texts: &[String]) {
    for value in every_value(fake) {
        for text in texts {
            assert!(
                !text.contains(&value),
                "a password or its base64 reached a report, an error or Debug output"
            );
        }
    }
}

fn texts(report: &Report, passwords: Option<&Passwords>) -> Vec<String> {
    let mut texts = report.changes.clone();
    texts.push(format!("{report:?}"));
    if let Some(passwords) = passwords {
        texts.push(format!("{passwords:?}"));
    }
    texts
}

fn generation_of(fake: &FakeSecrets, secret: &str) -> Option<String> {
    fake.get(secret)
        .expect("declared")
        .annotations
        .get(GENERATION_ANNOTATION)
        .cloned()
}

#[tokio::test]
async fn a_first_run_fills_the_empty_secrets_and_a_second_changes_nothing() {
    let fake = fake().await;
    let mut report = Report::default();
    let passwords = converge(&settings(&fake, "1"), WAIT, &mut report)
        .await
        .expect("the first run");
    assert_eq!(
        report.changes,
        [
            format!("generated the password of assets_app in Secret {ASSETS} (it had none)"),
            format!("generated the password of assets_platform in Secret {ASSETS} (it had none)"),
            format!("generated the password of identity_app in Secret {IDENTITY} (it had none)"),
            format!(
                "generated the password of trustiot_collector in Secret {COLLECTOR} (it had none)"
            ),
        ]
    );
    for (secret, key) in [
        (IDENTITY, "identity_app"),
        (ASSETS, "assets_app"),
        (ASSETS, "assets_platform"),
        (COLLECTOR, "trustiot_collector"),
    ] {
        let value = fake.value(secret, key).expect("filled");
        assert_eq!(value.len(), 64, "{key}: 256 bits as hex digits");
        assert!(value.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_eq!(generation_of(&fake, secret).as_deref(), Some("1"));
        let handed = if key == "trustiot_collector" {
            passwords.collector().map(|(account, password)| {
                assert_eq!(account, key);
                password.expose().to_owned()
            })
        } else {
            passwords
                .postgres(key)
                .map(|password| password.expose().to_owned())
        };
        assert_eq!(
            handed.as_deref(),
            Some(value.as_str()),
            "{key}: the run sets what it stored"
        );
    }
    assert_ne!(
        fake.value(ASSETS, "assets_app"),
        fake.value(ASSETS, "assets_platform"),
        "every key gets its own password"
    );
    assert!(passwords.postgres("nobody_app").is_none());
    assert_no_value_in(&fake, &texts(&report, Some(&passwords)));

    // Only GET and PATCH of the bound Secrets, with the token, as the chart's Role allows.
    let seen = fake.seen();
    let lines: Vec<&str> = seen.iter().map(|seen| seen.line.as_str()).collect();
    let path = |secret: &str| format!("/api/v1/namespaces/release-ns/secrets/{secret}");
    let mut expected = Vec::new();
    for secret in [ASSETS, IDENTITY, COLLECTOR] {
        expected.push(format!("GET {} HTTP/1.1", path(secret)));
        expected.push(format!(
            "PATCH {}?fieldManager=trustiot-migrate HTTP/1.1",
            path(secret)
        ));
    }
    let mut sorted_lines = lines.clone();
    sorted_lines.sort_unstable();
    expected.sort();
    assert_eq!(sorted_lines, expected);
    assert!(seen.iter().all(|seen| seen.authorized));
    assert!(
        seen.iter()
            .filter(|seen| seen.line.starts_with("PATCH"))
            .all(|seen| seen.content_type == "application/merge-patch+json")
    );

    let before = every_value(&fake);
    let requests = fake.seen().len();
    let mut report = Report::default();
    let again = converge(&settings(&fake, "1"), WAIT, &mut report)
        .await
        .expect("the second run");
    assert!(report.changes.is_empty(), "{:?}", report.changes);
    assert_eq!(
        every_value(&fake),
        before,
        "the second run changed a Secret"
    );
    let after: Vec<String> = fake.seen()[requests..]
        .iter()
        .map(|seen| seen.line.clone())
        .collect();
    assert_eq!(after.len(), 3, "one GET per Secret and no PATCH: {after:?}");
    assert!(after.iter().all(|line| line.starts_with("GET ")));
    assert_eq!(
        again
            .postgres("identity_app")
            .map(|password| password.expose().to_owned()),
        fake.value(IDENTITY, "identity_app")
    );
}

#[tokio::test]
async fn a_new_generation_rotates_every_managed_key_and_nothing_else() {
    let fake = fake().await;
    converge(&settings(&fake, "1"), WAIT, &mut Report::default())
        .await
        .expect("the first run");
    // A key the run does not manage, written by someone else, is left alone.
    fake.write(ASSETS, "unmanaged", Some(b"kept as it is"));
    let old = |key: &str, secret: &str| fake.value(secret, key).expect("filled");
    let before = [
        old("identity_app", IDENTITY),
        old("assets_app", ASSETS),
        old("assets_platform", ASSETS),
        old("trustiot_collector", COLLECTOR),
    ];
    let mut report = Report::default();
    converge(&settings(&fake, "2"), WAIT, &mut report)
        .await
        .expect("the rotation run");
    assert_eq!(report.changes.len(), 4, "{:?}", report.changes);
    assert!(
        report
            .changes
            .iter()
            .all(|change| change.ends_with("(password generation 2)"))
    );
    let after = [
        old("identity_app", IDENTITY),
        old("assets_app", ASSETS),
        old("assets_platform", ASSETS),
        old("trustiot_collector", COLLECTOR),
    ];
    for (old, new) in before.iter().zip(&after) {
        assert_ne!(old, new, "every managed key got a new password");
    }
    assert_eq!(
        fake.value(ASSETS, "unmanaged").as_deref(),
        Some("kept as it is")
    );
    for secret in [IDENTITY, ASSETS, COLLECTOR] {
        assert_eq!(generation_of(&fake, secret).as_deref(), Some("2"));
    }
    let mut report = Report::default();
    converge(&settings(&fake, "2"), WAIT, &mut report)
        .await
        .expect("the run after the rotation");
    assert!(
        report.changes.is_empty(),
        "a rotation happens once: {:?}",
        report.changes
    );
}

#[tokio::test]
async fn a_rollback_keeps_the_passwords_and_so_does_the_re_upgrade() {
    // Flux's remediation rolls back to the release before the rotation, which renders the older
    // migrate.passwordGeneration, and then upgrades again (Laysi/trustiot-v2#60 round 4, R5).
    let fake = fake().await;
    converge(&settings(&fake, "2"), WAIT, &mut Report::default())
        .await
        .expect("the first run");
    converge(&settings(&fake, "3"), WAIT, &mut Report::default())
        .await
        .expect("the rotation");
    let rotated = every_value(&fake);
    for requested in ["2", "1", "3"] {
        let mut report = Report::default();
        converge(&settings(&fake, requested), WAIT, &mut report)
            .await
            .expect("a rollback or a re-upgrade");
        assert!(
            report.changes.is_empty(),
            "generation {requested}: {:?}",
            report.changes
        );
        assert_eq!(every_value(&fake), rotated, "generation {requested}");
        for secret in [IDENTITY, ASSETS, COLLECTOR] {
            assert_eq!(generation_of(&fake, secret).as_deref(), Some("3"));
        }
    }
    // A key lost while the older generation is requested is filled under the recorded one, so
    // the re-upgrade after it rotates nothing either.
    fake.write(IDENTITY, "identity_app", None);
    let mut report = Report::default();
    converge(&settings(&fake, "1"), WAIT, &mut report)
        .await
        .expect("the run that fills the lost key");
    assert_eq!(
        report.changes,
        [format!(
            "generated the password of identity_app in Secret {IDENTITY} (it had none)"
        )]
    );
    assert_eq!(generation_of(&fake, IDENTITY).as_deref(), Some("3"));
    let mut report = Report::default();
    converge(&settings(&fake, "3"), WAIT, &mut report)
        .await
        .expect("the re-upgrade");
    assert!(report.changes.is_empty(), "{:?}", report.changes);
}

#[tokio::test]
async fn a_secret_someone_else_created_never_keeps_its_passwords() {
    // Someone deletes the kept Secret and creates one under the same name with passwords of
    // their choosing and the Job's annotations; Helm adopts it on the next install (R2). The uid
    // the Job recorded is not the new object's, so every key is replaced.
    let fake = fake().await;
    converge(&settings(&fake, "1"), WAIT, &mut Report::default())
        .await
        .expect("the first run");
    let uid = fake.get(IDENTITY).expect("declared").uid;
    assert_eq!(
        fake.get(IDENTITY)
            .expect("declared")
            .annotations
            .get(UID_ANNOTATION),
        Some(&uid),
        "the Job records the uid of the Secret it filled"
    );
    let chosen = "c".repeat(64);
    fake.write(IDENTITY, "identity_app", Some(chosen.as_bytes()));
    fake.recreate(IDENTITY);
    let assets = fake.value(ASSETS, "assets_app");
    let mut report = Report::default();
    let passwords = converge(&settings(&fake, "1"), WAIT, &mut report)
        .await
        .expect("the run after the Secret was replaced");
    assert_eq!(
        report.changes,
        [format!(
            "generated the password of identity_app in Secret {IDENTITY} (the Secret is not the one the Job filled)"
        )]
    );
    let stored = fake.value(IDENTITY, "identity_app").expect("filled");
    assert_ne!(stored, chosen, "the chosen password was replaced");
    assert_eq!(
        passwords
            .postgres("identity_app")
            .map(|password| password.expose().to_owned()),
        Some(stored)
    );
    let replaced = fake.get(IDENTITY).expect("declared");
    assert_eq!(
        replaced.annotations.get(UID_ANNOTATION),
        Some(&replaced.uid)
    );
    assert_ne!(replaced.uid, uid);
    assert_eq!(
        fake.value(ASSETS, "assets_app"),
        assets,
        "other Secrets are left alone"
    );
    // A forged uid annotation does not help: it must equal the object's own uid.
    fake.annotate(
        IDENTITY,
        UID_ANNOTATION,
        "00000000-0000-4000-8000-999999999999",
    );
    let mut report = Report::default();
    converge(&settings(&fake, "1"), WAIT, &mut report)
        .await
        .expect("the run after a forged annotation");
    assert_eq!(report.changes.len(), 1, "{:?}", report.changes);
}

#[tokio::test]
async fn binding_a_foreign_secret_removes_metadata_neither_the_chart_nor_the_job_sets() {
    // Someone with create rights pre-made the Secret Helm then adopted: a replicator's
    // annotation and label, an owner that could garbage-collect it, a finalizer, and a password of
    // their choosing (Laysi/trustiot-v2#60 round 5, N3). The patch that writes the generated
    // password removes all of it, and the report names the keys, never a value.
    let fake = fake().await;
    let pairs = |pairs: &[(&str, &str)]| -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect()
    };
    fake.edit(IDENTITY, |secret| {
        secret.annotations = pairs(&[
            ("helm.sh/resource-policy", "keep"),
            ("meta.helm.sh/release-name", "trustiot"),
            ("meta.helm.sh/release-namespace", "release-ns"),
            ("trustiot.io/description", "Generated by the migration Job"),
            (GENERATION_ANNOTATION, "1"),
            ("replicator.example/replicate-to", "namespace-elsewhere"),
        ]);
        secret.labels = pairs(&[
            ("app.kubernetes.io/instance", "trustiot"),
            ("trustiot.io/filled-by", "migrate"),
            ("copy-to", "namespace-elsewhere"),
        ]);
        secret.owner_references = vec![serde_json::json!({
            "apiVersion": "v1", "kind": "ConfigMap", "name": "holder", "uid": "u-holder"
        })];
        secret.finalizers = vec!["example.test/hold".to_owned()];
        secret
            .data
            .insert("identity_app".to_owned(), "c".repeat(64).into_bytes());
    });
    let mut report = Report::default();
    converge(&settings(&fake, "1"), WAIT, &mut report)
        .await
        .expect("the run that binds the Secret");
    let identity: Vec<&String> = report
        .changes
        .iter()
        .filter(|change| change.contains(IDENTITY))
        .collect();
    assert_eq!(
        identity,
        [
            &format!(
                "removed from Secret {IDENTITY} what neither the chart nor the Job sets: \
                 annotation replicator.example/replicate-to, label copy-to, ownerReferences, \
                 finalizers"
            ),
            &format!(
                "generated the password of identity_app in Secret {IDENTITY} (the Secret is not the one the Job filled)"
            ),
        ]
    );
    assert!(
        report
            .changes
            .iter()
            .all(|change| !change.contains("namespace-elsewhere") && !change.contains("holder")),
        "names only: {:?}",
        report.changes
    );
    let bound = fake.get(IDENTITY).expect("declared");
    assert_eq!(
        bound
            .annotations
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        [
            "helm.sh/resource-policy",
            "meta.helm.sh/release-name",
            "meta.helm.sh/release-namespace",
            "trustiot.io/description",
            GENERATION_ANNOTATION,
            UID_ANNOTATION,
        ]
    );
    assert_eq!(
        bound.labels.keys().map(String::as_str).collect::<Vec<_>>(),
        ["app.kubernetes.io/instance", "trustiot.io/filled-by"]
    );
    assert!(bound.owner_references.is_empty());
    assert!(bound.finalizers.is_empty());
    assert_ne!(
        fake.value(IDENTITY, "identity_app").as_deref(),
        Some("c".repeat(64).as_str())
    );

    // Once bound, its metadata is left alone (patch on these Secrets is for trusted principals
    // only: it can write a password of one's choosing, docs/architecture/secrets.md).
    fake.edit(IDENTITY, |secret| {
        secret
            .labels
            .insert("added-later".to_owned(), "x".to_owned());
    });
    let mut report = Report::default();
    converge(&settings(&fake, "1"), WAIT, &mut report)
        .await
        .expect("the run after another writer labelled the bound Secret");
    assert!(report.changes.is_empty(), "{:?}", report.changes);
    assert!(
        fake.get(IDENTITY)
            .expect("declared")
            .labels
            .contains_key("added-later")
    );
}

#[tokio::test]
async fn a_secret_being_deleted_is_never_filled() {
    // Generated passwords in a Secret that is about to go would leave the roles with passwords
    // nobody can read; the patch that binds it would also remove the finalizers holding it.
    let fake = fake().await;
    fake.edit(IDENTITY, |secret| {
        secret.finalizers = vec!["example.test/hold".to_owned()];
        secret.deletion_timestamp = Some("2026-10-14T00:00:00Z".to_owned());
    });
    let error = converge(&settings(&fake, "1"), WAIT, &mut Report::default())
        .await
        .expect_err("a Secret being deleted must stop the run");
    assert!(
        matches!(&error, Error::Secret { secret, problem } if secret == IDENTITY && problem.contains("is being deleted")),
        "{error}"
    );
    assert!(
        !fake
            .seen()
            .iter()
            .any(|seen| seen.line.starts_with("PATCH") && seen.line.contains(IDENTITY)),
        "it was not patched"
    );
    assert_eq!(
        fake.get(IDENTITY).expect("declared").finalizers,
        ["example.test/hold"]
    );
}

#[tokio::test]
async fn a_lost_or_unusable_key_is_regenerated_alone() {
    let fake = fake().await;
    converge(&settings(&fake, "1"), WAIT, &mut Report::default())
        .await
        .expect("the first run");
    let platform = fake.value(ASSETS, "assets_platform");
    let identity = fake.value(IDENTITY, "identity_app");
    fake.write(ASSETS, "assets_app", None);
    fake.write(COLLECTOR, "trustiot_collector", Some(b"short\n"));
    let mut report = Report::default();
    converge(&settings(&fake, "1"), WAIT, &mut report)
        .await
        .expect("the run after the loss");
    assert_eq!(
        report.changes,
        [
            format!("generated the password of assets_app in Secret {ASSETS} (it had none)"),
            format!(
                "generated the password of trustiot_collector in Secret {COLLECTOR} (the stored one was not usable)"
            ),
        ]
    );
    assert_eq!(fake.value(ASSETS, "assets_platform"), platform);
    assert_eq!(fake.value(IDENTITY, "identity_app"), identity);
    assert_eq!(
        fake.value(ASSETS, "assets_app").map(|value| value.len()),
        Some(64)
    );

    // A whole Secret gone (helm upgrade recreates it empty): the next run fills it again.
    fake.delete(IDENTITY);
    let error = converge(&settings(&fake, "1"), WAIT, &mut Report::default())
        .await
        .expect_err("a missing Secret stops the run");
    let text = error.to_string();
    assert!(matches!(error, Error::Secret { .. }), "{text}");
    for part in [
        IDENTITY,
        "does not exist",
        "helm upgrade",
        "migrate.revision",
    ] {
        assert!(text.contains(part), "{part:?} missing from: {text}");
    }
    fake.declare(IDENTITY);
    let mut report = Report::default();
    converge(&settings(&fake, "1"), WAIT, &mut report)
        .await
        .expect("the run after helm upgrade recreated the Secret");
    assert_eq!(
        report.changes,
        [format!(
            "generated the password of identity_app in Secret {IDENTITY} (it had none)"
        )]
    );
    assert_ne!(fake.value(IDENTITY, "identity_app"), identity);
}

#[tokio::test]
async fn a_concurrent_change_is_never_overwritten() {
    let fake = fake().await;
    // The first patch of the run finds the Secret changed since the run read it: the API refuses
    // it (409), and the run reads the Secret again and decides anew.
    fake.interfere(1);
    let mut report = Report::default();
    converge(&settings(&fake, "1"), WAIT, &mut report)
        .await
        .expect("the run after one conflict");
    assert_eq!(
        report.changes.len(),
        4,
        "each key generated once: {:?}",
        report.changes
    );
    let patches = fake
        .seen()
        .iter()
        .filter(|seen| seen.line.starts_with("PATCH"))
        .count();
    assert_eq!(patches, 4, "three Secrets and one refused patch");

    // A Secret that keeps changing stops the run instead of a write that wins blindly.
    fake.write(IDENTITY, "identity_app", None);
    fake.interfere(100);
    let error = converge(&settings(&fake, "1"), WAIT, &mut Report::default())
        .await
        .expect_err("a Secret that changes on every patch");
    assert!(
        matches!(&error, Error::Secret { problem, .. } if problem.contains("something else writes it")),
        "{error}"
    );
    assert_eq!(
        fake.value(IDENTITY, "identity_app"),
        None,
        "nothing was overwritten"
    );
}

#[tokio::test]
async fn a_value_the_api_did_not_store_as_written_stops_the_run() {
    // Something between the run and the stored Secret (an admission webhook, say) changes what
    // the patch wrote: setting the role to the run's password would leave the role and its
    // Secret apart, so the run stops before it uses the password.
    let fake = fake().await;
    fake.rewrite_patched_values(Some(b"rewritten-by-someone-else".as_slice()));
    let mut report = Report::default();
    let error = converge(&settings(&fake, "1"), WAIT, &mut report)
        .await
        .expect_err("a value stored differently from what the run wrote");
    assert!(
        matches!(&error, Error::Secret { problem, .. } if problem.contains("stored another value")),
        "{error}"
    );
    assert!(
        report.changes.is_empty(),
        "nothing reported as generated: {:?}",
        report.changes
    );
    assert_no_value_in(&fake, &[error.to_string(), format!("{error:?}")]);
}

#[tokio::test]
async fn refused_access_and_transient_failures() {
    let fake = fake().await;
    for (status, verb) in [(403, "get"), (401, "get")] {
        fake.refuse_with(Some(status));
        let error = converge(&settings(&fake, "1"), WAIT, &mut Report::default())
            .await
            .expect_err("refused");
        let text = error.to_string();
        assert!(
            text.contains(&format!("refused to {verb} it (HTTP {status})"))
                && text.contains("get and patch exactly the Secrets"),
            "{text}"
        );
    }
    // A token the API does not accept.
    fake.refuse_with(None);
    let mut wrong = settings(&fake, "1");
    std::fs::write(&wrong.token_file, "not-the-token").expect("write a wrong token");
    let error = converge(&wrong, WAIT, &mut Report::default())
        .await
        .expect_err("a wrong token");
    assert!(error.to_string().contains("HTTP 401"), "{error}");
    wrong.token_file = wrong.token_file.with_extension("missing");
    let error = converge(&wrong, WAIT, &mut Report::default())
        .await
        .expect_err("no token file");
    assert!(
        error
            .to_string()
            .contains("cannot read the ServiceAccount token"),
        "{error}"
    );

    // An API that answers 503 for a while is waited out within the bound.
    fake.refuse_with(Some(503));
    let healer = fake.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(3)).await;
        healer.refuse_with(None);
    });
    let mut report = Report::default();
    converge(&settings(&fake, "1"), WAIT, &mut report)
        .await
        .expect("the run after the API recovered");
    assert_eq!(report.changes.len(), 4, "{:?}", report.changes);
    // And one that does not recover gives up at the bound, naming what it saw.
    fake.refuse_with(Some(503));
    let error = converge(
        &settings(&fake, "2"),
        Duration::from_secs(1),
        &mut Report::default(),
    )
    .await
    .expect_err("an API that never recovers");
    assert!(error.to_string().contains("gave up after"), "{error}");
    assert!(error.to_string().contains("HTTP 503"), "{error}");
    fake.refuse_with(None);
    assert_no_value_in(&fake, &[error.to_string()]);
}

#[tokio::test]
async fn nothing_bound_makes_no_request() {
    let fake = fake().await;
    let mut empty = fake.settings("1", &[], None);
    empty.postgres = None;
    let passwords = converge(&empty, WAIT, &mut Report::default())
        .await
        .expect("nothing to do");
    assert!(passwords.collector().is_none());
    assert!(fake.seen().is_empty());
}
