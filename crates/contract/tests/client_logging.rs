//! A failed internal call is logged with the peer service and the kind of failure, and never
//! with the token. A test binary of its own, so no other test shares the capturing subscriber.

mod common;

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::peer::{Reply, serve_once};
use contract::client::{Endpoint, InternalClient, Method};
use contract::{InternalToken, service};
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Level, Metadata, Subscriber};

const TOKEN: &str = "secret-token-value.abc.def";

struct GetTenant;

impl Endpoint for GetTenant {
    type Service = service::Identity;
    type Body = ();
    type Response = ();
    const METHOD: Method = Method::Get;
    const ROUTE: &'static str = "/internal/tenants/{tenant_id}";

    fn path_params(&self) -> Vec<String> {
        vec!["01928f6e-7a3c-7d4e-8f00-00000000000a".to_owned()]
    }
}

#[derive(Debug)]
struct Captured {
    level: Level,
    fields: BTreeMap<&'static str, String>,
}

/// Records every event with its fields as text.
#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<Captured>>>);

impl Subscriber for Capture {
    fn enabled(&self, _: &Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, _: &Attributes<'_>) -> Id {
        Id::from_u64(1)
    }

    fn record(&self, _: &Id, _: &Record<'_>) {}

    fn record_follows_from(&self, _: &Id, _: &Id) {}

    fn event(&self, event: &Event<'_>) {
        let mut fields = Fields::default();
        event.record(&mut fields);
        self.0.lock().expect("capture lock").push(Captured {
            level: *event.metadata().level(),
            fields: fields.0,
        });
    }

    fn enter(&self, _: &Id) {}

    fn exit(&self, _: &Id) {}
}

#[derive(Default)]
struct Fields(BTreeMap<&'static str, String>);

impl Visit for Fields {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.insert(field.name(), value.to_owned());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.0.insert(field.name(), format!("{value:?}"));
    }
}

#[tokio::test]
async fn failures_are_logged_with_peer_and_kind_but_not_the_token() {
    let capture = Capture::default();
    let _guard = tracing::subscriber::set_default(capture.clone());

    let (base_url, _request) = serve_once(Reply::Reset).await;
    let client: InternalClient<service::Identity> =
        InternalClient::new(&base_url, Duration::from_secs(5)).expect("client");
    let token = InternalToken::new(TOKEN).expect("token");
    let error = client.call(&GetTenant, &token).await.expect_err("reset");

    let events = capture.0.lock().expect("capture lock");
    let failure = events
        .iter()
        .find(|event| event.fields.get("route").map(String::as_str) == Some(GetTenant::ROUTE))
        .unwrap_or_else(|| panic!("no failure event among {events:#?}"));
    assert_eq!(failure.level, Level::WARN);
    assert_eq!(failure.fields["peer"], "identity");
    assert_eq!(failure.fields["kind"], "connection_reset");
    assert_eq!(failure.fields["code"], "unavailable");
    assert_eq!(failure.fields["timeout_ms"], "5000");
    assert!(
        failure.fields["message"].contains("AuthorizationPolicy"),
        "{failure:?}"
    );
    assert_eq!(failure.fields["message"], error.to_string());

    for event in events.iter() {
        for value in event.fields.values() {
            assert!(!value.contains(TOKEN), "token logged in {event:?}");
            assert!(
                !value.contains("01928f6e"),
                "path parameter logged in {event:?}"
            );
        }
    }
}
