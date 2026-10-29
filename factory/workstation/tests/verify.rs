//! `verify` against the event log of the registration stand-in (Laysi/trustiot-v2#616): it
//! finds the `certs.gateway_registered` event of a gateway registered through the fake ssh,
//! following `next_cursor` and starting an hour before the record; it ends with 24 when there is
//! none, 25 when an event does not match the record and 26 when there is no record to compare
//! with; the platform's own 401 ends the session. Also: the README's operator steps and the
//! tool's `--help` name the same commands, options and exit codes.

#![cfg(target_os = "linux")]

mod support;

use support::bench::Bench;
use support::ssh::{CANNED_AGENT, GatewayKeys};
use support::{Environment, Scratch, run};
use workstation::config::Origin;
use workstation::secret::SessionCookie;
use workstation::session::{Session, SessionStore};
use workstation::{EVENT_MISMATCH_EXIT_CODE, NO_EVENT_EXIT_CODE, UNRECORDED_EXIT_CODE};

/// Registers `hardware_id` through the fake ssh with the canned agent.
async fn registered(bench: &Bench, hardware_id: &str) -> GatewayKeys {
    let keys = GatewayKeys::new();
    bench.ssh.reset_remote();
    bench.ssh.leave_reports(
        &keys.report(hardware_id, None),
        &keys.report(hardware_id, Some(true)),
    );
    bench.ssh.install_remote("trustiot-agent", CANNED_AGENT);
    let run = bench.run(&bench.register_args(hardware_id, &[])).await;
    assert_eq!(run.code, Some(0), "{}", run.output());
    keys
}

#[tokio::test]
async fn verify_shows_the_registration_event_of_the_gateway() {
    let bench = Bench::start("verify-found", Vec::new()).await;
    let keys = registered(&bench, "tg-0101").await;
    let _ = registered(&bench, "tg-0102").await;
    // Newer events of other types: the reader has to follow next_cursor past them.
    bench.registry.add_other_events(3);

    let run = bench.run(&bench.verify_args("tg-0101")).await;
    assert_eq!(run.code, Some(0), "{}", run.output());
    assert!(
        run.stdout.contains("certs.gateway_registered at "),
        "{}",
        run.stdout
    );
    assert!(run.stdout.contains("hardware ID tg-0101"), "{}", run.stdout);
    assert!(!run.stdout.contains("tg-0102"), "{}", run.stdout);
    assert!(
        run.stdout.contains(&keys.public_key_sha256()),
        "{}",
        run.stdout
    );
    assert!(
        run.stdout
            .contains("serial 4f3a9c1e2b7d6058a1c3e5f708192a3b"),
        "{}",
        run.stdout
    );
    assert!(
        run.stdout
            .contains("it matches this workstation's record (serial and public key)"),
        "{}",
        run.stdout
    );

    let queries = bench.registry.event_log_queries();
    assert!(queries.len() > 1, "it followed next_cursor: {queries:?}");
    for (number, query) in queries.iter().enumerate() {
        assert!(query.starts_with("limit=200&"), "{query}");
        // The record's registered_at (2026-10-24T08:00:00Z) less an hour.
        assert!(query.contains("from=2026-10-24T07%3A00%3A00Z"), "{query}");
        assert_eq!(
            query.contains("cursor="),
            number > 0,
            "only later pages carry a cursor: {query}"
        );
    }
}

#[tokio::test]
async fn verify_without_an_event_ends_with_24() {
    let bench = Bench::start("verify-missing", Vec::new()).await;
    let _ = registered(&bench, "tg-0103").await;
    let run = bench.run(&bench.verify_args("tg-0199")).await;
    assert_eq!(
        run.code,
        Some(i32::from(NO_EVENT_EXIT_CODE)),
        "{}",
        run.output()
    );
    assert!(
        run.stderr
            .contains("no certs.gateway_registered event of hardware ID tg-0199"),
        "{}",
        run.stderr
    );
    // Without a record of the hardware ID the whole log is read.
    for query in bench.registry.event_log_queries() {
        assert!(!query.contains("from="), "{query}");
    }
}

#[tokio::test]
async fn verify_tells_a_mismatched_event_and_a_missing_record_apart() {
    let bench = Bench::start("verify-compare", Vec::new()).await;
    let _ = registered(&bench, "tg-0105").await;
    let _ = registered(&bench, "tg-0106").await;
    let journal = bench.journal();

    // The record names another serial than the platform's event.
    let mut record = journal
        .load("tg-0105")
        .expect("readable")
        .expect("a record");
    record.registration.as_mut().expect("a registration").serial = "0123456789abcdef".to_owned();
    journal.write(&record).expect("rewritten");
    let run = bench.run(&bench.verify_args("tg-0105")).await;
    assert_eq!(
        run.code,
        Some(i32::from(EVENT_MISMATCH_EXIT_CODE)),
        "{}",
        run.output()
    );
    assert!(
        run.stderr
            .contains("does not match this workstation's record"),
        "{}",
        run.stderr
    );
    assert!(run.stdout.contains("hardware ID tg-0105"), "{}", run.stdout);

    // No record of the hardware ID on this workstation: the event is shown, not compared.
    std::fs::remove_file(journal.record_path("tg-0106")).expect("the record removed");
    let run = bench.run(&bench.verify_args("tg-0106")).await;
    assert_eq!(
        run.code,
        Some(i32::from(UNRECORDED_EXIT_CODE)),
        "{}",
        run.output()
    );
    assert!(run.stdout.contains("hardware ID tg-0106"), "{}", run.stdout);
    assert!(
        run.stderr
            .contains("has no registered record of hardware ID tg-0106"),
        "{}",
        run.stderr
    );
}

#[tokio::test]
async fn verify_with_an_ended_session_forgets_it() {
    let bench = Bench::start("verify-ended", Vec::new()).await;
    let _ = registered(&bench, "tg-0104").await;
    let store = SessionStore::new(
        bench
            .environment
            .runtime
            .as_ref()
            .expect("a runtime directory")
            .join("trustiot-workstation"),
    );
    store
        .save(&Session {
            origin: Origin::parse(&bench.registry.url).expect("a loopback origin"),
            cookie: SessionCookie::parse(&support::FakeSecrets::new().cookie).expect("a cookie"),
        })
        .expect("another session saved");
    let run = bench.run(&bench.verify_args("tg-0104")).await;
    assert_eq!(run.code, Some(1), "{}", run.output());
    assert!(run.stderr.contains("log in again"), "{}", run.stderr);
    assert!(
        !bench.environment.session_file().exists(),
        "the ended session's file is removed"
    );
    assert_eq!(bench.registry.logouts(), 1);
}

/// The words of the usage text that start with `--`, and the commands.
fn usage_words(usage: &str) -> (Vec<String>, Vec<String>) {
    let mut options: Vec<String> = usage
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
        .filter(|word| word.starts_with("--") && word.len() > 2)
        .map(str::to_owned)
        .collect();
    options.sort();
    options.dedup();
    let mut commands: Vec<String> = usage
        .lines()
        .filter_map(|line| line.trim().strip_prefix("trustiot-workstation "))
        .filter_map(|rest| rest.split_whitespace().next())
        .map(str::to_owned)
        .collect();
    commands.sort();
    commands.dedup();
    (options, commands)
}

#[tokio::test]
async fn the_readme_and_the_help_agree() {
    let scratch = Scratch::new("readme");
    let environment = Environment::in_scratch(&scratch);
    let help = run(&environment, &["--help"], "").await;
    assert_eq!(help.code, Some(0), "{}", help.output());
    let readme = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/README.md"))
        .expect("the README");
    let (options, commands) = usage_words(&help.stdout);
    assert!(options.contains(&"--gateway".to_owned()), "{options:?}");
    assert!(commands.contains(&"verify".to_owned()), "{commands:?}");
    for option in &options {
        assert!(
            readme.contains(&format!("`{option}")) || readme.contains(&format!(" {option} ")),
            "the README does not explain {option}"
        );
    }
    for command in &commands {
        assert!(
            readme.contains(&format!("trustiot-workstation {command}")),
            "the README has no step with `trustiot-workstation {command}`"
        );
    }
    // Every exit code of the tool has a row in the README's table.
    for code in [
        0, 1, 2, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 75,
    ] {
        assert!(
            readme.contains(&format!("\n| {code} |")),
            "the README's exit code table has no row for {code}"
        );
    }
}
