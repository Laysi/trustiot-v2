//! Reading a device's data points over Modbus (rebuild checklist F1, F2, F5).
//!
//! A [`DeviceReader`] is built once per device from the desired configuration: it plans the
//! reads of the device model's points ([`plan`]: same function, at most [`plan::MAX_GAP`]
//! addresses apart, at most 125 registers or 2000 bits per read). [`DeviceReader::poll`] sends
//! those reads over a [`Connection`] one after another, decodes every point
//! ([`decode`]: type, byte order, then scale and offset) and stamps it with the [`Clock`]: a
//! reading in milliseconds, with whether the clock was synchronized.
//!
//! Only the read function codes 1–4 can be sent: requests are built from
//! [`protocol::config::FunctionCode`], which has no other values ([`pdu::Read`]). The gateway
//! never writes to a device.
//!
//! [`tcp::TcpConnection`] is the [`Connection`] for both transports: MBAP frames for Modbus TCP,
//! RTU frames with a CRC over TCP for an RS485 converter. A response counts only when it comes
//! from the unit asked and answers this request (its transaction over MBAP; its function code
//! and byte count over RTU, which has no transaction); others are discarded and the request
//! keeps waiting. [`endpoint::Endpoints`] gives every device at the same endpoint one
//! [`endpoint::SharedConnection`], which their polls use one request at a time.
//!
//! A connection goes only to an address it checked (`address.rs`): the host is resolved once,
//! and loopback, unspecified, multicast and broadcast addresses, the gateway's own addresses
//! and the cloud metadata addresses are refused (Laysi/trustiot-v2#653).

// Crate-visible for the fieldbus allowlist (`crate::allowlist`), which refuses what the guard
// refuses wherever the gateway is.
pub(crate) mod address;
pub mod decode;
pub mod endpoint;
mod mbap;
pub mod pdu;
pub mod plan;
mod rtu;
pub mod tcp;

use std::future::Future;
use std::time::Duration;

use protocol::config::{DataPoint, Device, DeviceModel};
use protocol::health::{FailureKind, LastFailure, MAX_FAILURE_DETAIL_BYTES};
use protocol::telemetry::{Reading, Sample};
use protocol::{DeviceId, PointKey, UnixMillis};

use self::decode::{Raw, decode};
use self::pdu::{Data, Read, ResponseError};
use self::plan::{PlanError, PlannedRead, Slot, plan};

/// How long a request waits for its response unless the caller says otherwise (rebuild
/// checklist F4).
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// How long connecting waits unless the caller says otherwise (rebuild checklist F4).
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// A way to send reads to Modbus units and receive their responses.
pub trait Connection {
    /// Sends `read` to `unit` and waits at most `timeout` for the matching response. Dropping
    /// the future before it completes leaves the connection usable for the next read.
    fn read(
        &mut self,
        unit: u8,
        read: Read,
        timeout: Duration,
    ) -> impl Future<Output = Exchange> + Send;
}

/// How one request ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Exchange {
    /// The values, or why there are none.
    pub result: Result<Data, ReadError>,
    /// Responses from other units that were discarded while waiting.
    pub unit_mismatches: u32,
}

/// Why a request has no values.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReadError {
    /// No connection could be opened; the request was not sent.
    Connect(String),
    /// No matching response arrived in time. The connection stays open: a late response is
    /// discarded when it arrives, except over RTU when the request that waits then has the
    /// same unit, function code and count (RTU has no transaction identifier to tell them
    /// apart).
    Timeout,
    /// The device answered with this Modbus exception code.
    Exception(u8),
    /// The matching response does not fit the request. Over RTU, which has no transaction
    /// identifier, a frame of the unit that does not fit cannot be told from a late answer to
    /// an earlier request, so it is skipped and the request keeps waiting; when no answer comes
    /// by the deadline, the request ends with this instead of [`Self::Timeout`], and so does one
    /// that received only bytes that form no RTU frame (the detail lists possible causes, such
    /// as serial settings that differ between the converter and the device, or noise on the bus).
    Malformed(String),
    /// The connection failed while the request was under way: the device closed or reset it,
    /// or sending failed. It is closed, and the next request opens a new one.
    ConnectionLost(String),
    /// The device sent bytes that are not Modbus TCP, such as a header with another protocol
    /// identifier: for example an RS485 converter that speaks RTU over TCP but is configured as
    /// Modbus TCP. The connection is closed, and the next request opens a new one. Over RTU,
    /// bytes that form no frame are skipped instead.
    Protocol(String),
}

impl From<ResponseError> for ReadError {
    fn from(error: ResponseError) -> Self {
        match error {
            ResponseError::Exception(code) => Self::Exception(code),
            ResponseError::Malformed(detail) => Self::Malformed(detail),
        }
    }
}

impl ReadError {
    /// The failure kind reported in the gateway's health ([`FailureKind`]). A lost connection
    /// counts as `connect`, like one that could not be opened: both are the TCP link, while
    /// `timeout` is a device that stays silent on a working link. A malformed response and a
    /// stream that is not Modbus TCP count as `invalid_value`: the link works, but what comes
    /// back cannot be decoded. Over RTU that includes a request that received, by its
    /// deadline, only frames of its unit that do not fit or bytes that form no frame. A request
    /// that saw only other units' answers times out; those answers are counted in
    /// [`Exchange::unit_mismatches`].
    pub fn kind(&self) -> FailureKind {
        match self {
            Self::Connect(_) | Self::ConnectionLost(_) => FailureKind::Connect,
            Self::Timeout => FailureKind::Timeout,
            Self::Exception(_) => FailureKind::Exception,
            Self::Malformed(_) | Self::Protocol(_) => FailureKind::InvalidValue,
        }
    }

    /// Whether the rest of a poll is skipped: the connection is gone, and opening a new one at
    /// once for every remaining request would only multiply the failure.
    pub fn ends_poll(&self) -> bool {
        matches!(
            self,
            Self::Connect(_) | Self::ConnectionLost(_) | Self::Protocol(_)
        )
    }

    /// This failure as the health report records it.
    pub fn failure(&self, at: UnixMillis) -> LastFailure {
        let (exception_code, detail) = match self {
            Self::Connect(detail)
            | Self::Malformed(detail)
            | Self::ConnectionLost(detail)
            | Self::Protocol(detail) => (None, Some(detail.clone())),
            Self::Timeout => (None, None),
            Self::Exception(code) => (Some(*code), None),
        };
        LastFailure {
            kind: self.kind(),
            at,
            exception_code,
            detail: detail.map(bounded_detail),
        }
    }
}

/// A time to stamp readings with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timestamp {
    /// Milliseconds since the Unix epoch, from the gateway's clock.
    pub at: UnixMillis,
    /// Whether the clock was synchronized when it was read.
    pub synced: bool,
}

/// The gateway's clock.
pub trait Clock {
    /// The time now.
    fn now(&self) -> Timestamp;
}

/// Reads one device: its unit, the points of its model, and the reads planned for them.
#[derive(Clone, Debug, PartialEq)]
pub struct DeviceReader {
    device: DeviceId,
    unit: u8,
    points: Vec<DataPoint>,
    reads: Vec<PlannedRead>,
}

/// What one poll of a device produced.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Poll {
    /// One reading per point that was read and decoded, in the order of the reads.
    pub readings: Vec<Reading>,
    /// Every other point, and why it has no reading.
    pub failed: Vec<PointFailure>,
    /// The requests that were sent or failed to connect, in order. A request skipped because the
    /// connection failed earlier in the poll is not listed; its points are in `failed`.
    pub requests: Vec<RequestReport>,
}

/// A data point without a reading.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PointFailure {
    /// The point.
    pub point: PointKey,
    /// Why.
    pub failure: LastFailure,
}

/// One request of a poll.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestReport {
    /// What was asked.
    pub read: Read,
    /// Why it failed; `None` when the device answered with values.
    pub failure: Option<LastFailure>,
    /// Responses from other units discarded while it waited.
    pub unit_mismatches: u32,
}

impl DeviceReader {
    /// Plans the reads for `device`, whose model is `model`. Fails only for points that a
    /// validated configuration cannot contain ([`PlanError`]).
    pub fn new(device: &Device, model: &DeviceModel) -> Result<Self, PlanError> {
        Ok(Self {
            device: device.id.clone(),
            unit: device.unit_id,
            reads: plan(&model.points)?,
            points: model.points.clone(),
        })
    }

    /// The planned reads, in the order a poll sends them.
    pub fn reads(&self) -> &[PlannedRead] {
        &self.reads
    }

    /// Sends every planned read in turn, each waiting at most `timeout`. A timeout, an exception
    /// or a malformed response fails only that request's points and the poll goes on; a failed
    /// connection ([`ReadError::ends_poll`]) fails the remaining points without sending their
    /// requests. A point's reading is stamped with `clock` when its response arrived.
    pub async fn poll(
        &self,
        connection: &mut impl Connection,
        clock: &impl Clock,
        timeout: Duration,
    ) -> Poll {
        let mut poll = Poll::default();
        let mut skipped: Option<LastFailure> = None;
        for planned in &self.reads {
            if let Some(failure) = &skipped {
                self.fail_points(&mut poll, planned, failure);
                continue;
            }
            let exchange = connection.read(self.unit, planned.read, timeout).await;
            let now = clock.now();
            let mut report = RequestReport {
                read: planned.read,
                failure: None,
                unit_mismatches: exchange.unit_mismatches,
            };
            match exchange.result {
                Ok(data) => {
                    for slot in &planned.points {
                        self.decode_point(&mut poll, &data, slot, now);
                    }
                }
                Err(error) => {
                    let failure = error.failure(now.at);
                    self.fail_points(&mut poll, planned, &failure);
                    if error.ends_poll() {
                        skipped = Some(LastFailure {
                            detail: Some(bounded_detail(format!(
                                "not sent: an earlier request of this poll failed ({})",
                                failure.detail.as_deref().unwrap_or("connection")
                            ))),
                            ..failure.clone()
                        });
                    }
                    report.failure = Some(failure);
                }
            }
            poll.requests.push(report);
        }
        poll
    }

    fn decode_point(&self, poll: &mut Poll, data: &Data, slot: &Slot, now: Timestamp) {
        let Some(point) = self.points.get(slot.index) else {
            return;
        };
        let start = usize::from(slot.offset);
        let end = start + usize::from(slot.width);
        let raw = match data {
            Data::Bits(bits) => bits.get(start).map(|bit| Raw::Bit(*bit)),
            Data::Registers(registers) => registers.get(start..end).map(Raw::Registers),
        };
        let value = match raw {
            Some(raw) => decode(point, raw),
            None => Err(decode::InvalidValue::Width {
                expected: usize::from(slot.width),
                got: 0,
            }),
        };
        match value {
            Ok(value) => poll.readings.push(Reading {
                device: self.device.clone(),
                point: point.key.clone(),
                sample: Sample { at: now.at, value },
                clock_synced: now.synced,
            }),
            Err(invalid) => poll.failed.push(PointFailure {
                point: point.key.clone(),
                failure: LastFailure {
                    kind: FailureKind::InvalidValue,
                    at: now.at,
                    exception_code: None,
                    detail: Some(bounded_detail(invalid.to_string())),
                },
            }),
        }
    }

    fn fail_points(&self, poll: &mut Poll, planned: &PlannedRead, failure: &LastFailure) {
        for slot in &planned.points {
            if let Some(point) = self.points.get(slot.index) {
                poll.failed.push(PointFailure {
                    point: point.key.clone(),
                    failure: failure.clone(),
                });
            }
        }
    }
}

/// `detail` cut to [`MAX_FAILURE_DETAIL_BYTES`] at a character boundary, without U+0000, as
/// [`LastFailure::detail`] requires.
fn bounded_detail(detail: String) -> String {
    let mut detail = detail.replace('\0', " ");
    if detail.len() > MAX_FAILURE_DETAIL_BYTES {
        let mut end = MAX_FAILURE_DETAIL_BYTES;
        while !detail.is_char_boundary(end) {
            end -= 1;
        }
        detail.truncate(end);
    }
    detail
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use protocol::config::{ByteOrder, DataType, FunctionCode, Host, Transport};
    use protocol::{ModelName, Value};

    use super::*;

    /// Answers each read with the next scripted exchange and records what was asked.
    struct Scripted {
        answers: VecDeque<Exchange>,
        asked: Vec<(u8, Read, Duration)>,
    }

    impl Connection for Scripted {
        async fn read(&mut self, unit: u8, read: Read, timeout: Duration) -> Exchange {
            self.asked.push((unit, read, timeout));
            self.answers
                .pop_front()
                .expect("a scripted answer for every read")
        }
    }

    fn scripted(answers: impl IntoIterator<Item = Result<Data, ReadError>>) -> Scripted {
        Scripted {
            answers: answers
                .into_iter()
                .map(|result| Exchange {
                    result,
                    unit_mismatches: 0,
                })
                .collect(),
            asked: Vec::new(),
        }
    }

    /// A clock that advances 1 ms per reading and is synchronized from the third reading on.
    struct Ticking(std::cell::Cell<u64>);

    impl Clock for Ticking {
        fn now(&self) -> Timestamp {
            let tick = self.0.get();
            self.0.set(tick + 1);
            Timestamp {
                at: UnixMillis::new(1_760_000_000_000 + tick),
                synced: tick >= 2,
            }
        }
    }

    fn holding(key: &str, address: u16, data_type: DataType) -> DataPoint {
        DataPoint {
            key: PointKey::new(key).unwrap(),
            function_code: FunctionCode::ReadHoldingRegisters,
            address,
            data_type,
            byte_order: ByteOrder::Abcd,
            length: None,
            scale: None,
            offset: None,
        }
    }

    /// Three reads: holding 0..=1, holding 100, holding 200..=201.
    fn reader() -> DeviceReader {
        let model = DeviceModel {
            name: ModelName::new("meter").unwrap(),
            points: vec![
                holding("a", 0, DataType::U16),
                DataPoint {
                    scale: Some(0.1),
                    ..holding("b", 1, DataType::I16)
                },
                holding("c", 100, DataType::U16),
                holding("d", 200, DataType::F32),
            ],
        };
        let device = Device {
            id: DeviceId::new("meter-1").unwrap(),
            model: model.name.clone(),
            transport: Transport::ModbusTcp,
            host: Host::new("192.0.2.10").unwrap(),
            port: 502,
            unit_id: 7,
            poll_interval_ms: 1000,
        };
        DeviceReader::new(&device, &model).unwrap()
    }

    fn keys(poll: &Poll) -> (Vec<&str>, Vec<(&str, FailureKind)>) {
        (
            poll.readings
                .iter()
                .map(|reading| reading.point.as_str())
                .collect(),
            poll.failed
                .iter()
                .map(|failed| (failed.point.as_str(), failed.failure.kind.clone()))
                .collect(),
        )
    }

    #[tokio::test]
    async fn a_poll_reads_decodes_and_stamps_every_point() {
        let reader = reader();
        let mut connection = scripted([
            Ok(Data::Registers(vec![42, 0xFFF6])),
            Ok(Data::Registers(vec![7])),
            Ok(Data::Registers(vec![0x4366, 0x8000])), // 230.5
        ]);
        let clock = Ticking(std::cell::Cell::new(0));
        let poll = reader
            .poll(&mut connection, &clock, Duration::from_millis(1500))
            .await;

        let values: Vec<(&str, &Value, u64, bool)> = poll
            .readings
            .iter()
            .map(|reading| {
                assert_eq!(reading.device.as_str(), "meter-1");
                (
                    reading.point.as_str(),
                    &reading.sample.value,
                    reading.sample.at.get() - 1_760_000_000_000,
                    reading.clock_synced,
                )
            })
            .collect();
        assert_eq!(
            values,
            [
                ("a", &Value::Int(42), 0, false),
                ("b", &Value::Float(-10.0 * 0.1), 0, false),
                ("c", &Value::Int(7), 1, false),
                ("d", &Value::Float(230.5), 2, true),
            ]
        );
        assert!(poll.failed.is_empty());
        assert_eq!(poll.requests.len(), 3);
        assert!(
            poll.requests
                .iter()
                .all(|request| request.failure.is_none())
        );
        let asked: Vec<(u8, u16, u16, Duration)> = connection
            .asked
            .iter()
            .map(|(unit, read, timeout)| (*unit, read.start(), read.count(), *timeout))
            .collect();
        let timeout = Duration::from_millis(1500);
        assert_eq!(
            asked,
            [
                (7, 0, 2, timeout),
                (7, 100, 1, timeout),
                (7, 200, 2, timeout)
            ]
        );
    }

    #[tokio::test]
    async fn failures_of_one_request_leave_the_others_alone() {
        let reader = reader();
        let mut connection = scripted([
            Err(ReadError::Timeout),
            Err(ReadError::Exception(2)),
            Err(ReadError::Malformed(
                "2 data bytes for 2 registers".to_owned(),
            )),
        ]);
        let clock = Ticking(std::cell::Cell::new(0));
        let poll = reader
            .poll(&mut connection, &clock, DEFAULT_REQUEST_TIMEOUT)
            .await;
        assert_eq!(connection.asked.len(), 3);
        assert_eq!(
            keys(&poll),
            (
                vec![],
                vec![
                    ("a", FailureKind::Timeout),
                    ("b", FailureKind::Timeout),
                    ("c", FailureKind::Exception),
                    ("d", FailureKind::InvalidValue),
                ]
            )
        );
        assert_eq!(poll.failed[2].failure.exception_code, Some(2));
        let failures: Vec<FailureKind> = poll
            .requests
            .iter()
            .map(|request| request.failure.as_ref().unwrap().kind.clone())
            .collect();
        assert_eq!(
            failures,
            [
                FailureKind::Timeout,
                FailureKind::Exception,
                FailureKind::InvalidValue
            ]
        );
    }

    #[tokio::test]
    async fn a_lost_connection_skips_the_rest_of_the_poll() {
        let reader = reader();
        let mut connection = scripted([
            Ok(Data::Registers(vec![1, 2])),
            Err(ReadError::ConnectionLost(
                "the device closed the connection".to_owned(),
            )),
        ]);
        let clock = Ticking(std::cell::Cell::new(0));
        let poll = reader
            .poll(&mut connection, &clock, DEFAULT_REQUEST_TIMEOUT)
            .await;
        assert_eq!(connection.asked.len(), 2, "the third read is not sent");
        assert_eq!(
            keys(&poll),
            (
                vec!["a", "b"],
                vec![("c", FailureKind::Connect), ("d", FailureKind::Connect)]
            )
        );
        assert_eq!(poll.requests.len(), 2);
        assert_eq!(
            poll.failed[1].failure.detail.as_deref(),
            Some(
                "not sent: an earlier request of this poll failed (the device closed the connection)"
            )
        );
    }

    #[tokio::test]
    async fn a_failed_connect_is_one_request_and_skips_the_rest() {
        let reader = reader();
        let mut connection = scripted([Err(ReadError::Connect("connection refused".to_owned()))]);
        let clock = Ticking(std::cell::Cell::new(0));
        let poll = reader
            .poll(&mut connection, &clock, DEFAULT_REQUEST_TIMEOUT)
            .await;
        assert_eq!(connection.asked.len(), 1);
        assert_eq!(poll.requests.len(), 1);
        assert_eq!(poll.failed.len(), 4);
        assert!(
            poll.failed
                .iter()
                .all(|failed| failed.failure.kind == FailureKind::Connect)
        );
    }

    #[tokio::test]
    async fn an_undecodable_point_fails_alone() {
        let reader = reader();
        let mut connection = scripted([
            Ok(Data::Registers(vec![1, 2])),
            Ok(Data::Registers(vec![3])),
            Ok(Data::Registers(vec![0x7FC0, 0x0000])), // NaN
        ]);
        let clock = Ticking(std::cell::Cell::new(0));
        let poll = reader
            .poll(&mut connection, &clock, DEFAULT_REQUEST_TIMEOUT)
            .await;
        assert_eq!(
            keys(&poll),
            (vec!["a", "b", "c"], vec![("d", FailureKind::InvalidValue)])
        );
        assert!(
            poll.requests
                .iter()
                .all(|request| request.failure.is_none())
        );
    }

    #[test]
    fn failures_carry_bounded_details() {
        let long = "é".repeat(MAX_FAILURE_DETAIL_BYTES);
        let failure = ReadError::ConnectionLost(long).failure(UnixMillis::new(5));
        let detail = failure.detail.unwrap();
        assert!(detail.len() <= MAX_FAILURE_DETAIL_BYTES);
        assert_eq!(detail.len(), MAX_FAILURE_DETAIL_BYTES);
        assert_eq!(bounded_detail("a\0b".to_owned()), "a b");
        assert_eq!(ReadError::Timeout.failure(UnixMillis::new(5)).detail, None);
        let exception = ReadError::Exception(0x0B).failure(UnixMillis::new(5));
        assert_eq!(
            (exception.kind, exception.exception_code),
            (FailureKind::Exception, Some(0x0B))
        );
    }
}
