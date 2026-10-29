//! `verify` (Laysi/trustiot-v2#616): the evidence that the platform sees a registered gateway.
//! It reads the platform-wide event log, `GET /api/identity/v1/platform/event-log`
//! (services/identity/README.md, "事件層與操作紀錄"; platform administrators only), newest first,
//! and returns the `certs.gateway_registered` events of one hardware ID: the certs service writes
//! one when it registers a gateway, and none for a repeated registration.
//!
//! When this workstation has a `registered` record of the hardware ID, the search starts an hour
//! before the record's `registered_at` (`from`), so it reads only the pages after it. It reads at
//! most [`MAX_PAGES`] pages of [`PAGE_LIMIT`] events and says when it stopped before the end.
//! Everything printed from the answer is checked or made printable first.

use chrono::{DateTime, TimeDelta, Utc};
use contract::events::certs::{GATEWAY_REGISTRATION_AGGREGATE, GatewayRegistered};
use http::{Method, StatusCode};
use protocol::GatewayId;
use serde::Deserialize;
use tracing::debug;

use crate::Error;
use crate::http::{ApiError, Call, Client, printable};
use crate::secret::SessionCookie;

/// The platform-wide event log.
pub const EVENT_LOG_PATH: &str = "/api/identity/v1/platform/event-log";

/// The registration event's type.
pub const EVENT_TYPE: &str = "certs.gateway_registered";

/// Events asked for per page.
pub const PAGE_LIMIT: u32 = 200;

/// The most pages one `verify` reads.
pub const MAX_PAGES: u32 = 50;

/// The longest `next_cursor` sent back: the identity service's are about 60 characters.
const MAX_CURSOR_LEN: usize = 256;

/// How long before the recorded `registered_at` the search starts.
const FROM_MARGIN: TimeDelta = TimeDelta::hours(1);

#[derive(Deserialize)]
struct Page {
    events: Vec<LoggedEvent>,
    next_cursor: Option<String>,
}

#[derive(Deserialize)]
struct LoggedEvent {
    event_id: String,
    #[serde(rename = "type")]
    event_type: String,
    occurred_at: DateTime<Utc>,
    aggregate_type: String,
    aggregate_id: String,
    payload: serde_json::Value,
}

/// One `certs.gateway_registered` event of the hardware ID.
#[derive(Clone, Debug)]
pub struct Found {
    /// The event's ID, printable.
    pub event_id: String,
    /// When the certs service registered the gateway.
    pub occurred_at: DateTime<Utc>,
    /// The event's payload.
    pub registered: GatewayRegistered,
}

impl Found {
    /// The certificate serial to print: as it is when it is 2 to 40 lower-case hex digits,
    /// made printable otherwise.
    #[must_use]
    pub fn serial(&self) -> String {
        let serial = &self.registered.certificate_serial;
        if crate::identity::is_serial(serial) {
            serial.clone()
        } else {
            printable(serial)
        }
    }
}

/// What a search found.
#[derive(Clone, Debug)]
pub struct Search {
    /// The hardware ID's registration events, newest first.
    pub found: Vec<Found>,
    /// How many events were read.
    pub events_read: usize,
    /// Whether the whole log (from `from` on) was read; false when [`MAX_PAGES`] ran out.
    pub complete: bool,
}

/// Where a search for `record_registered_at` starts.
#[must_use]
pub fn search_from(record_registered_at: Option<DateTime<Utc>>) -> Option<DateTime<Utc>> {
    record_registered_at.and_then(|at| at.checked_sub_signed(FROM_MARGIN))
}

/// Reads the platform-wide event log for `hardware_id`'s registration events, with the platform
/// administrator's `cookie`, from `from` on (all of it when `None`).
///
/// # Errors
///
/// No response, an error status ([`Error::Api`]: the caller handles the platform's own 401), or
/// an answer that is not an event log page.
pub async fn find(
    client: &Client,
    cookie: &SessionCookie,
    hardware_id: &GatewayId,
    from: Option<DateTime<Utc>>,
) -> Result<Search, Error> {
    let mut found = Vec::new();
    let mut events_read = 0;
    let mut cursor: Option<String> = None;
    for page_number in 1..=MAX_PAGES {
        let mut query = url::form_urlencoded::Serializer::new(String::new());
        query.append_pair("limit", &PAGE_LIMIT.to_string());
        if let Some(from) = from {
            query.append_pair(
                "from",
                &from.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            );
        }
        if let Some(cursor) = &cursor {
            query.append_pair("cursor", cursor);
        }
        let query = query.finish();
        let response = client
            .send(Call {
                method: Method::GET,
                path: EVENT_LOG_PATH,
                query: Some(&query),
                cookie: Some(cookie),
                headers: Vec::new(),
                json: None,
            })
            .await?;
        if response.status != StatusCode::OK {
            return Err(ApiError::new(&Method::GET, EVENT_LOG_PATH, &response).into());
        }
        let page: Page = serde_json::from_slice(&response.body).map_err(|_| {
            Error::Failed(format!(
                "GET {EVENT_LOG_PATH} answered 200 with a body that is not an event log page"
            ))
        })?;
        events_read += page.events.len();
        debug!(
            page = page_number,
            events = page.events.len(),
            "read a page of the event log"
        );
        for event in page.events {
            if event.event_type != EVENT_TYPE
                || event.aggregate_type != GATEWAY_REGISTRATION_AGGREGATE
                || event.aggregate_id != hardware_id.as_str()
            {
                continue;
            }
            let Ok(registered) = serde_json::from_value::<GatewayRegistered>(event.payload) else {
                return Err(Error::Failed(format!(
                    "the event log has a {EVENT_TYPE} event of hardware ID {hardware_id} whose \
                     payload this tool cannot read (event {})",
                    printable(&event.event_id)
                )));
            };
            if registered.hardware_id != *hardware_id {
                continue;
            }
            found.push(Found {
                event_id: printable(&event.event_id),
                occurred_at: event.occurred_at,
                registered,
            });
        }
        match page.next_cursor {
            None => {
                return Ok(Search {
                    found,
                    events_read,
                    complete: true,
                });
            }
            Some(next) if next.is_empty() || next.len() > MAX_CURSOR_LEN => {
                return Err(Error::Failed(format!(
                    "GET {EVENT_LOG_PATH} answered with a next_cursor that is empty or longer \
                     than {MAX_CURSOR_LEN} characters"
                )));
            }
            Some(next) => cursor = Some(next),
        }
    }
    Ok(Search {
        found,
        events_read,
        complete: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_search_starts_an_hour_before_the_record() {
        let at = DateTime::parse_from_rfc3339("2026-10-29T08:00:00Z")
            .expect("a time")
            .with_timezone(&Utc);
        assert_eq!(
            search_from(Some(at)).expect("a start").to_rfc3339(),
            "2026-10-29T07:00:00+00:00"
        );
        assert!(search_from(None).is_none());
    }
}
