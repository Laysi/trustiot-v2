//! The password file: what is accepted, what is refused, and that neither the password type nor
//! a refusal prints the password. No broker needed.

#![allow(
    clippy::unwrap_used,
    reason = "tests unwrap (clippy.toml allows it in tests)"
)]

mod support;

use mqtt_client::{Client, MAX_PASSWORD_BYTES, Password, PasswordError, StartError};
use support::{Scratch, random_password};

#[test]
fn a_final_line_break_is_not_part_of_the_password() {
    let scratch = Scratch::new();
    let password = random_password();
    for content in [
        password.clone(),
        format!("{password}\n"),
        format!("{password}\r\n"),
    ] {
        let read = Password::read(&scratch.write("password", &content)).unwrap();
        assert_eq!(format!("{read:?}"), "Password(<redacted>)");
    }
}

#[test]
fn unusable_files_are_refused_without_their_content() {
    let scratch = Scratch::new();
    let secret = random_password();
    let cases = [
        (String::new(), PasswordError::Empty),
        ("\n".to_owned(), PasswordError::Empty),
        ("x".repeat(MAX_PASSWORD_BYTES + 1), PasswordError::TooLong),
        (format!("{secret} {secret}"), PasswordError::Characters),
        (format!("{secret}\n\n"), PasswordError::Characters),
        (format!("{secret}\u{e9}"), PasswordError::Characters),
    ];
    for (content, expected) in cases {
        let error = Password::read(&scratch.write("password", &content)).unwrap_err();
        assert_eq!(error, expected);
        let shown = format!("{error} {error:?}");
        let piece: String = secret.chars().take(8).collect();
        assert!(!shown.contains(&piece), "{shown}");
    }
    assert_eq!(
        Password::read(&scratch.write("password", "ok").with_extension("missing")).unwrap_err(),
        PasswordError::Unreadable(std::io::ErrorKind::NotFound)
    );
    // A file just at the limit is accepted.
    Password::read(&scratch.write("password", &"x".repeat(MAX_PASSWORD_BYTES))).unwrap();
}

#[tokio::test]
async fn start_refuses_an_unusable_password_file_and_bad_subscriptions() {
    let scratch = Scratch::new();
    let config = mqtt_client::Config {
        host: "127.0.0.1".to_owned(),
        port: 1,
        username: "service:test".to_owned(),
        password_file: scratch.write("password", ""),
    };
    assert!(matches!(
        Client::start(&config, Vec::new()),
        Err(StartError::Password(PasswordError::Empty))
    ));
    let config = mqtt_client::Config {
        password_file: scratch.write("password", "secret"),
        ..config
    };
    assert!(matches!(
        Client::start(&config, vec!["ok/#".to_owned(), "bad/#/x".to_owned()]),
        Err(StartError::Subscription { index: 1 })
    ));
    let bad_name = mqtt_client::Config {
        username: "gw-a".to_owned(),
        ..config
    };
    assert!(matches!(
        Client::start(&bad_name, Vec::new()),
        Err(StartError::Config(mqtt_client::ConfigError::Username))
    ));
}
