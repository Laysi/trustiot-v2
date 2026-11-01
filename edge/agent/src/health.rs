//! Read statistics of every configured device, for the heartbeat's `devices`
//! (rebuild checklist H1, Laysi/trustiot-v2#117; Laysi/trustiot#160, Laysi/trustiot#183).
//!
//! A [`DeviceBook`] keeps one [`DeviceStats`] per device of the applied configuration, from the
//! agent's start. [`HealthSink`] feeds it from the polling loops: every poll that ends is counted,
//! the regular polls and the extra ones made for live values alike, because every one of them
//! sent requests to the device. The book is the only thing the heartbeat reads; the cloud works
//! out the success rate and the rates of each kind from the difference between two heartbeats
//! (protocol README, 「在線狀態與心跳」).
//!
//! What is counted, per request of a poll ([`RequestReport`]):
//!
//! - `requests`: every request that was sent or failed to connect. A request skipped because the
//!   connection failed earlier in the same poll is not one (its points are in `points_failed`).
//! - `failures`: a failed request counts once, under its kind ([`crate::modbus::ReadError::kind`]): `connect`
//!   (no connection, or a lost one), `timeout`, `exception` (and `exception_codes` by code) and
//!   `invalid_value` (a response that does not fit, or a stream that is not Modbus). Not part of
//!   a request's failure: `unit_mismatch` counts the responses of other units that were
//!   discarded while the request waited (a converter that shares responses among connections,
//!   Laysi/trustiot#183), and the request then usually also counts as a `timeout`; and
//!   `invalid_value` also counts the points that were read but did not decode ([`Poll::undecodable`]).
//! - `last_success`: the time of the newest reading the device produced.
//! - `points_total` and `points_failed`: the latest poll's points, whatever kind it was. 0 failed
//!   is healthy, some is a partial failure, all of them means the device is unreachable.
//! - `last_failure`: the latest failed request of the latest poll that had one, or its latest
//!   undecodable point; it stays until a newer failure replaces it, also after the device
//!   recovered.
//!
//! A device is in the book from the moment its polling task starts ([`Sink::started`]), before
//! its first poll, with zero requests, and leaves it with [`Sink::removed`]. A device whose
//! settings changed keeps its counters, like everything else the sinks keep for it
//! ([`crate::desired::fleet`]).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use protocol::config::MAX_DEVICES;
use protocol::health::{DeviceStats, FailureCounts, FailureKind, LastFailure, MAX_EXCEPTION_CODES};
use protocol::{DeviceId, UnixMillis};

use crate::desired::fleet::Sink;
use crate::modbus::{Poll, RequestReport};
use crate::polling::Polled;

/// What is kept for one device.
#[derive(Debug, Default)]
struct Counters {
    last_success: Option<UnixMillis>,
    points_total: u32,
    points_failed: u32,
    requests: u64,
    failures: FailureCounts,
    exception_codes: BTreeMap<u8, u64>,
    last_failure: Option<LastFailure>,
}

/// The read statistics of the configured devices; cheap to clone, every clone is the same book.
#[derive(Clone, Debug, Default)]
pub struct DeviceBook {
    devices: Arc<Mutex<BTreeMap<DeviceId, Counters>>>,
}

impl DeviceBook {
    /// An empty book.
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, BTreeMap<DeviceId, Counters>> {
        self.devices.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Puts `device` in the book with zero counters unless it is there already.
    pub fn add(&self, device: &DeviceId) {
        self.lock().entry(device.clone()).or_default();
    }

    /// Takes `device` out of the book, counters included.
    pub fn remove(&self, device: &DeviceId) {
        self.lock().remove(device);
    }

    /// Counts a poll of `device`.
    pub fn record(&self, device: &DeviceId, poll: &Poll) {
        let mut devices = self.lock();
        let counters = devices.entry(device.clone()).or_default();
        counters.record(poll);
    }

    /// The statistics of every device, in the order of their IDs and at most
    /// [`MAX_DEVICES`] of them (a validated configuration has no more).
    pub fn snapshot(&self) -> Vec<DeviceStats> {
        self.lock()
            .iter()
            .take(MAX_DEVICES)
            .map(|(device, counters)| DeviceStats {
                device: device.clone(),
                last_success: counters.last_success,
                points_total: counters.points_total,
                points_failed: counters.points_failed,
                requests: counters.requests,
                failures: counters.failures,
                exception_codes: counters.exception_codes.clone(),
                last_failure: counters.last_failure.clone(),
            })
            .collect()
    }
}

impl Counters {
    fn record(&mut self, poll: &Poll) {
        let points_failed = u32::try_from(poll.failed.len()).unwrap_or(u32::MAX);
        self.points_failed = points_failed;
        self.points_total =
            points_failed.saturating_add(u32::try_from(poll.readings.len()).unwrap_or(u32::MAX));
        if let Some(newest) = poll.readings.iter().map(|reading| reading.sample.at).max() {
            self.last_success = self.last_success.max(Some(newest));
        }
        self.requests = self
            .requests
            .saturating_add(u64::try_from(poll.requests.len()).unwrap_or(u64::MAX));
        for request in &poll.requests {
            self.record_request(request);
        }
        self.failures.invalid_value = self
            .failures
            .invalid_value
            .saturating_add(u64::from(poll.undecodable));
        let failed_request = poll
            .requests
            .iter()
            .rev()
            .find_map(|request| request.failure.as_ref());
        let undecodable_point = (poll.undecodable > 0)
            .then(|| {
                poll.failed
                    .iter()
                    .rev()
                    .find(|point| point.failure.kind == FailureKind::InvalidValue)
                    .map(|point| &point.failure)
            })
            .flatten();
        // The latest in time: a request's failure carries the time of its response, a point's
        // the time of its decoding, which follows its request's.
        let latest = match (failed_request, undecodable_point) {
            (Some(request), Some(point)) => Some(if point.at >= request.at {
                point
            } else {
                request
            }),
            (request, point) => request.or(point),
        };
        if let Some(failure) = latest {
            self.last_failure = Some(failure.clone());
        }
    }

    fn record_request(&mut self, request: &RequestReport) {
        self.failures.unit_mismatch = self
            .failures
            .unit_mismatch
            .saturating_add(u64::from(request.unit_mismatches));
        let Some(failure) = &request.failure else {
            return;
        };
        let count = match &failure.kind {
            FailureKind::Connect => &mut self.failures.connect,
            FailureKind::Timeout => &mut self.failures.timeout,
            FailureKind::Exception => &mut self.failures.exception,
            FailureKind::InvalidValue => &mut self.failures.invalid_value,
            // A request's failure is one of the above ([`ReadError::kind`]); a kind of a later
            // protocol revision cannot come from this agent.
            _ => return,
        };
        *count = count.saturating_add(1);
        if let Some(code) = failure.exception_code {
            // Past the limit a new code is counted in `exception` only.
            if self.exception_codes.contains_key(&code)
                || self.exception_codes.len() < MAX_EXCEPTION_CODES
            {
                let times = self.exception_codes.entry(code).or_default();
                *times = times.saturating_add(1);
            }
        }
    }
}

/// The sink that counts every poll in a [`DeviceBook`] and hands it on to `inner`.
pub struct HealthSink {
    inner: Arc<dyn Sink>,
    book: DeviceBook,
}

impl HealthSink {
    /// Counts into `book` what the polling loops report, and passes everything on to `inner`.
    pub fn new(book: DeviceBook, inner: Arc<dyn Sink>) -> Self {
        Self { inner, book }
    }
}

impl std::fmt::Debug for HealthSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HealthSink").finish_non_exhaustive()
    }
}

impl Sink for HealthSink {
    fn started(&self, device: &DeviceId) {
        self.book.add(device);
        self.inner.started(device);
    }

    fn polled(&self, device: &DeviceId, polled: Polled) {
        self.book.record(device, &polled.poll);
        self.inner.polled(device, polled);
    }

    fn removed(&self, device: &DeviceId) {
        self.book.remove(device);
        self.inner.removed(device);
    }
}

#[cfg(test)]
mod tests {
    use protocol::config::FunctionCode;
    use protocol::health::MAX_FAILURE_DETAIL_BYTES;
    use protocol::telemetry::{Reading, Sample};
    use protocol::{PointKey, Value};

    use super::*;
    use crate::modbus::pdu::Read;
    use crate::modbus::{PointFailure, ReadError};

    fn id(name: &str) -> DeviceId {
        DeviceId::new(name).unwrap()
    }

    fn read() -> Read {
        Read::new(FunctionCode::ReadHoldingRegisters, 0, 1).unwrap()
    }

    fn reading(at: u64) -> Reading {
        Reading {
            device: id("meter-1"),
            point: PointKey::new("power").unwrap(),
            sample: Sample {
                at: UnixMillis::new(at),
                value: Value::Int(1),
            },
            clock_synced: true,
        }
    }

    fn ok_request() -> RequestReport {
        RequestReport {
            read: read(),
            failure: None,
            unit_mismatches: 0,
        }
    }

    fn failed_request(error: &ReadError, at: u64, unit_mismatches: u32) -> RequestReport {
        RequestReport {
            read: read(),
            failure: Some(error.failure(UnixMillis::new(at))),
            unit_mismatches,
        }
    }

    /// A poll whose single request failed with `error`, so its single point failed too.
    fn failed_poll(error: &ReadError, at: u64, unit_mismatches: u32) -> Poll {
        let request = failed_request(error, at, unit_mismatches);
        Poll {
            readings: vec![],
            failed: vec![PointFailure {
                point: PointKey::new("power").unwrap(),
                failure: request.failure.clone().unwrap(),
            }],
            requests: vec![request],
            undecodable: 0,
        }
    }

    fn ok_poll(at: u64) -> Poll {
        Poll {
            readings: vec![reading(at)],
            failed: vec![],
            requests: vec![ok_request()],
            undecodable: 0,
        }
    }

    fn stats(book: &DeviceBook, device: &str) -> DeviceStats {
        book.snapshot()
            .into_iter()
            .find(|stats| stats.device == id(device))
            .unwrap()
    }

    #[test]
    fn a_device_is_known_before_its_first_poll_with_zero_counters() {
        let book = DeviceBook::new();
        book.add(&id("meter-1"));
        let stats = stats(&book, "meter-1");
        assert_eq!(stats.requests, 0);
        assert_eq!(stats.last_success, None);
        assert_eq!(stats.failures, FailureCounts::default());
        assert_eq!(stats.last_failure, None);
        // Adding it again does not reset it.
        book.record(&id("meter-1"), &ok_poll(5));
        book.add(&id("meter-1"));
        assert_eq!(self::stats(&book, "meter-1").requests, 1);
    }

    #[test]
    fn every_kind_of_failed_request_is_counted_under_its_kind() {
        let book = DeviceBook::new();
        let device = id("meter-1");
        book.record(&device, &ok_poll(10));
        for (error, at) in [
            (ReadError::Connect("refused".to_owned()), 20),
            (ReadError::ConnectionLost("reset".to_owned()), 21),
            (ReadError::Timeout, 30),
            (ReadError::Timeout, 31),
            (ReadError::Timeout, 32),
            (ReadError::Exception(2), 40),
            (ReadError::Exception(2), 41),
            (ReadError::Exception(11), 42),
            (ReadError::Malformed("short".to_owned()), 50),
            (ReadError::Protocol("not Modbus".to_owned()), 51),
        ] {
            book.record(&device, &failed_poll(&error, at, 0));
        }
        let stats = stats(&book, "meter-1");
        assert_eq!(stats.requests, 11);
        assert_eq!(
            stats.failures,
            FailureCounts {
                connect: 2,
                timeout: 3,
                exception: 3,
                unit_mismatch: 0,
                invalid_value: 2,
            }
        );
        assert_eq!(stats.exception_codes, BTreeMap::from([(2, 2), (11, 1)]));
        let last = stats.last_failure.unwrap();
        assert_eq!(last.kind, FailureKind::InvalidValue);
        assert_eq!(last.at.get(), 51);
        assert_eq!(last.detail.as_deref(), Some("not Modbus"));
        assert_eq!(stats.last_success, Some(UnixMillis::new(10)));
        // The latest poll failed entirely: the device is unreachable.
        assert_eq!((stats.points_total, stats.points_failed), (1, 1));
        // The sum of the kinds that fail a request is below the requests, as a success rate
        // needs: 1 - (2 + 3 + 3) / 11.
        assert!(
            stats.failures.connect + stats.failures.timeout + stats.failures.exception
                < stats.requests
        );
    }

    #[test]
    fn responses_of_other_units_are_counted_apart_from_the_request_that_times_out() {
        let book = DeviceBook::new();
        let device = id("meter-1");
        book.record(&device, &failed_poll(&ReadError::Timeout, 10, 3));
        book.record(&device, &failed_poll(&ReadError::Timeout, 20, 2));
        // A request that got its answer after discarding another unit's still succeeded.
        let mut late = ok_poll(30);
        late.requests[0].unit_mismatches = 1;
        book.record(&device, &late);
        let stats = stats(&book, "meter-1");
        assert_eq!(stats.requests, 3);
        assert_eq!(stats.failures.unit_mismatch, 6);
        assert_eq!(stats.failures.timeout, 2);
        assert_eq!(
            stats.last_failure.unwrap().at.get(),
            20,
            "from the second poll"
        );
    }

    #[test]
    fn a_partial_poll_counts_its_points_and_its_undecodable_ones_as_invalid_values() {
        let book = DeviceBook::new();
        let device = id("meter-1");
        // Three points: two read, one that did not decode; its request succeeded.
        let poll = Poll {
            readings: vec![reading(100), reading(101)],
            failed: vec![PointFailure {
                point: PointKey::new("nan").unwrap(),
                failure: LastFailure {
                    kind: FailureKind::InvalidValue,
                    at: UnixMillis::new(102),
                    exception_code: None,
                    detail: Some("not a number".to_owned()),
                },
            }],
            requests: vec![ok_request()],
            undecodable: 1,
        };
        book.record(&device, &poll);
        let stats = stats(&book, "meter-1");
        assert_eq!((stats.points_total, stats.points_failed), (3, 1));
        assert_eq!(stats.failures.invalid_value, 1);
        assert_eq!(stats.requests, 1);
        assert_eq!(stats.last_success, Some(UnixMillis::new(101)));
        assert_eq!(
            stats.last_failure.unwrap().detail.as_deref(),
            Some("not a number")
        );
        // The device recovers: the failure stays as the most recent one, the points are
        // healthy again.
        book.record(&device, &ok_poll(200));
        let stats = self::stats(&book, "meter-1");
        assert_eq!((stats.points_total, stats.points_failed), (1, 0));
        assert_eq!(stats.last_success, Some(UnixMillis::new(200)));
        assert_eq!(stats.last_failure.unwrap().at.get(), 102);
    }

    #[test]
    fn requests_skipped_after_a_lost_connection_are_not_requests() {
        let book = DeviceBook::new();
        let device = id("meter-1");
        // One request failed to connect; the other two reads of the poll were not sent.
        let failure = ReadError::Connect("refused".to_owned()).failure(UnixMillis::new(5));
        let poll = Poll {
            readings: vec![],
            failed: ["a", "b", "c"]
                .map(|key| PointFailure {
                    point: PointKey::new(key).unwrap(),
                    failure: failure.clone(),
                })
                .to_vec(),
            requests: vec![RequestReport {
                read: read(),
                failure: Some(failure),
                unit_mismatches: 0,
            }],
            undecodable: 0,
        };
        book.record(&device, &poll);
        let stats = stats(&book, "meter-1");
        assert_eq!(stats.requests, 1);
        assert_eq!(stats.failures.connect, 1);
        assert_eq!((stats.points_total, stats.points_failed), (3, 3));
    }

    #[test]
    fn exception_codes_beyond_the_limit_are_counted_in_exception_only() {
        let book = DeviceBook::new();
        let device = id("meter-1");
        let codes = u8::try_from(MAX_EXCEPTION_CODES).unwrap();
        for code in 1..=codes + 4 {
            book.record(&device, &failed_poll(&ReadError::Exception(code), 1, 0));
        }
        // A code that is already there still counts.
        book.record(&device, &failed_poll(&ReadError::Exception(1), 2, 0));
        let stats = stats(&book, "meter-1");
        assert_eq!(stats.exception_codes.len(), MAX_EXCEPTION_CODES);
        assert_eq!(stats.exception_codes[&1], 2);
        assert!(!stats.exception_codes.contains_key(&(codes + 1)));
        assert_eq!(stats.failures.exception, u64::from(codes) + 5);
    }

    #[test]
    fn removing_a_device_forgets_it_and_adding_it_again_starts_over() {
        let book = DeviceBook::new();
        book.record(&id("a"), &ok_poll(1));
        book.record(&id("b"), &ok_poll(1));
        book.remove(&id("a"));
        assert_eq!(
            book.snapshot()
                .iter()
                .map(|stats| stats.device.as_str())
                .collect::<Vec<_>>(),
            ["b"]
        );
        book.add(&id("a"));
        assert_eq!(stats(&book, "a").requests, 0);
    }

    #[test]
    fn failure_details_stay_within_what_the_heartbeat_accepts() {
        let book = DeviceBook::new();
        let device = id("meter-1");
        // The reader bounds every detail it makes; a hostile device cannot make the heartbeat
        // invalid with what its connection error says.
        let hostile = "é\0".repeat(MAX_FAILURE_DETAIL_BYTES);
        book.record(
            &device,
            &failed_poll(&ReadError::ConnectionLost(hostile), 1, 0),
        );
        let detail = stats(&book, "meter-1")
            .last_failure
            .unwrap()
            .detail
            .unwrap();
        assert!(detail.len() <= MAX_FAILURE_DETAIL_BYTES && !detail.contains('\0'));
    }
}
