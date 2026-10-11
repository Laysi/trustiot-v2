//! Splitting by the size limits: telemetry batches (with their sequence numbers), logs and live
//! values.

#![allow(
    clippy::unwrap_used,
    reason = "test helpers unwrap like the tests themselves (clippy.toml allows it in tests)"
)]

mod common;

use std::collections::BTreeMap;

use common::{at, device, point};
use protocol::live::LiveSamples;
use protocol::logs::{LogBatch, LogCoverage, LogDump, LogEntry, LogLevel, MAX_FIELDS};
use protocol::telemetry::{
    BatchBuilder, LimitError, MIN_BATCH_LIMIT, PushError, Reading, Sample, Series, TelemetryBatch,
};
use protocol::topic::{Channel, MAX_BULK_BYTES};
use protocol::{
    DeviceId, MAX_ID_LEN, MAX_TEXT_BYTES, PointKey, RequestId, Seq, StreamId, UnixMillis, Value,
    ValueError, encode,
};

/// A small deterministic generator (xorshift64*), so failures reproduce.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }
}

fn value(rng: &mut Rng) -> Value {
    match rng.below(5) {
        0 => Value::Bool(rng.below(2) == 0),
        1 => Value::Int(i64::from_ne_bytes(rng.next().to_ne_bytes())),
        2 => Value::Float(f64::from(u32::try_from(rng.below(1 << 20)).unwrap()) / 8.0),
        3 => Value::Float(f64::from(u32::try_from(rng.below(1 << 30)).unwrap()) / 3.0),
        _ => Value::Text("x".repeat(usize::try_from(rng.below(64)).unwrap())),
    }
}

fn reading(
    device: DeviceId,
    point: PointKey,
    at: UnixMillis,
    value: Value,
    clock: bool,
) -> Reading {
    Reading {
        device,
        point,
        sample: Sample { at, value },
        clock_synced: clock,
    }
}

/// Pushes every reading and finishes, returning every batch in order.
fn build(builder: &mut BatchBuilder, readings: &[Reading]) -> Vec<TelemetryBatch> {
    let mut batches = Vec::new();
    for reading in readings {
        let before = builder.open_len();
        if let Some(closed) = builder.push(reading.clone()).unwrap() {
            // The size the builder tracked is the size the batch encodes to.
            assert_eq!(Some(encode(&closed).unwrap().len()), before);
            batches.push(closed);
        }
    }
    batches.extend(builder.finish());
    batches
}

/// The readings of `batches`, per series in order, with the batch's clock state.
fn per_series(batches: &[TelemetryBatch]) -> BTreeMap<(String, String), Vec<(Sample, bool)>> {
    let mut series: BTreeMap<_, Vec<_>> = BTreeMap::new();
    for batch in batches {
        for entry in &batch.series {
            series
                .entry((entry.device.to_string(), entry.point.to_string()))
                .or_default()
                .extend(
                    entry
                        .samples
                        .iter()
                        .map(|sample| (sample.clone(), batch.clock_synced)),
                );
        }
    }
    series
}

fn expected_series(readings: &[Reading]) -> BTreeMap<(String, String), Vec<(Sample, bool)>> {
    let mut series: BTreeMap<_, Vec<_>> = BTreeMap::new();
    for reading in readings {
        series
            .entry((reading.device.to_string(), reading.point.to_string()))
            .or_default()
            .push((reading.sample.clone(), reading.clock_synced));
    }
    series
}

/// Checks the splitting rules: consecutive sequence numbers from `first`, every batch within
/// `limit`, valid, homogeneous in clock state, and every reading kept in order per series.
fn check(batches: &[TelemetryBatch], readings: &[Reading], first: Seq, limit: usize) {
    let mut seq = Some(first);
    for batch in batches {
        assert_eq!(Some(batch.seq), seq, "sequence numbers are consecutive");
        seq = batch.seq.next();
        let size = encode(batch).unwrap().len();
        assert!(
            size <= limit,
            "batch {} is {size} bytes; the limit is {limit}",
            batch.seq
        );
        let mut points: Vec<_> = batch.series.iter().map(|s| (&s.device, &s.point)).collect();
        points.sort();
        points.dedup();
        assert_eq!(
            points.len(),
            batch.series.len(),
            "each data point once per batch"
        );
    }
    let total: usize = batches.iter().map(TelemetryBatch::record_count).sum();
    assert_eq!(total, readings.len());
    assert_eq!(per_series(batches), expected_series(readings));
}

#[test]
fn oversized_input_is_split_into_consecutive_batches_within_the_limit() {
    let mut rng = Rng(0x5eed);
    let points = ["voltage_a", "current_a", "power", "energy_wh"];
    let readings: Vec<Reading> = (0..2_000u64)
        .map(|index| {
            let n = u8::try_from(index % 5).unwrap();
            let key = points[usize::try_from(index / 5 % 4).unwrap()];
            reading(
                device(n),
                point(key),
                at(index * 100),
                value(&mut rng),
                true,
            )
        })
        .collect();
    let first = Seq::new(7).unwrap();
    let stream = common::stream();
    let mut builder = BatchBuilder::with_limit(stream.clone(), first, MIN_BATCH_LIMIT).unwrap();
    let batches = build(&mut builder, &readings);
    assert!(batches.len() > 10, "{} batches", batches.len());
    check(&batches, &readings, first, MIN_BATCH_LIMIT);
    assert!(batches.iter().all(|batch| batch.stream == stream));
    assert_eq!(
        builder.next_seq(),
        Seq::new(7 + u64::try_from(batches.len()).unwrap())
    );
}

#[test]
fn batches_are_filled_to_the_limit() {
    let readings: Vec<Reading> = (0..500u64)
        .map(|index| {
            reading(
                device(u8::try_from(index % 3).unwrap()),
                point("p"),
                at(index),
                Value::Int(i64::try_from(index * 1_000_003).unwrap()),
                true,
            )
        })
        .collect();
    let mut builder =
        BatchBuilder::with_limit(common::stream(), Seq::FIRST, MIN_BATCH_LIMIT).unwrap();
    let batches = build(&mut builder, &readings);
    check(&batches, &readings, Seq::FIRST, MIN_BATCH_LIMIT);
    // Every closed batch is too full for the reading that started the next one.
    let mut consumed = 0;
    for pair in batches.windows(2) {
        consumed += pair[0].record_count();
        let next = &readings[consumed];
        let mut grown = pair[0].clone();
        match grown
            .series
            .iter_mut()
            .find(|series| series.device == next.device && series.point == next.point)
        {
            Some(series) => series.samples.push(next.sample.clone()),
            None => grown.series.push(Series {
                device: next.device.clone(),
                point: next.point.clone(),
                samples: vec![next.sample.clone()],
            }),
        }
        assert!(encode(&grown).unwrap().len() > MIN_BATCH_LIMIT);
    }
}

#[test]
fn a_change_of_clock_state_closes_the_batch() {
    let clocks = [false, false, true, true, true, false, true];
    let readings: Vec<Reading> = clocks
        .iter()
        .enumerate()
        .map(|(index, &clock)| {
            reading(
                device(1),
                point("p"),
                at(u64::try_from(index).unwrap()),
                Value::Bool(clock),
                clock,
            )
        })
        .collect();
    let mut builder = BatchBuilder::new(common::stream(), Seq::FIRST);
    let batches = build(&mut builder, &readings);
    check(&batches, &readings, Seq::FIRST, MAX_BULK_BYTES);
    let shape: Vec<(bool, usize)> = batches
        .iter()
        .map(|batch| (batch.clock_synced, batch.record_count()))
        .collect();
    assert_eq!(shape, [(false, 2), (true, 3), (false, 1), (true, 1)]);
}

#[test]
fn open_len_is_the_exact_encoded_size() {
    let mut rng = Rng(42);
    let mut builder =
        BatchBuilder::with_limit(common::stream(), Seq::new(23).unwrap(), 6_000).unwrap();
    assert_eq!(builder.open_len(), None);
    assert_eq!(builder.open_records(), 0);
    for index in 0..1_000u64 {
        let n = u8::try_from(rng.below(6)).unwrap();
        let key = format!("p{}", rng.below(9));
        builder
            .push(reading(
                device(n),
                point(&key),
                at(index),
                value(&mut rng),
                true,
            ))
            .unwrap();
        // Crossing seq 23 -> 24 changes the width of the encoded sequence number.
        let open = builder.clone().finish().unwrap();
        assert_eq!(builder.open_len(), Some(encode(&open).unwrap().len()));
        assert_eq!(builder.open_records(), open.record_count());
        assert_eq!(builder.next_seq(), Some(open.seq));
    }
}

#[test]
fn the_default_limit_is_the_telemetry_channel_limit() {
    let mut builder = BatchBuilder::new(common::stream(), Seq::FIRST);
    assert_eq!(builder.limit(), MAX_BULK_BYTES);
    assert_eq!(builder.limit(), Channel::Telemetry.max_payload_bytes());
    let text = Value::Text("t".repeat(MAX_TEXT_BYTES));
    let mut index = 0;
    let closed = loop {
        let n = u8::try_from(index % 200).unwrap();
        if let Some(closed) = builder
            .push(reading(
                device(n),
                point("text"),
                at(index),
                text.clone(),
                true,
            ))
            .unwrap()
        {
            break closed;
        }
        index += 1;
    };
    let size = encode(&closed).unwrap().len();
    assert!(
        size <= MAX_BULK_BYTES && size > MAX_BULK_BYTES - 1_200,
        "{size}"
    );
}

#[test]
fn the_largest_valid_reading_fits_the_smallest_limit() {
    let id = "w".repeat(MAX_ID_LEN);
    let mut builder = BatchBuilder::with_limit(
        StreamId::new(&id).unwrap(),
        Seq::new(u64::MAX).unwrap(),
        MIN_BATCH_LIMIT,
    )
    .unwrap();
    let largest = reading(
        DeviceId::new(&id).unwrap(),
        PointKey::new(&id).unwrap(),
        UnixMillis::new(u64::MAX),
        Value::Text("\u{10ffff}".repeat(MAX_TEXT_BYTES / 4)),
        false,
    );
    assert_eq!(builder.push(largest), Ok(None));
    let batch = builder.finish().unwrap();
    assert!(encode(&batch).unwrap().len() <= MIN_BATCH_LIMIT);
}

#[test]
fn limits_outside_the_allowed_range_are_refused() {
    for limit in [0, MIN_BATCH_LIMIT - 1, MAX_BULK_BYTES + 1] {
        assert_eq!(
            BatchBuilder::with_limit(common::stream(), Seq::FIRST, limit).map(|_| ()),
            Err(LimitError { limit })
        );
    }
    for limit in [MIN_BATCH_LIMIT, MAX_BULK_BYTES] {
        assert!(BatchBuilder::with_limit(common::stream(), Seq::FIRST, limit).is_ok());
    }
}

#[test]
fn a_refused_reading_leaves_the_builder_unchanged() {
    let mut builder = BatchBuilder::new(common::stream(), Seq::FIRST);
    builder
        .push(reading(device(1), point("p"), at(0), Value::Int(1), true))
        .unwrap();
    let (len, records, next) = (
        builder.open_len(),
        builder.open_records(),
        builder.next_seq(),
    );
    for (bad, error) in [
        (Value::Float(f64::NAN), ValueError::NonFiniteFloat),
        (Value::Float(f64::INFINITY), ValueError::NonFiniteFloat),
        (
            Value::Text("x".repeat(MAX_TEXT_BYTES + 1)),
            ValueError::TextTooLong {
                len: MAX_TEXT_BYTES + 1,
            },
        ),
        (Value::Text("a\0b".to_owned()), ValueError::NulInText),
    ] {
        // Also with a clock change, which would otherwise close the open batch.
        for clock in [true, false] {
            assert_eq!(
                builder.push(reading(device(1), point("p"), at(1), bad.clone(), clock)),
                Err(PushError::InvalidValue(error))
            );
            assert_eq!(
                (
                    builder.open_len(),
                    builder.open_records(),
                    builder.next_seq()
                ),
                (len, records, next)
            );
        }
    }
}

#[test]
fn a_stream_never_wraps_its_sequence_numbers() {
    let last = Seq::new(u64::MAX).unwrap();
    let before_last = Seq::new(u64::MAX - 1).unwrap();
    let mut builder = BatchBuilder::new(common::stream(), before_last);
    let push = |builder: &mut BatchBuilder, clock: bool| {
        builder.push(reading(
            device(1),
            point("p"),
            at(0),
            Value::Bool(clock),
            clock,
        ))
    };
    assert_eq!(push(&mut builder, true).unwrap(), None);
    let closed = push(&mut builder, false).unwrap().unwrap();
    assert_eq!(closed.seq, before_last);
    assert_eq!(builder.next_seq(), Some(last));
    // Same clock state: the reading joins the open batch numbered u64::MAX.
    assert_eq!(push(&mut builder, false).unwrap(), None);
    // A new batch would need a number after u64::MAX.
    let len = builder.open_len();
    assert_eq!(push(&mut builder, true), Err(PushError::SeqExhausted));
    assert_eq!(builder.open_len(), len);
    assert_eq!(builder.next_seq(), Some(last));
    let final_batch = builder.finish().unwrap();
    assert_eq!((final_batch.seq, final_batch.record_count()), (last, 2));
    assert_eq!(builder.next_seq(), None);
    assert_eq!(push(&mut builder, true), Err(PushError::SeqExhausted));
}

#[test]
fn randomized_splits_keep_every_rule() {
    for (round, limit) in [MIN_BATCH_LIMIT, 5_000, 10_000, 65_536, MAX_BULK_BYTES]
        .into_iter()
        .enumerate()
    {
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15 ^ u64::try_from(round).unwrap());
        let first = Seq::new(1 + rng.below(1_000)).unwrap();
        let mut builder = BatchBuilder::with_limit(common::stream(), first, limit).unwrap();
        let mut clock = true;
        let mut readings = Vec::new();
        let mut batches = Vec::new();
        for index in 0..5_000u64 {
            if rng.below(500) == 0 {
                clock = !clock;
            }
            let r = reading(
                device(u8::try_from(rng.below(8)).unwrap()),
                point(&format!("point_{}", rng.below(10))),
                at(index * 10),
                value(&mut rng),
                clock,
            );
            readings.push(r.clone());
            let before = builder.open_len();
            if let Some(closed) = builder.push(r).unwrap() {
                assert_eq!(Some(encode(&closed).unwrap().len()), before);
                batches.push(closed);
            }
            // The upload interval ends now and then.
            if rng.below(700) == 0 {
                batches.extend(builder.finish());
            }
        }
        batches.extend(builder.finish());
        check(&batches, &readings, first, limit);
    }
}

fn entry(index: u64, message_len: usize) -> LogEntry {
    LogEntry::new(
        at(index),
        if index.is_multiple_of(2) {
            LogLevel::Warn
        } else {
            LogLevel::Error
        },
        "edge_agent::modbus",
        "m".repeat(message_len),
    )
    .with_field("index", index.to_string())
}

#[test]
fn log_batches_split_by_the_channel_limit() {
    assert_eq!(LogBatch::split(Vec::new(), 0).unwrap(), Vec::new());
    let only_drops = LogBatch::split(Vec::new(), 5).unwrap();
    assert_eq!(only_drops.len(), 1);
    assert_eq!(only_drops[0].dropped, 5);
    assert!(encode(&only_drops[0]).is_ok());

    let entries: Vec<LogEntry> = (0..300).map(|index| entry(index, 4_000)).collect();
    let batches = LogBatch::split(entries.clone(), 17).unwrap();
    assert!(batches.len() > 3, "{}", batches.len());
    for (index, batch) in batches.iter().enumerate() {
        let size = encode(batch).unwrap().len();
        assert!(size <= Channel::Log.max_payload_bytes(), "{size}");
        assert_eq!(batch.dropped, if index == 0 { 17 } else { 0 });
    }
    let rejoined: Vec<LogEntry> = batches
        .into_iter()
        .flat_map(|batch| batch.entries)
        .collect();
    assert_eq!(rejoined, entries);
}

#[test]
fn log_dumps_split_into_numbered_parts() {
    let request = RequestId::new("req-1").unwrap();
    let empty =
        LogDump::split(request.clone(), LogCoverage::Unavailable, None, Vec::new()).unwrap();
    assert_eq!(empty.len(), 1);
    assert_eq!((empty[0].part, empty[0].last), (0, true));
    assert!(encode(&empty[0]).is_ok());

    // The largest entry the limits allow still fits a part.
    let mut largest = LogEntry::new(at(0), LogLevel::Trace, "t".repeat(200), "m".repeat(9_000));
    for field in 0..MAX_FIELDS + 5 {
        largest = largest.with_field(format!("{field:_<70}"), "v".repeat(2_000));
    }
    assert_eq!(largest.fields.len(), MAX_FIELDS);
    let mut entries: Vec<LogEntry> = (0..200).map(|index| entry(index, 3_000)).collect();
    entries.push(largest);
    let parts = LogDump::split(
        request.clone(),
        LogCoverage::Partial,
        Some(at(5)),
        entries.clone(),
    )
    .unwrap();
    assert!(parts.len() > 2, "{}", parts.len());
    for (index, part) in parts.iter().enumerate() {
        assert_eq!(part.part, u32::try_from(index).unwrap());
        assert_eq!(part.last, index + 1 == parts.len());
        assert_eq!(part.request, request);
        assert_eq!(part.coverage, LogCoverage::Partial);
        assert_eq!(part.available_from, Some(at(5)));
        let size = encode(part).unwrap().len();
        assert!(size <= Channel::LogDump.max_payload_bytes(), "{size}");
    }
    let rejoined: Vec<LogEntry> = parts.into_iter().flat_map(|part| part.entries).collect();
    assert_eq!(rejoined, entries);
}

#[test]
fn live_samples_split_by_the_channel_limit() {
    assert_eq!(LiveSamples::split(Vec::new()).unwrap(), Vec::new());

    // One poll of a large register map: many series, and one long series that has to continue
    // in the next message.
    let mut series: Vec<Series> = (0..900)
        .map(|index| Series {
            device: device(1),
            point: PointKey::new(format!("register_{index:04}")).unwrap(),
            samples: vec![Sample {
                at: at(0),
                value: Value::Float(f64::from(index) / 3.0),
            }],
        })
        .collect();
    series.push(Series {
        device: device(2),
        point: point("serial"),
        samples: (0..200)
            .map(|index| Sample {
                at: at(index),
                value: Value::Text("s".repeat(MAX_TEXT_BYTES)),
            })
            .collect(),
    });
    let messages = LiveSamples::split(series.clone()).unwrap();
    assert!(messages.len() > 3, "{}", messages.len());
    let limit = Channel::Live.max_payload_bytes();
    for message in &messages {
        let size = encode(message).unwrap().len();
        assert!(size <= limit, "{size}");
    }
    // Every message but the last is nearly full: the accounting is exact, not a rough estimate.
    for message in &messages[..messages.len() - 1] {
        let size = encode(message).unwrap().len();
        assert!(size > limit - 2 * MAX_TEXT_BYTES, "{size}");
    }
    // Rejoining the messages restores every series and its samples in order.
    let mut rejoined: Vec<Series> = Vec::new();
    for message in messages {
        for part in message.series {
            match rejoined.last_mut() {
                Some(last) if last.device == part.device && last.point == part.point => {
                    last.samples.extend(part.samples);
                }
                _ => rejoined.push(part),
            }
        }
    }
    assert_eq!(rejoined, series);
}
