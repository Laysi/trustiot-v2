use ed25519_dalek::pkcs8::EncodePrivateKey as _;
use ed25519_dalek::pkcs8::spki::der::pem::LineEnding;
use serde_json::{Value, json};

use super::*;

/// RFC 8037 appendix A: the example key, its public key and its RFC 7638 thumbprint (A.3).
const RFC8037_D: &str = "nWGxne_9WmC6hEr0kuwsxERJxWl7MmkZcDusAxyuf2A";
const RFC8037_X: &str = "11qYAYKxCrfVS_7TyWQHOg7hcvPapiMlrwIaaPcHURo";
const RFC8037_THUMBPRINT: &str = "kPrK_qmxVWaYVA9wwBF6Iuo3vVzz7TxHCTwXBygrS4k";

const NOW: i64 = 1_791_590_400;
const PROPAGATION: i64 = 600;

fn key(seed: u8, generation: u64) -> SigningKey {
    SigningKey::from_seed(&[seed; 32], generation)
}

/// A generator that hands out keys from consecutive seeds, never the operating system's.
fn seeds(first: u8) -> impl FnMut(u64) -> Result<SigningKey, RandomError> {
    let mut next = first;
    move |generation| {
        let key = key(next, generation);
        next = next.wrapping_add(1);
        Ok(key)
    }
}

fn request(generation: u64, revoke_before: u64) -> Request<'static> {
    Request {
        generation,
        revoke_before,
        legacy: None,
    }
}

/// One run of the init container: plan, then publish at the same moment.
fn run(
    keys: SigningKeys,
    request: &Request<'_>,
    now: i64,
    first_seed: u8,
) -> (SigningKeys, Vec<Change>) {
    let Plan {
        mut keys,
        mut changes,
    } = keys.plan(request, now, seeds(first_seed)).expect("plan");
    changes.extend(keys.publish(now, PROPAGATION));
    (keys, changes)
}

fn kids(keys: &SigningKeys) -> Vec<String> {
    keys.key_ids().map(str::to_owned).collect()
}

fn signer(keys: &SigningKeys, now: i64) -> Option<String> {
    keys.signing_key(now, 0, |_| true)
        .map(|key| key.kid().to_owned())
}

#[test]
fn key_ids_are_rfc7638_thumbprints() {
    let seed: [u8; 32] = URL_SAFE_NO_PAD
        .decode(RFC8037_D)
        .expect("base64url")
        .try_into()
        .expect("32 bytes");
    let key = SigningKey::from_seed(&seed, 1);
    assert_eq!(key.x(), RFC8037_X);
    assert_eq!(key.kid(), RFC8037_THUMBPRINT);
    assert_eq!(key.kid().len(), 43);
    assert!(is_key_id(key.kid()));
}

#[test]
fn generated_keys_differ_and_are_not_published() {
    let one = SigningKey::generate(3).expect("randomness");
    let two = SigningKey::generate(3).expect("randomness");
    assert_ne!(one.kid(), two.kid());
    assert_eq!(one.generation(), 3);
    assert_eq!((one.published_at(), one.signs_from()), (None, None));
    assert!(!one.signs_at(i64::MAX));
}

#[test]
fn the_document_round_trips_and_the_jwk_set_is_public_only() {
    let mut keys = SigningKeys::default();
    keys.keys.push(key(1, 1));
    keys.publish(NOW, PROPAGATION);
    keys.keys.push(key(2, 2));
    let text = keys.to_json();
    let parsed = SigningKeys::parse(&text).expect("valid document");
    assert!(parsed.same_as(&keys));
    assert_eq!(
        *parsed.to_json(),
        *text,
        "the same set writes the same text"
    );

    let document: Value = serde_json::from_str(&text).expect("JSON");
    assert_eq!(
        document["keys"][0],
        json!({
            "kid": key(1, 1).kid(), "generation": 1, "x": key(1, 1).x(),
            "d": URL_SAFE_NO_PAD.encode([1_u8; 32]),
            "published_at": NOW, "signs_from": NOW
        })
    );
    assert_eq!(
        document["keys"][1],
        json!({
            "kid": key(2, 2).kid(), "generation": 2, "x": key(2, 2).x(),
            "d": URL_SAFE_NO_PAD.encode([2_u8; 32])
        })
    );

    let jwks: Value = serde_json::from_str(&keys.jwks()).expect("JSON");
    assert_eq!(
        jwks,
        json!({"keys": [
            {"kty": "OKP", "crv": "Ed25519", "kid": key(1, 1).kid(), "x": key(1, 1).x(), "alg": "EdDSA", "use": "sig"},
            {"kty": "OKP", "crv": "Ed25519", "kid": key(2, 2).kid(), "x": key(2, 2).x(), "alg": "EdDSA", "use": "sig"},
        ]})
    );
    assert!(!keys.jwks().contains(&URL_SAFE_NO_PAD.encode([1_u8; 32])));
    assert_eq!(SigningKeys::default().jwks(), r#"{"keys":[]}"#);
    assert!(
        SigningKeys::parse(r#"{"keys":[]}"#)
            .expect("empty")
            .is_empty()
    );
}

#[test]
fn invalid_documents_are_refused_without_quoting_them() {
    let good = key(1, 1);
    let stored = |change: &dyn Fn(&mut Value)| {
        let mut entry = json!({
            "kid": good.kid(), "generation": 1, "x": good.x(),
            "d": URL_SAFE_NO_PAD.encode([1_u8; 32]),
            "published_at": NOW, "signs_from": NOW + 10
        });
        change(&mut entry);
        json!({"keys": [entry]}).to_string()
    };
    let problem = |text: &str| match SigningKeys::parse(text) {
        Err(InvalidSigningKeys::Key { index: 0, problem }) => problem,
        other => panic!("expected a key problem, got {other:?}"),
    };
    assert!(SigningKeys::parse(&stored(&|_| {})).is_ok());
    assert_eq!(
        problem(&stored(&|e| e["kid"] = json!(""))),
        KeyProblem::KeyId
    );
    assert_eq!(
        problem(&stored(&|e| e["kid"] = json!("two words"))),
        KeyProblem::KeyId
    );
    assert_eq!(
        problem(&stored(&|e| e["kid"] = json!("k".repeat(65)))),
        KeyProblem::KeyId
    );
    assert_eq!(
        problem(&stored(&|e| e["generation"] = json!(1_000_000_000_u64))),
        KeyProblem::Generation
    );
    assert_eq!(
        problem(&stored(&|e| e["d"] = json!("AAAA"))),
        KeyProblem::PrivateKey
    );
    assert_eq!(
        problem(&stored(
            &|e| e["d"] = json!(format!("{}=", URL_SAFE_NO_PAD.encode([1_u8; 32])))
        )),
        KeyProblem::PrivateKey
    );
    assert_eq!(
        problem(&stored(&|e| e["x"] = json!(key(2, 1).x()))),
        KeyProblem::PublicKeyMismatch
    );
    assert_eq!(
        problem(&stored(&|e| {
            e.as_object_mut().expect("object").remove("signs_from");
        })),
        KeyProblem::Times
    );
    assert_eq!(
        problem(&stored(&|e| e["signs_from"] = json!(NOW - 1))),
        KeyProblem::Times
    );
    assert_eq!(
        problem(&stored(&|e| e["published_at"] = json!(-1))),
        KeyProblem::Times
    );

    let twice = json!({"keys": [
        {"kid": "same", "generation": 1, "x": key(1, 1).x(), "d": URL_SAFE_NO_PAD.encode([1_u8; 32])},
        {"kid": "same", "generation": 2, "x": key(2, 1).x(), "d": URL_SAFE_NO_PAD.encode([2_u8; 32])},
    ]})
    .to_string();
    assert_eq!(
        SigningKeys::parse(&twice).err(),
        Some(InvalidSigningKeys::Key {
            index: 1,
            problem: KeyProblem::DuplicateKeyId
        })
    );
    let nine: Vec<Value> = (1..=9_u8)
        .map(|seed| {
            let key = key(seed, 1);
            json!({"kid": key.kid(), "generation": 1, "x": key.x(), "d": URL_SAFE_NO_PAD.encode([seed; 32])})
        })
        .collect();
    assert_eq!(
        SigningKeys::parse(&json!({ "keys": nine }).to_string()).err(),
        Some(InvalidSigningKeys::TooMany)
    );
    for text in [
        "",
        "{}",
        "null",
        r#"{"keys":{}}"#,
        r#"{"keys":[],"other":1}"#,
    ] {
        assert_eq!(
            SigningKeys::parse(text).err(),
            Some(InvalidSigningKeys::NotADocument),
            "{text}"
        );
    }
    assert_eq!(
        SigningKeys::parse(&" ".repeat(MAX_DOCUMENT_BYTES + 1)).err(),
        Some(InvalidSigningKeys::TooLarge)
    );

    // No error and no Debug output shows a private key.
    let private = URL_SAFE_NO_PAD.encode([1_u8; 32]);
    let leaked = stored(&|e| e["x"] = json!(private.clone()));
    let error = SigningKeys::parse(&leaked).expect_err("mismatch");
    assert!(!error.to_string().contains(&private), "{error}");
    let keys = SigningKeys::parse(&stored(&|_| {})).expect("valid");
    let debug = format!("{keys:?} {:?}", keys.keys()[0]);
    assert!(!debug.contains(&private), "{debug}");
    assert!(debug.contains(good.kid()), "{debug}");
}

#[test]
fn a_fresh_install_signs_at_once() {
    let (keys, changes) = run(SigningKeys::default(), &request(1, 0), NOW, 1);
    let kid = key(1, 1).kid().to_owned();
    assert_eq!(
        changes,
        [
            Change::Generated {
                kid: kid.clone(),
                generation: 1
            },
            Change::Published {
                kid: kid.clone(),
                signs_from: NOW
            },
        ]
    );
    assert_eq!(signer(&keys, NOW).as_deref(), Some(kid.as_str()));
    assert_eq!(signer(&keys, NOW - 1), None);

    // Running again changes nothing.
    let (again, changes) = run(keys.clone(), &request(1, 0), NOW + 3600, 9);
    assert!(changes.is_empty(), "{changes:?}");
    assert!(again.same_as(&keys));
}

/// Acceptance (Laysi/trustiot-v2#404): a rotation publishes the new key first, signs with it only
/// after the propagation time, and keeps the old public key until every token it signed has
/// expired.
#[test]
fn a_rotation_overlaps_and_then_retires_the_old_key() {
    let (keys, _) = run(SigningKeys::default(), &request(1, 0), NOW, 1);
    let old = key(1, 1).kid().to_owned();
    let new = key(2, 2).kid().to_owned();

    let bump = NOW + 86_400;
    let (keys, changes) = run(keys, &request(2, 0), bump, 2);
    assert_eq!(
        changes,
        [
            Change::Generated {
                kid: new.clone(),
                generation: 2
            },
            Change::Published {
                kid: new.clone(),
                signs_from: bump + PROPAGATION
            },
        ]
    );
    assert_eq!(kids(&keys), [old.clone(), new.clone()]);
    assert_eq!(signer(&keys, bump).as_deref(), Some(old.as_str()));
    assert_eq!(
        signer(&keys, bump + PROPAGATION - 1).as_deref(),
        Some(old.as_str())
    );
    assert_eq!(
        signer(&keys, bump + PROPAGATION).as_deref(),
        Some(new.as_str())
    );
    // A replica whose public keys do not have the new key yet keeps signing with the old one.
    assert_eq!(
        keys.signing_key(bump + PROPAGATION, 0, |key| key.kid() == old)
            .map(SigningKey::kid),
        Some(old.as_str())
    );

    let retire_at = bump + PROPAGATION + PROPAGATION + TOKEN_EXPIRY_SECS;
    assert_eq!(keys.retire_at(bump + PROPAGATION), Some(retire_at));
    let (kept, changes) = run(keys.clone(), &request(2, 0), retire_at - 1, 9);
    assert!(changes.is_empty(), "{changes:?}");
    assert_eq!(kids(&kept), [old.clone(), new.clone()]);
    let (retired, changes) = run(keys, &request(2, 0), retire_at, 9);
    assert_eq!(changes, [Change::Retired { kid: old }]);
    assert_eq!(kids(&retired), [new]);
}

#[test]
fn generations_only_go_up() {
    let (keys, _) = run(SigningKeys::default(), &request(5, 0), NOW, 1);
    for lower in [0, 1, 4, 5] {
        let (same, changes) = run(keys.clone(), &request(lower, 0), NOW + 10, 9);
        assert!(changes.is_empty(), "generation {lower}: {changes:?}");
        assert!(same.same_as(&keys));
    }
}

#[test]
fn a_bump_while_a_key_waits_replaces_the_waiting_key() {
    let (keys, _) = run(SigningKeys::default(), &request(1, 0), NOW, 1);
    let (keys, _) = run(keys, &request(2, 0), NOW + 100, 2);
    let waiting = key(2, 2).kid().to_owned();
    let (keys, changes) = run(keys, &request(3, 0), NOW + 200, 3);
    let newest = key(3, 3).kid().to_owned();
    assert_eq!(
        changes,
        [
            Change::Dropped { kid: waiting },
            Change::Generated {
                kid: newest.clone(),
                generation: 3
            },
            Change::Published {
                kid: newest.clone(),
                signs_from: NOW + 200 + PROPAGATION
            },
        ]
    );
    assert_eq!(kids(&keys), [key(1, 1).kid().to_owned(), newest]);
}

#[test]
fn an_interrupted_run_is_finished_by_the_next() {
    // Step 1 written, then the init container stopped: the key has no times yet.
    let (keys, _) = run(SigningKeys::default(), &request(1, 0), NOW, 1);
    let Plan { keys, changes } = keys.plan(&request(2, 0), NOW + 50, seeds(2)).expect("plan");
    assert_eq!(changes.len(), 1);
    assert!(keys.has_unpublished());
    assert_eq!(signer(&keys, i64::MAX).as_deref(), Some(key(1, 1).kid()));

    // The next run publishes it at its own time and generates nothing.
    let later = NOW + 500;
    let (keys, changes) = run(keys, &request(2, 0), later, 9);
    assert_eq!(
        changes,
        [Change::Published {
            kid: key(2, 2).kid().to_owned(),
            signs_from: later + PROPAGATION
        }]
    );
    assert!(!keys.has_unpublished());
}

#[test]
fn revocation_removes_older_generations_and_signs_at_once() {
    let (keys, _) = run(SigningKeys::default(), &request(1, 0), NOW, 1);
    let (keys, _) = run(keys, &request(2, 0), NOW + 100, 2);
    let (keys, changes) = run(keys, &request(3, 3), NOW + 200, 3);
    let newest = key(3, 3).kid().to_owned();
    assert_eq!(
        changes,
        [
            Change::Revoked {
                kid: key(1, 1).kid().to_owned(),
                generation: 1
            },
            Change::Revoked {
                kid: key(2, 2).kid().to_owned(),
                generation: 2
            },
            Change::Generated {
                kid: newest.clone(),
                generation: 3
            },
            Change::Published {
                kid: newest.clone(),
                signs_from: NOW + 200
            },
        ]
    );
    assert_eq!(kids(&keys), std::slice::from_ref(&newest));
    assert_eq!(signer(&keys, NOW + 200).as_deref(), Some(newest.as_str()));
    // A replica that still reads the old set never signs with a revoked key.
    let (old, _) = run(SigningKeys::default(), &request(1, 0), NOW, 1);
    assert!(old.signing_key(NOW + 200, 3, |_| true).is_none());
    // A lower revocation later does not bring anything back.
    let (same, changes) = run(keys.clone(), &request(3, 0), NOW + 300, 9);
    assert!(changes.is_empty(), "{changes:?}");
    assert!(same.same_as(&keys));
}

#[test]
fn the_set_never_grows_past_eight_keys() {
    let mut keys = SigningKeys::default();
    let mut now = NOW;
    for generation in 1..=12_u8 {
        // Each bump comes after the previous key's overlap ended, so the key before it retires.
        (keys, _) = run(keys, &request(u64::from(generation), 0), now, generation);
        now += 2 * PROPAGATION + TOKEN_EXPIRY_SECS;
    }
    assert_eq!(
        keys.keys().len(),
        2,
        "older keys retire as the rotations go"
    );

    // Without retiring: build eight signing keys directly.
    let mut full = SigningKeys::default();
    for seed in 1..=8_u8 {
        let mut key = key(seed, u64::from(seed));
        key.published_at = Some(NOW);
        key.signs_from = Some(NOW + i64::from(seed));
        full.keys.push(key);
    }
    let error = full
        .plan(&request(9, 0), NOW + 20, seeds(9))
        .expect_err("a ninth key");
    let retire_at = NOW + 8 + 8 + TOKEN_EXPIRY_SECS;
    assert_eq!(
        error,
        PlanError::TooMany {
            retire_at: Some(retire_at)
        }
    );
    assert!(
        error.to_string().contains(&retire_at.to_string()),
        "{error}"
    );
}

fn legacy_pem(seed: u8) -> String {
    ed25519_dalek::SigningKey::from_bytes(&[seed; 32])
        .to_pkcs8_pem(LineEnding::LF)
        .expect("PKCS#8 PEM")
        .to_string()
}

fn legacy_public_keys(entries: &[(u8, &str)]) -> String {
    let keys: Vec<Value> = entries
        .iter()
        .map(|(seed, kid)| json!({"kty": "OKP", "crv": "Ed25519", "kid": kid, "x": key(*seed, 1).x()}))
        .collect();
    json!({ "keys": keys }).to_string()
}

#[test]
fn a_person_made_key_is_imported_under_its_key_id() {
    let legacy = SigningKey::import(
        &legacy_pem(7),
        &legacy_public_keys(&[(6, "2026-09-a"), (7, "2026-10-a")]),
    )
    .expect("listed once");
    assert_eq!(legacy.kid(), "2026-10-a");
    assert_eq!(legacy.generation(), IMPORTED_GENERATION);

    assert_eq!(
        SigningKey::import(&legacy_pem(7), &legacy_public_keys(&[(6, "a")])).err(),
        Some(ImportError::NotListed)
    );
    assert_eq!(
        SigningKey::import(&legacy_pem(7), &legacy_public_keys(&[(7, "a"), (7, "b")])).err(),
        Some(ImportError::ListedTwice)
    );
    assert_eq!(
        SigningKey::import("not a key", &legacy_public_keys(&[(7, "a")])).err(),
        Some(ImportError::NotPkcs8Ed25519)
    );
    assert_eq!(
        SigningKey::import(&legacy_pem(7), "[]").err(),
        Some(ImportError::PublicKeysInvalid)
    );
    assert_eq!(
        SigningKey::import(&legacy_pem(7), &legacy_public_keys(&[(7, "two words")])).err(),
        Some(ImportError::PublicKeysInvalid)
    );
}

/// The move from a person's key (docs/architecture/secrets.md, 「從人建立的私鑰搬過來」).
#[test]
fn the_move_from_a_person_made_key_takes_three_steps() {
    let legacy = SigningKey::import(&legacy_pem(7), &legacy_public_keys(&[(7, "2026-10-a")]))
        .expect("importable");
    let with_legacy = Request {
        generation: 1,
        revoke_before: 0,
        legacy: Some(&legacy),
    };

    // Step 1: the empty set gets the person's key, which signs at once.
    let (keys, changes) = run(SigningKeys::default(), &with_legacy, NOW, 1);
    assert_eq!(
        changes,
        [
            Change::Imported {
                kid: "2026-10-a".to_owned()
            },
            Change::Published {
                kid: "2026-10-a".to_owned(),
                signs_from: NOW
            },
        ]
    );
    assert_eq!(signer(&keys, NOW).as_deref(), Some("2026-10-a"));
    // While the person's key is configured, nothing is added or retired, whatever the generation.
    let (same, changes) = run(
        keys.clone(),
        &Request {
            generation: 7,
            ..with_legacy
        },
        NOW + 86_400,
        2,
    );
    assert!(changes.is_empty(), "{changes:?}");
    assert!(same.same_as(&keys));

    // Step 2: without it, keyGeneration (at least 1) is above the imported generation 0.
    let step2 = NOW + 3600;
    let (keys, _) = run(keys, &request(1, 0), step2, 2);
    let new = key(2, 1).kid().to_owned();
    assert_eq!(kids(&keys), ["2026-10-a".to_owned(), new.clone()]);
    assert_eq!(signer(&keys, step2).as_deref(), Some("2026-10-a"));
    assert_eq!(
        signer(&keys, step2 + PROPAGATION).as_deref(),
        Some(new.as_str())
    );
    let retire_at = step2 + 2 * PROPAGATION + TOKEN_EXPIRY_SECS;
    let (keys, changes) = run(keys, &request(1, 0), retire_at, 9);
    assert_eq!(
        changes,
        [Change::Retired {
            kid: "2026-10-a".to_owned()
        }]
    );
    assert_eq!(kids(&keys), std::slice::from_ref(&new));

    // A person's key still configured never comes back into a set that replaced it.
    let (same, changes) = run(keys.clone(), &with_legacy, retire_at + 10, 9);
    assert!(changes.is_empty(), "{changes:?}");
    assert!(same.same_as(&keys));
}

#[test]
fn revocation_works_while_a_person_made_key_is_configured() {
    let legacy = SigningKey::import(&legacy_pem(7), &legacy_public_keys(&[(7, "2026-10-a")]))
        .expect("importable");
    let (keys, _) = run(
        SigningKeys::default(),
        &Request {
            generation: 1,
            revoke_before: 0,
            legacy: Some(&legacy),
        },
        NOW,
        1,
    );
    let (keys, changes) = run(
        keys,
        &Request {
            generation: 1,
            revoke_before: 1,
            legacy: Some(&legacy),
        },
        NOW + 60,
        2,
    );
    let new = key(2, 1).kid().to_owned();
    assert_eq!(
        changes,
        [
            Change::Revoked {
                kid: "2026-10-a".to_owned(),
                generation: 0
            },
            Change::Generated {
                kid: new.clone(),
                generation: 1
            },
            Change::Published {
                kid: new.clone(),
                signs_from: NOW + 60
            },
        ]
    );
    assert_eq!(kids(&keys), [new]);
}

#[test]
fn the_signing_key_honours_revocation_and_publication() {
    let (keys, _) = run(SigningKeys::default(), &request(1, 0), NOW, 1);
    let (keys, _) = run(keys, &request(2, 0), NOW + 10, 2);
    let after = NOW + 10 + PROPAGATION;
    assert_eq!(signer(&keys, after).as_deref(), Some(key(2, 2).kid()));
    assert_eq!(
        keys.signing_key(after, 2, |candidate| candidate.kid() == key(1, 1).kid())
            .map(SigningKey::kid),
        None,
        "the only published key is revoked"
    );
    assert_eq!(
        keys.signing_key(after, 0, |_| false).map(SigningKey::kid),
        None
    );
}

#[test]
fn changes_name_key_ids_and_times_only() {
    let line = Change::Published {
        kid: "k".to_owned(),
        signs_from: 42,
    }
    .to_string();
    assert_eq!(
        line,
        "published signing key k; it signs from 42 (Unix time)"
    );
}
