//! The forward-only rule, the marker, the atomic writes and the lock shared with
//! `provision install`, on files in a scratch directory. The updater against an agent that
//! connects is in `tests/root_crl_update.rs`; `provision install` running alongside it, in
//! `tests/provision_install.rs`.

use std::os::unix::fs::PermissionsExt as _;

use testkit::mqtt::{Authority, ScratchDir};

use super::*;

/// A gateway's `broker.ca` with one root, and `broker.crl` in a state directory.
struct Gateway {
    dir: ScratchDir,
    root: Authority,
    trust: BrokerTrust,
    path: PathBuf,
}

impl Gateway {
    fn new(kind: &str) -> Self {
        let dir = ScratchDir::new(kind);
        let root = Authority::new("testkit root CA");
        let trust = BrokerTrust::load(&dir.write("broker-ca.pem", &root.pem)).unwrap();
        let state = dir.path().join("state");
        std::fs::create_dir(&state).unwrap();
        Self {
            path: state.join("root-crl.pem"),
            dir,
            root,
            trust,
        }
    }

    /// A root CRL of this gateway's root with `number`, revoking `revoked`.
    fn crl_revoking(&self, number: u64, revoked: &[&Authority]) -> RootCrl {
        let pem = self.root.crl(number, revoked, (2099, 1, 1));
        RootCrl::from_bytes(&self.path, pem.as_bytes(), &self.trust, UnixTime::now()).unwrap()
    }

    fn crl(&self, number: u64) -> RootCrl {
        self.crl_revoking(number, &[])
    }

    fn adopt(&self, held: &RootCrl, candidate: Option<&RootCrl>) -> Adoption {
        adopt(
            &self.path,
            &self.trust,
            held,
            candidate,
            UnixTime::now(),
            write_all,
        )
    }

    /// What a restarted agent loads.
    fn load(&self) -> RootCrl {
        load(Some(&self.path), &self.trust, UnixTime::now()).unwrap()
    }

    fn marker(&self) -> Marker {
        marker_of(read_marker(&marker_path(&self.path)))
    }

    /// The files in the state directory, sorted.
    fn listing(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(self.path.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    fn missing(&self) -> RootCrl {
        RootCrl::Missing(self.path.clone())
    }
}

fn number(crl: &RootCrl) -> Option<&str> {
    number_of(crl)
}

fn root_of(crl: &RootCrl) -> RootKey {
    match crl {
        RootCrl::Loaded { root, .. } => *root,
        other => panic!("not loaded: {other:?}"),
    }
}

fn pem_of(crl: &RootCrl) -> String {
    match crl {
        RootCrl::Loaded { der, .. } => install::pem(install::CRL, der),
        other => panic!("not loaded: {other:?}"),
    }
}

fn refused(decision: &Decision) -> &str {
    match &decision.verdict {
        Verdict::Refused(reason) => reason,
        other => panic!("not refused: {other:?}"),
    }
}

#[test]
fn the_marker_is_two_strict_fields_and_anything_else_is_damaged() {
    let key = RootKey::of_spki(b"a key");
    let text = Marker::text(&key, "42");
    assert_eq!(
        Marker::parse(text.as_bytes()),
        Marker::At {
            root: key,
            number: "42".to_owned()
        }
    );
    assert_eq!(RootKey::from_hex(&key.to_hex()), Some(key));
    for (what, bytes) in [
        ("empty", String::new()),
        ("not TOML", "number 42".to_owned()),
        ("an unknown field", format!("{text}extra = 1\n")),
        ("no number", text.replace("number = \"42\"\n", "")),
        ("a negative number", text.replace("\"42\"", "\"-1\"")),
        ("an empty number", text.replace("\"42\"", "\"\"")),
        ("a number as an integer", text.replace("\"42\"", "42")),
        (
            "50 digits",
            text.replace("\"42\"", &format!("\"{}\"", "9".repeat(50))),
        ),
        (
            "an uppercase key",
            text.replace(&key.to_hex(), &key.to_hex().to_uppercase()),
        ),
        ("a short key", text.replace(&key.to_hex(), "abcd")),
        ("too large", format!("{text}#{}\n", "x".repeat(1024))),
    ] {
        assert!(
            matches!(Marker::parse(bytes.as_bytes()), Marker::Damaged(_)),
            "{what}"
        );
    }
    assert!(matches!(Marker::parse(&[0xff, 0xfe]), Marker::Damaged(_)));
    // Never lower for the same root; another root's marker is replaced.
    let marker = Marker::At {
        root: key,
        number: "7".to_owned(),
    };
    assert_eq!(marker.raised(&key, "5"), "7");
    assert_eq!(marker.raised(&key, "12"), "12");
    assert_eq!(marker.raised(&RootKey::of_spki(b"another"), "5"), "5");
    assert_eq!(Marker::Absent.raised(&key, "5"), "5");
}

#[test]
fn a_root_crl_not_newer_than_the_one_held_is_refused_and_the_one_held_kept() {
    let gateway = Gateway::new("revocation-forward");
    let held = gateway.crl(5);
    // The one held is on disk, with its marker.
    let settled = gateway.adopt(&held, None);
    assert_eq!(settled.saved, Ok(()));
    assert_eq!(settled.decision.held, held);
    assert_eq!(
        std::fs::read_to_string(&gateway.path).unwrap(),
        pem_of(&held)
    );

    // Lower, or the same number with another CRL: refused, nothing written.
    let other_issuing = gateway.root.issuing("testkit issuing CA");
    for (what, candidate, reason) in [
        ("lower", gateway.crl(4), "is lower than 5"),
        (
            "the same number, another CRL",
            gateway.crl_revoking(5, &[&other_issuing]),
            "a CRL number names one CRL",
        ),
    ] {
        let before = std::fs::metadata(&gateway.path).unwrap();
        let adoption = gateway.adopt(&held, Some(&candidate));
        assert!(
            refused(&adoption.decision).contains(reason),
            "{what}: {adoption:?}"
        );
        assert_eq!(adoption.decision.held, held, "{what}");
        assert!(!adoption.decision.write_file, "{what}");
        assert_eq!(
            std::fs::metadata(&gateway.path).unwrap().ino(),
            before.ino(),
            "{what}: broker.crl was replaced"
        );
    }
    // The same CRL again: nothing to do.
    let again = gateway.adopt(&held, Some(&held));
    assert_eq!(again.decision.verdict, Verdict::Same);
    assert_eq!(again.decision.held, held);

    // Without a number, or with more digits than a CRL number has: refused.
    let RootCrl::Loaded {
        path,
        der,
        root,
        next_update,
        expired,
        ..
    } = gateway.crl(9)
    else {
        unreachable!()
    };
    for (number, reason) in [
        (None, "no CRL number"),
        (Some("9".repeat(50)), "more than 49 digits"),
    ] {
        let odd = RootCrl::Loaded {
            path: path.clone(),
            der: der.clone(),
            root,
            number,
            next_update,
            expired,
        };
        let adoption = gateway.adopt(&held, Some(&odd));
        assert!(refused(&adoption.decision).contains(reason), "{adoption:?}");
        assert_eq!(adoption.decision.held, held);
    }

    // Another trust anchor's root CRL (a root rollover with two roots in broker.ca): not the
    // root of the one held.
    let second = Authority::new("testkit second root CA");
    let both = BrokerTrust::load(
        &gateway
            .dir
            .write("both-ca.pem", format!("{}{}", gateway.root.pem, second.pem)),
    )
    .unwrap();
    let theirs = RootCrl::from_bytes(
        &gateway.path,
        second.crl(99, &[], (2099, 1, 1)).as_bytes(),
        &both,
        UnixTime::now(),
    )
    .unwrap();
    let decision = decide(
        &gateway.path,
        &both,
        &held,
        &OnDisk::Usable(held.clone()),
        &gateway.marker(),
        Some(&theirs),
    );
    assert!(refused(&decision).contains("another root"), "{decision:?}");
    assert_eq!(decision.held, held);
    // The same while nothing is held in memory and there is no marker yet (a root CRL from
    // before markers, not loaded yet): the file's root is the one that counts.
    let decision = decide(
        &gateway.path,
        &both,
        &gateway.missing(),
        &OnDisk::Usable(held.clone()),
        &Marker::Absent,
        Some(&theirs),
    );
    assert!(refused(&decision).contains("another root"), "{decision:?}");
    assert_eq!(decision.held, held);
    assert!(!decision.write_file);

    // Newer: taken, written, and the marker raised.
    let newer = gateway.crl(6);
    let adoption = gateway.adopt(&held, Some(&newer));
    assert_eq!(adoption.decision.verdict, Verdict::Taken);
    assert_eq!(adoption.decision.held, newer);
    assert_eq!(adoption.saved, Ok(()));
    assert_eq!(
        std::fs::read_to_string(&gateway.path).unwrap(),
        pem_of(&newer)
    );
    assert_eq!(
        gateway.marker(),
        Marker::At {
            root: root_of(&newer),
            number: "6".to_owned()
        }
    );
    let mode = std::fs::metadata(&gateway.path)
        .unwrap()
        .permissions()
        .mode()
        & 0o7777;
    assert_eq!(mode, install::FILE_MODE);
    assert_eq!(
        gateway.listing(),
        ["root-crl.pem", "root-crl.pem.highest"],
        "no staged file is left"
    );
}

/// Writes the first half of a new file, then fails: a power cut or a full disk mid-write.
fn half_then_fail(file: &mut File, bytes: &[u8]) -> io::Result<()> {
    file.write_all(bytes.get(..bytes.len() / 2).unwrap_or_default())?;
    Err(io::Error::other("the disk is full"))
}

#[test]
fn an_interrupted_write_leaves_the_last_saved_root_crl_which_a_restart_loads() {
    let gateway = Gateway::new("revocation-interrupted");
    let saved = gateway.crl(3);
    assert_eq!(
        gateway.adopt(&gateway.missing(), Some(&saved)).saved,
        Ok(())
    );
    assert_eq!(gateway.load(), saved, "a restart loads the root CRL saved");

    let newer = gateway.crl(4);
    let adoption = adopt(
        &gateway.path,
        &gateway.trust,
        &saved,
        Some(&newer),
        UnixTime::now(),
        half_then_fail,
    );
    // In use anyway, until the agent restarts; the error says why it is not saved.
    assert_eq!(adoption.decision.held, newer);
    let error = adoption.saved.unwrap_err();
    assert!(error.contains("the disk is full"), "{error}");
    // On disk: the old file whole, the old marker, no half-written file.
    assert_eq!(
        std::fs::read_to_string(&gateway.path).unwrap(),
        pem_of(&saved)
    );
    assert_eq!(
        gateway.marker(),
        Marker::At {
            root: root_of(&saved),
            number: "3".to_owned()
        }
    );
    assert_eq!(gateway.listing(), ["root-crl.pem", "root-crl.pem.highest"]);
    assert_eq!(gateway.load(), saved, "a restart loads the last saved one");

    // The next update saves it.
    let retried = gateway.adopt(&newer, None);
    assert_eq!(retried.saved, Ok(()));
    assert_eq!(gateway.load(), newer);
}

#[test]
fn once_a_root_crl_was_accepted_a_deleted_or_older_file_withholds_every_connection() {
    let gateway = Gateway::new("revocation-withheld");
    // Before any root CRL: none, and revocation is not checked.
    assert_eq!(gateway.load(), gateway.missing());
    // A root CRL from before the marker existed (an older agent): loaded as it is; the updater
    // writes the marker.
    let first = gateway.crl(2);
    std::fs::write(&gateway.path, pem_of(&first)).unwrap();
    assert_eq!(gateway.load(), first);
    assert_eq!(gateway.marker(), Marker::Absent);
    assert_eq!(gateway.adopt(&first, None).saved, Ok(()));
    let accepted = gateway.crl(3);
    assert_eq!(gateway.adopt(&first, Some(&accepted)).saved, Ok(()));
    let withheld = RootCrl::Withheld {
        path: gateway.path.clone(),
        root: Some(root_of(&accepted)),
        number: Some("3".to_owned()),
    };

    // Deleted: withheld, not "no root CRL yet".
    std::fs::remove_file(&gateway.path).unwrap();
    assert_eq!(gateway.load(), withheld);
    // Replaced by an older one: withheld.
    std::fs::write(&gateway.path, pem_of(&first)).unwrap();
    assert_eq!(gateway.load(), withheld);
    // And the updater does not write the older one over it, nor take it.
    let settled = gateway.adopt(&withheld, None);
    assert_eq!(settled.decision.held, withheld);
    assert!(!settled.decision.write_file);
    assert!(refused(&gateway.adopt(&withheld, Some(&first)).decision).contains("lower than 3"));
    // At least the number accepted ends it: the same number (a re-fetch of the one accepted) or a
    // newer one.
    let again = gateway.crl(3);
    let adoption = gateway.adopt(&withheld, Some(&again));
    assert_eq!(adoption.decision.held, again);
    assert_eq!(adoption.saved, Ok(()));
    assert_eq!(gateway.load(), again);

    // The marker damaged and the file gone: withheld, number unknown; any root CRL of the trust
    // anchors ends it.
    std::fs::write(marker_path(&gateway.path), "garbage").unwrap();
    std::fs::remove_file(&gateway.path).unwrap();
    let unknown = gateway.load();
    assert_eq!(
        unknown,
        RootCrl::Withheld {
            path: gateway.path.clone(),
            root: None,
            number: None
        }
    );
    let adoption = gateway.adopt(&unknown, Some(&first));
    assert_eq!(adoption.decision.held, first);
    assert_eq!(gateway.marker().raised(&root_of(&first), "0"), "2");

    // A stale marker of a root no longer in broker.ca (an install changed the root without a
    // root CRL) and no broker.crl: ignored at start. broker.ca still holds the roots in use, so
    // the marker is stale for certain: a fetched root CRL is saved and the marker replaced
    // (Laysi/trustiot-v2#926; before, nothing was ever saved until `provision install` ran
    // again), a restart loads it, and the forward-only rule holds from then on.
    let old_root = RootKey::of_spki(b"the old root");
    std::fs::write(marker_path(&gateway.path), Marker::text(&old_root, "50")).unwrap();
    std::fs::remove_file(&gateway.path).unwrap();
    assert_eq!(gateway.load(), gateway.missing());
    let adoption = gateway.adopt(&gateway.missing(), Some(&first));
    assert_eq!(adoption.decision.held, first, "used in memory");
    assert_eq!(adoption.saved, Ok(()));
    assert_eq!(gateway.load(), first, "saved");
    assert_eq!(
        gateway.marker(),
        Marker::At {
            root: root_of(&first),
            number: "2".to_owned()
        }
    );
    assert!(
        refused(&gateway.adopt(&first, Some(&gateway.crl(1))).decision).contains("lower than 2")
    );
    std::fs::remove_file(&gateway.path).unwrap();
    assert!(
        matches!(gateway.load(), RootCrl::Withheld { number: Some(number), .. } if number == "2"),
        "deleted again, it withholds"
    );

    // The same foreign marker next to a root CRL of this root (a power cut between the two renames
    // of a root change): the marker is stale, the updater replaces it, and a later deletion of
    // broker.crl withholds again.
    std::fs::write(marker_path(&gateway.path), Marker::text(&old_root, "50")).unwrap();
    std::fs::write(&gateway.path, pem_of(&accepted)).unwrap();
    assert_eq!(gateway.load(), accepted);
    let adoption = gateway.adopt(&accepted, None);
    assert_eq!(adoption.saved, Ok(()));
    assert_eq!(
        gateway.marker(),
        Marker::At {
            root: root_of(&accepted),
            number: "3".to_owned()
        }
    );
    std::fs::remove_file(&gateway.path).unwrap();
    assert_eq!(gateway.load(), withheld);
}

#[test]
fn the_directory_check_refuses_a_directory_the_agent_cannot_write_and_changes_nothing() {
    let gateway = Gateway::new("revocation-directory");
    let state = gateway.path.parent().unwrap().to_owned();
    assert_eq!(check_directory(&gateway.path), Ok(()));
    let missing = gateway.dir.path().join("missing").join("root-crl.pem");
    let error = check_directory(&missing).unwrap_err();
    assert!(error.contains("cannot be opened"), "{error}");
    assert!(!missing.parent().unwrap().exists(), "created");

    std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o555)).unwrap();
    let checked = check_directory(&gateway.path);
    let mode = std::fs::metadata(&state).unwrap().permissions().mode() & 0o7777;
    std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(mode, 0o555, "the check changed the mode");
    if std::fs::metadata(&state).unwrap().uid() == 0 {
        // Root may write anywhere on a writable file system; the agent never runs as root.
        assert_eq!(checked, Ok(()));
    } else {
        let error = checked.unwrap_err();
        assert!(
            error.contains("not writable by this account") && error.contains("Permission denied"),
            "{error}"
        );
    }
    assert_eq!(gateway.listing(), Vec::<String>::new(), "created a file");
}

#[test]
fn nothing_is_written_while_broker_ca_holds_other_roots_than_the_ones_in_use() {
    // The agent runs with the gateway's root; `provision install` then put another root in
    // broker.ca without a root CRL (broker.crl was gone, so it could). A root CRL of the old root
    // written now would keep the agent from starting under the new root.
    let gateway = Gateway::new("revocation-root-replaced");
    let held = gateway.crl(2);
    assert_eq!(gateway.adopt(&gateway.missing(), Some(&held)).saved, Ok(()));
    std::fs::remove_file(&gateway.path).unwrap();
    let marker_before = std::fs::read(marker_path(&gateway.path)).unwrap();
    let new_root = Authority::new("testkit new root CRL root");
    gateway.dir.write("broker-ca.pem", &new_root.pem);

    let adoption = gateway.adopt(&held, Some(&gateway.crl(3)));
    assert_eq!(adoption.decision.verdict, Verdict::Taken);
    assert!(
        matches!(&adoption.decision.held, RootCrl::Loaded { number: Some(number), .. } if number == "3"),
        "used in memory, under the roots in use: {adoption:?}"
    );
    assert!(!adoption.decision.write_file && adoption.decision.write_marker.is_none());
    let error = adoption.saved.unwrap_err();
    assert!(error.contains("other roots"), "{error}");
    assert!(!gateway.path.exists(), "broker.crl was written");
    assert_eq!(
        std::fs::read(marker_path(&gateway.path)).unwrap(),
        marker_before,
        "the marker was written"
    );
    // An unreadable broker.ca confirms nothing either.
    gateway.dir.write("broker-ca.pem", "not a certificate\n");
    let adoption = gateway.adopt(&held, Some(&gateway.crl(3)));
    let error = adoption.saved.unwrap_err();
    assert!(error.contains("cannot be confirmed"), "{error}");
    assert!(!gateway.path.exists(), "broker.crl was written");
    // broker.ca back to the roots in use: written again.
    gateway.dir.write("broker-ca.pem", &gateway.root.pem);
    assert_eq!(gateway.adopt(&held, Some(&gateway.crl(3))).saved, Ok(()));
    assert_eq!(number(&gateway.load()), Some("3"));
}

#[test]
fn a_newer_root_crl_written_by_provision_install_is_taken_over_not_overwritten() {
    let gateway = Gateway::new("revocation-install");
    let held = gateway.crl(2);
    assert_eq!(gateway.adopt(&gateway.missing(), Some(&held)).saved, Ok(()));
    // provision install wrote root CRL 7 (and its marker) while this agent held 2.
    let installed = gateway.crl(7);
    std::fs::write(&gateway.path, pem_of(&installed)).unwrap();
    std::fs::write(
        marker_path(&gateway.path),
        Marker::text(&root_of(&installed), "7"),
    )
    .unwrap();
    // A fetched 5 is refused; 7 is what the agent holds from now on.
    let adoption = gateway.adopt(&held, Some(&gateway.crl(5)));
    assert!(refused(&adoption.decision).contains("lower than 7"));
    assert_eq!(adoption.decision.held, installed);
    assert!(!adoption.decision.write_file);
    assert_eq!(
        std::fs::read_to_string(&gateway.path).unwrap(),
        pem_of(&installed)
    );
}

#[test]
fn a_power_cut_between_the_two_renames_of_a_root_change_leaves_a_marker_that_does_not_count() {
    // broker.ca with two roots; root CRL 50 of the first was accepted, then the second root's
    // root CRL 1 replaced broker.crl and the power went before the marker's rename.
    let gateway = Gateway::new("revocation-root-change");
    let first = gateway.crl(50);
    assert_eq!(
        gateway.adopt(&gateway.missing(), Some(&first)).saved,
        Ok(())
    );
    let second = Authority::new("testkit second root CA");
    let both = BrokerTrust::load(
        &gateway
            .dir
            .write("both-ca.pem", format!("{}{}", gateway.root.pem, second.pem)),
    )
    .unwrap();
    let theirs = |number: u64| {
        RootCrl::from_bytes(
            &gateway.path,
            second.crl(number, &[], (2099, 1, 1)).as_bytes(),
            &both,
            UnixTime::now(),
        )
        .unwrap()
    };
    let installed = theirs(1);
    std::fs::write(&gateway.path, pem_of(&installed)).unwrap();

    // Not withheld: the second root's root CRL loads, the marker is rewritten for it, and the
    // second root's newer root CRLs are taken from then on.
    let loaded = load(Some(&gateway.path), &both, UnixTime::now()).unwrap();
    assert_eq!(loaded, installed);
    let settled = adopt(
        &gateway.path,
        &both,
        &loaded,
        None,
        UnixTime::now(),
        write_all,
    );
    assert_eq!(settled.saved, Ok(()));
    assert_eq!(
        gateway.marker(),
        Marker::At {
            root: root_of(&loaded),
            number: "1".to_owned()
        }
    );
    let newer = theirs(2);
    let adoption = adopt(
        &gateway.path,
        &both,
        &loaded,
        Some(&newer),
        UnixTime::now(),
        write_all,
    );
    assert_eq!(adoption.decision.verdict, Verdict::Taken);
    assert_eq!(adoption.saved, Ok(()));
    // And the rollback guard works for the new root: deleted, it withholds.
    std::fs::remove_file(&gateway.path).unwrap();
    assert!(matches!(
        load(Some(&gateway.path), &both, UnixTime::now()).unwrap(),
        RootCrl::Withheld { number: Some(number), .. } if number == "2"
    ));
}

/// A source answering what the test sets.
struct Answers(std::sync::Mutex<io::Result<Fetched>>);

impl Answers {
    fn set(&self, answer: io::Result<Fetched>) {
        *self.0.lock().unwrap() = answer;
    }
}

impl Source for &Answers {
    async fn fetch(&self) -> io::Result<Fetched> {
        match &*self.0.lock().unwrap() {
            Ok(fetched) => Ok(fetched.clone()),
            Err(error) => Err(io::Error::new(error.kind(), error.to_string())),
        }
    }

    fn describe(&self) -> String {
        "the stand-in".to_owned()
    }
}

#[tokio::test]
async fn a_failed_fetch_still_takes_over_a_newer_file_and_saves_what_is_held() {
    let gateway = Gateway::new("revocation-failed-fetch");
    let held = gateway.crl(2);
    assert_eq!(gateway.adopt(&gateway.missing(), Some(&held)).saved, Ok(()));
    let leaf = gateway.root.client("tg-revocation-1");
    let certificate = gateway.dir.write("gateway.pem", &leaf.cert_pem);
    let key = gateway.dir.write("gateway.key", &leaf.key_pem);
    let identity = crate::identity::load(
        &certificate,
        &crate::identity::KeySource::File { path: key },
    )
    .unwrap();
    let inner = InnerConfig::new(gateway.trust.clone(), held.clone(), identity).unwrap();
    let answers = Answers(std::sync::Mutex::new(Ok(Fetched::NotPublished)));
    let in_use =
        |updater: &Updater<&Answers>| number(&updater.inner().root_crl()).map(str::to_owned);

    // provision install wrote root CRL 3 while the agent held 2; the cloud cannot be reached, then
    // answers 503: the agent takes 3 over anyway, and the next connection is a new one.
    let updater = Updater::new(&answers, gateway.path.clone(), inner.clone());
    let installed = gateway.crl(3);
    std::fs::write(&gateway.path, pem_of(&installed)).unwrap();
    answers.set(Err(io::Error::other("unreachable")));
    let version = inner.version();
    let error = updater.update().await.unwrap_err();
    assert!(error.contains("unreachable"), "{error}");
    assert_eq!(in_use(&updater).as_deref(), Some("3"));
    assert!(inner.version() > version, "the inner TLS settings are new");
    answers.set(Ok(Fetched::Status(503)));
    assert!(updater.update().await.unwrap_err().contains("HTTP 503"));
    assert_eq!(in_use(&updater).as_deref(), Some("3"));
    assert_eq!(gateway.marker().raised(&root_of(&installed), "0"), "3");

    // Root CRL 4 fetched but not saved (the disk is full): in use, and the error says so. The
    // next answers are 404 and a refused CRL, and each of them saves 4 again; the first that
    // succeeds leaves it on disk.
    let updater = updater.with_writer(half_then_fail);
    answers.set(Ok(Fetched::Crl(
        gateway.root.crl(4, &[], (2099, 1, 1)).into_bytes(),
    )));
    let error = updater.update().await.unwrap_err();
    assert!(error.contains("cannot be saved"), "{error}");
    assert_eq!(in_use(&updater).as_deref(), Some("4"));
    assert_eq!(number(&gateway.load()), Some("3"));
    answers.set(Ok(Fetched::NotPublished));
    let error = updater.update().await.unwrap_err();
    assert!(
        error.contains("cannot be saved"),
        "a 404 saves again: {error}"
    );
    let updater = updater.with_writer(write_all);
    assert_eq!(updater.update().await, Ok(Update::NotPublished));
    assert_eq!(number(&gateway.load()), Some("4"));
    answers.set(Ok(Fetched::Crl(b"not a CRL".to_vec())));
    assert!(matches!(updater.update().await, Ok(Update::Refused(_))));
    assert_eq!(in_use(&updater).as_deref(), Some("4"));
}

/// Whether some process waits for a `flock` on the file with inode `inode` (`/proc/locks`).
fn flock_waiter_on(inode: u64) -> bool {
    std::fs::read_to_string("/proc/locks")
        .unwrap_or_default()
        .lines()
        .any(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            fields.get(1) == Some(&"->")
                && fields.get(2) == Some(&"FLOCK")
                && fields
                    .get(6)
                    .and_then(|id| id.rsplit(':').next())
                    .is_some_and(|number| number == inode.to_string())
        })
}

/// How long a test waits for the other side to wait for the lock: generous for a loaded host.
const LOCK_WAIT_BOUND: Duration = Duration::from_secs(300);

#[test]
fn the_files_are_read_and_written_only_under_the_lock_provision_install_takes() {
    // Another holder of the directory's lock (as `provision install` holds it while it plans and
    // renames) writes root CRL 8 while the agent waits to take 6: the agent must see 8 and keep
    // it, not rename 6 over it.
    let gateway = Gateway::new("revocation-lock");
    let held = gateway.crl(2);
    assert_eq!(gateway.adopt(&gateway.missing(), Some(&held)).saved, Ok(()));
    let state = gateway.path.parent().unwrap().to_owned();
    let lock = File::open(&state).unwrap();
    lock.lock().unwrap();
    let candidate = gateway.crl(6);
    let path = gateway.path.clone();
    let trust = gateway.trust.clone();
    let waiting_held = held.clone();
    let agent = std::thread::spawn(move || {
        adopt(
            &path,
            &trust,
            &waiting_held,
            Some(&candidate),
            UnixTime::now(),
            write_all,
        )
    });
    let inode = std::fs::metadata(&state).unwrap().ino();
    let started = std::time::Instant::now();
    while !flock_waiter_on(inode) {
        assert!(
            started.elapsed() < LOCK_WAIT_BOUND,
            "the agent never waited for the lock of the state directory"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let installed = gateway.crl(8);
    let staged = state.join(".root-crl-8");
    std::fs::write(&staged, pem_of(&installed)).unwrap();
    std::fs::rename(&staged, &gateway.path).unwrap();
    drop(lock);
    let adoption = agent.join().unwrap();
    assert!(
        refused(&adoption.decision).contains("lower than 8"),
        "{adoption:?}"
    );
    assert_eq!(adoption.decision.held, installed);
    assert_eq!(
        std::fs::read_to_string(&gateway.path).unwrap(),
        pem_of(&installed),
        "root CRL 8, written while the agent waited, is kept"
    );
    // Its marker is raised to 8 by the agent, since the install in this test wrote none.
    assert_eq!(
        gateway.marker(),
        Marker::At {
            root: root_of(&installed),
            number: "8".to_owned()
        }
    );
    assert_eq!(number(&gateway.load()), Some("8"));
}
