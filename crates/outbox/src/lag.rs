//! How far a subscription is behind its producer, exported as OpenTelemetry gauges.
//!
//! - `outbox.consumer.lag` (`{event}`): committed events after the subscription's position,
//!   counted up to [`LAG_COUNT_CAP`].
//! - `outbox.consumer.lag.age` (`s`): how long ago the oldest of them was written; 0 when none
//!   is waiting.
//!
//! Both carry `outbox.subscription` and `outbox.source` (the producer's schema). The values
//! come from the last measurement of each live [`crate::Consumer`].

use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use opentelemetry::KeyValue;
use opentelemetry::metrics::ObservableGauge;
use sqlx::PgConnection;

use crate::position::Position;

/// The most events a measurement counts; beyond it the lag reads as this value.
pub const LAG_COUNT_CAP: u32 = 10_000;

/// One lag measurement.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Lag {
    /// Committed events after the subscription's position, at most [`LAG_COUNT_CAP`]. Events of
    /// transactions still running are not counted.
    pub events: u32,
    /// How long ago the oldest waiting event was written; `None` when nothing waits.
    pub oldest_age: Option<Duration>,
}

/// Counts the events after `after` in the log on `tx`'s `search_path`.
pub(crate) async fn measure(tx: &mut PgConnection, after: Position) -> Result<Lag, sqlx::Error> {
    let (count, age): (i64, Option<f64>) = sqlx::query_as(
        "SELECT count(*)::bigint, \
         EXTRACT(EPOCH FROM clock_timestamp() - min(stored_at))::float8 \
         FROM (SELECT stored_at FROM outbox_event WHERE (tx_order, id) > ($1, $2) \
         ORDER BY tx_order, id LIMIT $3) AS waiting",
    )
    .bind(after.tx())
    .bind(after.id())
    .bind(i64::from(LAG_COUNT_CAP))
    .fetch_one(tx)
    .await?;
    Ok(Lag {
        events: u32::try_from(count).unwrap_or(LAG_COUNT_CAP),
        oldest_age: age
            .map(|seconds| Duration::try_from_secs_f64(seconds.max(0.0)).unwrap_or_default()),
    })
}

/// The last measurement of one consumer, read by the gauges' callbacks.
pub(crate) struct LagCell {
    attributes: [KeyValue; 2],
    last: Mutex<Option<Lag>>,
}

/// Every live consumer's cell. Consumers hold the strong references, so a dropped consumer's
/// series stops being reported.
static CELLS: Mutex<Vec<Weak<LagCell>>> = Mutex::new(Vec::new());

/// The gauges, registered once per process with the global meter provider.
static GAUGES: OnceLock<(ObservableGauge<u64>, ObservableGauge<f64>)> = OnceLock::new();

impl LagCell {
    /// A cell for one consumer, registering the gauges on first use.
    pub(crate) fn register(subscription: &str, source: &str) -> Arc<Self> {
        GAUGES.get_or_init(gauges);
        let cell = Arc::new(Self {
            attributes: [
                KeyValue::new("outbox.subscription", subscription.to_owned()),
                KeyValue::new("outbox.source", source.to_owned()),
            ],
            last: Mutex::new(None),
        });
        let mut cells = CELLS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cells.retain(|cell| cell.strong_count() > 0);
        cells.push(Arc::downgrade(&cell));
        cell
    }

    pub(crate) fn set(&self, lag: Lag) {
        *self
            .last
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(lag);
    }

    pub(crate) fn get(&self) -> Option<Lag> {
        *self
            .last
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Calls `observe` with each live consumer's last measurement and attributes.
fn each_measurement(mut observe: impl FnMut(Lag, &[KeyValue])) {
    let cells: Vec<Arc<LagCell>> = CELLS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .filter_map(Weak::upgrade)
        .collect();
    for cell in cells {
        if let Some(lag) = cell.get() {
            observe(lag, &cell.attributes);
        }
    }
}

fn gauges() -> (ObservableGauge<u64>, ObservableGauge<f64>) {
    let meter = opentelemetry::global::meter("outbox");
    let events = meter
        .u64_observable_gauge("outbox.consumer.lag")
        .with_unit("{event}")
        .with_description(
            "Committed outbox events the subscription has not processed yet (counted up to 10000)",
        )
        .with_callback(|observer| {
            each_measurement(|lag, attributes| observer.observe(u64::from(lag.events), attributes));
        })
        .build();
    let age = meter
        .f64_observable_gauge("outbox.consumer.lag.age")
        .with_unit("s")
        .with_description("Age of the oldest outbox event the subscription has not processed yet")
        .with_callback(|observer| {
            each_measurement(|lag, attributes| {
                let seconds = lag.oldest_age.map_or(0.0, |age| age.as_secs_f64());
                observer.observe(seconds, attributes);
            });
        })
        .build();
    (events, age)
}
