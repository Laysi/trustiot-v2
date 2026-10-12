//! How far a subscription is behind its producer, exported as OpenTelemetry gauges.
//!
//! - `outbox.consumer.lag` (`{event}`): committed events after the subscription's position,
//!   counted up to [`LAG_COUNT_CAP`].
//! - `outbox.consumer.lag.age` (`s`): how long ago the oldest of them was written; 0 when none
//!   is waiting.
//!
//! Both carry `outbox.subscription` and `outbox.source` (the producer's schema). The values
//! come from the last measurement of each live [`crate::Consumer`], and only while it is
//! current: a measurement older than [`CURRENT_FOR_INTERVALS`] lag intervals, or one whose
//! [`crate::Consumer::monitor_lag`] has ended (shutdown, or its task dropped), is no longer
//! reported, so a consumer that stopped measuring never shows a frozen value. Its series then
//! stops, which an alert on missing data catches; a consumer that still measures but no longer
//! processes shows a rising lag and age instead.

use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError, Weak};
use std::time::{Duration, Instant};

use opentelemetry::KeyValue;
use opentelemetry::metrics::ObservableGauge;
use sqlx::PgConnection;

use crate::position::Position;

/// The most events a measurement counts; beyond it the lag reads as this value.
pub const LAG_COUNT_CAP: u32 = 10_000;

/// How many lag intervals a measurement stays current. Measurements normally replace each other
/// every interval; one that has not been replaced for this long means the measuring loop is
/// stuck (a query that does not return, a blocked thread) or keeps failing.
pub const CURRENT_FOR_INTERVALS: u32 = 3;

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
    /// How long a measurement stays current.
    current_for: Duration,
    last: Mutex<Option<Reading>>,
}

/// A measurement and when it was stored.
#[derive(Clone, Copy)]
struct Reading {
    lag: Lag,
    at: Instant,
}

/// Every live consumer's cell. Consumers hold the strong references, so a dropped consumer's
/// series stops being reported.
static CELLS: Mutex<Vec<Weak<LagCell>>> = Mutex::new(Vec::new());

/// The gauges, registered once per process with the global meter provider.
static GAUGES: OnceLock<(ObservableGauge<u64>, ObservableGauge<f64>)> = OnceLock::new();

impl LagCell {
    /// A cell for one consumer that measures every `interval`, registering the gauges on first
    /// use. A measurement stays current for [`CURRENT_FOR_INTERVALS`] intervals.
    pub(crate) fn register(subscription: &str, source: &str, interval: Duration) -> Arc<Self> {
        GAUGES.get_or_init(gauges);
        let cell = Arc::new(Self {
            attributes: [
                KeyValue::new("outbox.subscription", subscription.to_owned()),
                KeyValue::new("outbox.source", source.to_owned()),
            ],
            current_for: interval.saturating_mul(CURRENT_FOR_INTERVALS),
            last: Mutex::new(None),
        });
        let mut cells = CELLS.lock().unwrap_or_else(PoisonError::into_inner);
        cells.retain(|cell| cell.strong_count() > 0);
        cells.push(Arc::downgrade(&cell));
        cell
    }

    /// Stores a measurement taken now.
    pub(crate) fn set(&self, lag: Lag) {
        *self.last() = Some(Reading {
            lag,
            at: Instant::now(),
        });
    }

    /// Forgets the last measurement: nothing measures this consumer any more.
    pub(crate) fn clear(&self) {
        *self.last() = None;
    }

    /// The last measurement, while it is current.
    pub(crate) fn get(&self) -> Option<Lag> {
        let reading = (*self.last())?;
        (reading.at.elapsed() <= self.current_for).then_some(reading.lag)
    }

    fn last(&self) -> MutexGuard<'_, Option<Reading>> {
        self.last.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Clears a cell when dropped, so that whatever ends the loop measuring it (shutdown, a panic,
/// its task dropped), the gauges stop reporting the last value.
pub(crate) struct ClearOnDrop<'a>(pub(crate) &'a LagCell);

impl Drop for ClearOnDrop<'_> {
    fn drop(&mut self) {
        self.0.clear();
    }
}

/// Calls `observe` with each live consumer's current measurement and attributes.
fn each_measurement(mut observe: impl FnMut(Lag, &[KeyValue])) {
    let cells: Vec<Arc<LagCell>> = CELLS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
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

#[cfg(test)]
mod tests {
    use super::*;

    const LAG: Lag = Lag {
        events: 2,
        oldest_age: Some(Duration::from_secs(5)),
    };

    /// What the gauges' callbacks report for `subscription` now.
    fn reported(subscription: &str) -> Option<Lag> {
        let mut found = None;
        each_measurement(|lag, attributes| {
            let ours = attributes.iter().any(|attribute| {
                attribute.key.as_str() == "outbox.subscription"
                    && attribute.value.as_str() == subscription
            });
            if ours {
                found = Some(lag);
            }
        });
        found
    }

    #[test]
    fn a_measurement_is_reported_until_its_loop_ends() {
        // Current for an hour: the reading cannot go stale during the test, however loaded.
        let cell = LagCell::register("lag-test.ended", "producer", Duration::from_secs(1200));
        assert_eq!(cell.get(), None, "nothing measured yet");
        assert_eq!(reported("lag-test.ended"), None);
        cell.set(LAG);
        assert_eq!(cell.get(), Some(LAG));
        assert_eq!(reported("lag-test.ended"), Some(LAG));
        drop(ClearOnDrop(&cell));
        assert_eq!(cell.get(), None, "the measuring loop ended");
        assert_eq!(reported("lag-test.ended"), None);
    }

    #[test]
    fn a_measurement_that_is_not_replaced_in_time_is_not_reported() {
        // Current for 3 ms; after sleeping 20 ms the reading is older than that.
        let cell = LagCell::register("lag-test.stale", "producer", Duration::from_millis(1));
        cell.set(LAG);
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(cell.get(), None, "a stale reading is not current");
        assert_eq!(reported("lag-test.stale"), None);
    }
}
