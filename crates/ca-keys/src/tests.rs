use serde_json::{Value, json};

use super::*;

fn document(keys: &CaKeys) -> Value {
    serde_json::from_str(&keys.to_json()).expect("JSON")
}

fn planned(keys: CaKeys, generation: u64, remove_before: u64) -> Plan {
    keys.plan(
        Request {
            generation,
            remove_before,
        },
        1_000,
        CaKey::generate,
    )
    .expect("plan")
}

fn reparse(value: &Value) -> Result<CaKeys, KeysError> {
    CaKeys::parse(&value.to_string())
}

#[test]
fn a_set_round_trips_to_the_same_text() {
    let Plan { keys, changes } = planned(CaKeys::default(), 1, 0);
    assert_eq!(changes.len(), 1, "{changes:?}");
    let Plan { keys, .. } = planned(keys, 2, 0);
    let text = keys.to_json();
    let again = CaKeys::parse(&text).expect("parse");
    assert_eq!(*again.to_json(), *text);
    assert_eq!(
        again.key_ids().collect::<Vec<_>>(),
        keys.key_ids().collect::<Vec<_>>()
    );
    for key in again.keys() {
        assert_eq!(key.id().len(), 64);
        assert_eq!(key.id(), key_id(&key.subject_public_key_info()));
        assert_eq!(key.created_at(), 1_000);
    }
    assert_eq!(
        again
            .keys()
            .iter()
            .map(CaKey::generation)
            .collect::<Vec<_>>(),
        [1, 2]
    );
}

#[test]
fn every_key_has_a_request_for_its_own_public_key() {
    let Plan { keys, .. } = planned(CaKeys::default(), 7, 0);
    let requests = keys.requests();
    assert_eq!(
        requests.keys().collect::<Vec<_>>(),
        ["issuing-ca-generation-7.csr.pem"]
    );
    let pem = &requests["issuing-ca-generation-7.csr.pem"];
    assert!(
        pem.starts_with("-----BEGIN CERTIFICATE REQUEST-----"),
        "{pem}"
    );
    let spki = keys.keys()[0].subject_public_key_info();
    assert!(check_request(pem, &spki).is_ok());
    // Another key's request does not pass for this key.
    let Plan { keys: other, .. } = planned(CaKeys::default(), 7, 0);
    assert!(matches!(
        check_request(other.keys()[0].request_pem(), &spki),
        Err(KeyProblem::RequestMismatch)
    ));
}

#[test]
fn plans_add_only_for_a_higher_generation_and_remove_below_the_floor() {
    let Plan { keys, changes } = planned(CaKeys::default(), 1, 0);
    assert!(
        changes[0].starts_with("added issuing CA key"),
        "{changes:?}"
    );
    let first = keys.keys()[0].id().to_owned();

    // The same request again: nothing to do.
    let Plan { keys, changes } = planned(keys, 1, 0);
    assert!(changes.is_empty(), "{changes:?}");

    // A rotation adds generation 2 and keeps 1, whose gateway certificates are still valid.
    let Plan { keys, changes } = planned(keys, 2, 0);
    assert_eq!(changes.len(), 1, "{changes:?}");
    assert_eq!(keys.keys().len(), 2);
    assert_eq!(keys.keys()[0].id(), first);

    // A rollback of the values to generation 1 makes no key.
    let Plan { keys, changes } = planned(keys, 1, 0);
    assert!(changes.is_empty(), "{changes:?}");
    assert_eq!(keys.keys().len(), 2);

    // Retiring generation 1.
    let Plan { keys, changes } = planned(keys, 2, 2);
    assert_eq!(changes.len(), 1, "{changes:?}");
    assert!(changes[0].contains(&first), "{changes:?}");
    assert_eq!(
        keys.keys()
            .iter()
            .map(CaKey::generation)
            .collect::<Vec<_>>(),
        [2]
    );

    // A compromise: remove everything below 4 and make generation 4 in one run.
    let Plan { keys, changes } = planned(keys, 4, 4);
    assert_eq!(changes.len(), 2, "{changes:?}");
    assert_eq!(
        keys.keys()
            .iter()
            .map(CaKey::generation)
            .collect::<Vec<_>>(),
        [4]
    );
}

#[test]
fn plans_refuse_too_many_keys_and_requests_that_remove_themselves() {
    let mut keys = CaKeys::default();
    for generation in 1..=4 {
        keys = planned(keys, generation, 0).keys;
    }
    assert_eq!(keys.keys().len(), MAX_KEYS);
    let too_many = keys.plan(
        Request {
            generation: 5,
            remove_before: 0,
        },
        0,
        CaKey::generate,
    );
    assert!(matches!(too_many, Err(KeysError::TooMany)));

    let removes = CaKeys::default().plan(
        Request {
            generation: 2,
            remove_before: 3,
        },
        0,
        CaKey::generate,
    );
    assert!(matches!(removes, Err(KeysError::RemovesRequested { .. })));
    for generation in [0, MAX_GENERATION + 1] {
        let refused = CaKeys::default().plan(
            Request {
                generation,
                remove_before: 0,
            },
            0,
            CaKey::generate,
        );
        assert!(matches!(refused, Err(KeysError::Generation(_))));
    }
}

#[test]
fn unusable_documents_are_refused_without_their_values() {
    let Plan { keys, .. } = planned(CaKeys::default(), 1, 0);
    let Plan { keys: other, .. } = planned(CaKeys::default(), 2, 0);
    let good = document(&keys);
    let private_key = good["keys"][0]["private_key"]
        .as_str()
        .expect("a private key")
        .to_owned();
    let p384 = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P384_SHA384).expect("a P-384 key");

    type Breakage = (&'static str, Box<dyn Fn(&mut Value)>);
    let cases: Vec<Breakage> = vec![
        (
            "not a key set",
            Box::new(|document| document["keys"][0]["extra"] = json!(1)),
        ),
        (
            "generation is outside",
            Box::new(|document| document["keys"][0]["generation"] = json!(0)),
        ),
        (
            "creation time is negative",
            Box::new(|document| document["keys"][0]["created_at"] = json!(-1)),
        ),
        (
            "not base64",
            Box::new(|document| document["keys"][0]["private_key"] = json!("%%%")),
        ),
        (
            "not an ECDSA P-256",
            Box::new(move |document| {
                document["keys"][0]["private_key"] = json!(STANDARD.encode(p384.serialized_der()));
            }),
        ),
        (
            "not the SHA-256",
            Box::new(|document| document["keys"][0]["id"] = json!("0".repeat(64))),
        ),
        (
            "not one PEM certificate signing request",
            Box::new(|document| document["keys"][0]["request"] = json!("hello")),
        ),
        (
            "not signed by this key",
            Box::new({
                let request = other.keys()[0].request_pem().to_owned();
                move |document| document["keys"][0]["request"] = json!(request)
            }),
        ),
        (
            "same id",
            Box::new(|document| {
                let mut copy = document["keys"][0].clone();
                copy["generation"] = json!(9);
                document["keys"].as_array_mut().expect("keys").push(copy);
            }),
        ),
        (
            "more than 4 keys",
            Box::new(|document| {
                let copy = document["keys"][0].clone();
                let keys = document["keys"].as_array_mut().expect("keys");
                keys.extend(std::iter::repeat_n(copy, 4));
            }),
        ),
    ];
    for (expected, breakage) in cases {
        let mut broken = good.clone();
        breakage(&mut broken);
        let error = reparse(&broken).expect_err(expected);
        let text = error.to_string();
        assert!(text.contains(expected), "{expected}: {text}");
        assert!(!text.contains(&private_key), "{text}");
        assert!(!format!("{error:?}").contains(&private_key));
    }

    // Two keys of the same generation.
    let mut twice = good.clone();
    let mut second = document(&other)["keys"][0].clone();
    second["generation"] = json!(1);
    twice["keys"].as_array_mut().expect("keys").push(second);
    let error = reparse(&twice).expect_err("the same generation");
    assert!(error.to_string().contains("same generation"), "{error}");

    assert!(matches!(
        CaKeys::parse(&" ".repeat(MAX_DOCUMENT_BYTES + 1)),
        Err(KeysError::TooLarge)
    ));
    assert!(CaKeys::parse(r#"{"keys":[]}"#).expect("empty").is_empty());
}

#[test]
fn debug_never_shows_a_private_key() {
    let Plan { keys, changes } = planned(CaKeys::default(), 1, 0);
    let document = document(&keys);
    let private_key = document["keys"][0]["private_key"]
        .as_str()
        .expect("a private key")
        .to_owned();
    let shown = format!("{keys:?} {changes:?}");
    assert!(!shown.contains(&private_key), "{shown}");
    assert!(shown.contains(keys.keys()[0].id()), "{shown}");
    let key = keys.into_keys().remove(0);
    assert!(!format!("{key:?}").contains(&private_key));
    assert!(!format!("{:?}", key.into_signing_key()).contains(&private_key));
}
