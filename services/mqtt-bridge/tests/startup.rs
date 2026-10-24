//! The bridge refuses to start rather than record a source IP it cannot trust: an unknown
//! source-IP mode, the on-prem mode it does not implement yet (Laysi/trustiot-v2#446), no mode at
//! all, or no broker address all end the process with exit code 1 before it listens.

mod support;

use support::{Bridge, CF};

/// Starts the binary with `vars` and expects it to refuse: exit code 1, no listening port, and an
/// `invalid configuration` line whose error names `variable`. Returns that error text.
async fn refuses(vars: &[(&str, &str)], variable: &str) -> String {
    let mut bridge = Bridge::spawn(vars);
    let code = bridge.wait_exit().await;
    assert_eq!(code, 1, "refused to start:\n{}", bridge.dump());
    assert!(
        bridge.messages("listening").is_empty(),
        "never listened:\n{}",
        bridge.dump()
    );
    let refused = bridge.messages("invalid configuration; not starting");
    assert_eq!(refused.len(), 1, "{}", bridge.dump());
    let error = refused[0]["fields"]["error"]
        .as_str()
        .expect("the error text")
        .to_owned();
    assert!(error.contains(variable), "names {variable}: {error}");
    error
}

#[tokio::test]
async fn an_unknown_source_ip_mode_refuses_to_start() {
    // Header names a deployment might wrongly expect to work, in any case.
    for mode in [
        "x-forwarded-for",
        "true-client-ip",
        "forwarded",
        "CF-Connecting-IP",
        "none",
        "",
    ] {
        let error = refuses(
            &[
                ("TRUSTIOT_CLIENT_IP_SOURCE", mode),
                ("TRUSTIOT__BROKER__ADDRESS", "127.0.0.1:8883"),
            ],
            "TRUSTIOT_CLIENT_IP_SOURCE",
        )
        .await;
        if !mode.is_empty() {
            assert!(!error.contains(mode), "the value is not echoed: {error}");
        }
    }
}

#[tokio::test]
async fn the_on_prem_mode_refuses_to_start_until_it_is_implemented() {
    let error = refuses(
        &[
            ("TRUSTIOT_CLIENT_IP_SOURCE", "x-forwarded-for-rightmost"),
            ("TRUSTIOT__BROKER__ADDRESS", "127.0.0.1:8883"),
        ],
        "TRUSTIOT_CLIENT_IP_SOURCE",
    )
    .await;
    assert!(error.contains("Laysi/trustiot-v2#446"), "{error}");
}

#[tokio::test]
async fn a_missing_mode_or_broker_address_refuses_to_start() {
    refuses(
        &[("TRUSTIOT__BROKER__ADDRESS", "127.0.0.1:8883")],
        "TRUSTIOT_CLIENT_IP_SOURCE",
    )
    .await;
    refuses(
        &[("TRUSTIOT_CLIENT_IP_SOURCE", CF)],
        "TRUSTIOT__BROKER__ADDRESS",
    )
    .await;
}
