//! Seeded random inputs for the splitters and the MQTT filter matcher.
//!
//! - Telemetry: whatever readings arrive, every batch fits its limit and decodes, sequence
//!   numbers are consecutive from the first one, never reused and never wrap (some rounds start
//!   next to `u64::MAX`), and no reading is lost.
//! - Logs and live values: every part fits its channel limit and decodes, order and content are
//!   preserved, `dropped` and part numbers follow the documented rules.
//! - Filters: `filter_matches` agrees with an independent MQTT 5 reference on every small
//!   filter and topic, and `filter_covers` never says an ACL covers a subscription that can
//!   match a topic the ACL does not.
//!
//! `FUZZ_SPLIT_ROUNDS` and `FUZZ_SEED` scale and move the random rounds.

#![allow(
    clippy::unwrap_used,
    reason = "test helpers unwrap like the tests themselves (clippy.toml allows it in tests)"
)]

use protocol::acl::{filter_covers, filter_matches};
use protocol::live::LiveSamples;
use protocol::logs::{LogBatch, LogCoverage, LogDump, LogEntry, LogLevel};
use protocol::telemetry::{
    BatchBuilder, MIN_BATCH_LIMIT, PushError, Reading, Sample, Series, TelemetryBatch,
};
use protocol::topic::{Channel, MAX_BULK_BYTES};
use protocol::{DeviceId, PointKey, RequestId, Seq, StreamId, UnixMillis, Value};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, bound: usize) -> usize {
        usize::try_from(self.next() % u64::try_from(bound.max(1)).unwrap()).unwrap()
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.next() % 100 < percent
    }

    fn pick<T: Copy>(&mut self, items: &[T]) -> T {
        items[self.below(items.len())]
    }
}

fn env_number(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|text| text.parse().ok())
        .unwrap_or(default)
}

// ---------------------------------------------------------------------------------------------
// MQTT filter matching, against an independent reference

/// MQTT 5 section 4.7 on level lists: does `filter` match `topic`?
fn reference_matches(filter: &[&str], topic: &[&str]) -> bool {
    match (filter.split_first(), topic.split_first()) {
        (Some((&"#", rest)), _) => rest.is_empty(),
        (Some((&"+", rest)), Some((_, topic_rest))) => reference_matches(rest, topic_rest),
        (Some((level, rest)), Some((topic_level, topic_rest))) if level == topic_level => {
            reference_matches(rest, topic_rest)
        }
        (None, None) => true,
        _ => false,
    }
}

fn level_lists(alphabet: &[&'static str], max_len: usize) -> Vec<Vec<&'static str>> {
    let mut lists: Vec<Vec<&'static str>> = Vec::new();
    let mut frontier: Vec<Vec<&'static str>> = vec![Vec::new()];
    for _ in 0..max_len {
        let mut next = Vec::new();
        for list in &frontier {
            for level in alphabet {
                let mut longer = list.clone();
                longer.push(level);
                next.push(longer);
            }
        }
        lists.extend(next.iter().cloned());
        frontier = next;
    }
    lists
}

#[test]
fn filter_matching_agrees_with_the_mqtt_rules_and_acl_coverage_is_sound() {
    // Filters use the levels a, b, + and # (# only last); topics use a, b, c and the empty level.
    let filters: Vec<Vec<&str>> = level_lists(&["a", "b", "+", "#"], 3)
        .into_iter()
        .filter(|filter| {
            filter
                .iter()
                .enumerate()
                .all(|(index, level)| *level != "#" || index + 1 == filter.len())
        })
        .collect();
    let topics = level_lists(&["a", "b", "c", ""], 4);
    for filter in &filters {
        let filter_text = filter.join("/");
        for topic in &topics {
            let topic_text = topic.join("/");
            assert_eq!(
                filter_matches(&filter_text, &topic_text),
                reference_matches(filter, topic),
                "filter {filter_text:?} topic {topic_text:?}"
            );
        }
    }
    // `filter_covers(acl, subscription)` must imply that every topic the subscription matches
    // is matched by the ACL: a gateway must never subscribe wider than its ACL.
    let mut covered = 0u64;
    for acl in &filters {
        let acl_text = acl.join("/");
        for subscription in &filters {
            let subscription_text = subscription.join("/");
            if !filter_covers(&acl_text, &subscription_text) {
                continue;
            }
            covered += 1;
            for topic in &topics {
                if reference_matches(subscription, topic) {
                    assert!(
                        reference_matches(acl, topic),
                        "{acl_text:?} covers {subscription_text:?} but not {topic:?}"
                    );
                }
            }
        }
    }
    assert!(covered > 100, "{covered}");
}

// ---------------------------------------------------------------------------------------------
// Splitting

fn random_id(rng: &mut Rng, pool: usize) -> String {
    const CHARS: &[u8] = b"abcXYZ019._-";
    if rng.chance(30) {
        // A long identifier, up to the limit.
        let len = 1 + rng.below(64);
        let mut text = String::from("a");
        text.extend((1..len).map(|_| char::from(CHARS[rng.below(CHARS.len())])));
        text
    } else {
        format!("d{}", rng.below(pool))
    }
}

fn random_value(rng: &mut Rng) -> Value {
    let finite =
        |value: f64, fallback: f64| Value::Float(if value.is_finite() { value } else { fallback });
    match rng.below(7) {
        0 => Value::Bool(rng.chance(50)),
        1 => Value::Int(i64::from_ne_bytes(rng.next().to_ne_bytes()) >> rng.below(64)),
        2 => Value::Int(rng.pick(&[i64::MIN, i64::MAX, 0, -1])),
        3 => finite(
            f64::from(f32::from_bits(u32::try_from(rng.next() >> 32).unwrap())),
            1.5,
        ),
        4 => finite(f64::from_bits(rng.next()), -0.0),
        5 => Value::Text("é".repeat(rng.below(513))),
        _ => Value::Text("x".repeat(rng.below(1025))),
    }
}

fn random_reading(rng: &mut Rng, pool: usize, clock_synced: bool) -> Reading {
    let random = rng.next();
    let at = rng.pick(&[0, 1, u64::MAX, random, 1_791_590_400_000]);
    Reading {
        device: DeviceId::new(random_id(rng, pool)).unwrap(),
        point: PointKey::new(random_id(rng, pool)).unwrap(),
        sample: Sample {
            at: UnixMillis::new(at),
            value: random_value(rng),
        },
        clock_synced,
    }
}

#[test]
fn random_readings_split_into_bounded_batches_with_consecutive_sequence_numbers() {
    let rounds = env_number("FUZZ_SPLIT_ROUNDS", 6);
    let mut rng = Rng(env_number("FUZZ_SEED", 11) ^ 0x5b11);
    let mut exhaustions = 0;
    for round in 0..rounds {
        let limit = if rng.chance(50) {
            MAX_BULK_BYTES
        } else {
            MIN_BATCH_LIMIT + rng.below(60_000)
        };
        // Some rounds start next to the end of the sequence space.
        let first = match rng.below(4) {
            0 => u64::MAX - u64::try_from(rng.below(4)).unwrap(),
            1 => 1,
            _ => 1 + rng.next() % 1_000_000,
        };
        let first = Seq::new(first).unwrap();
        let mut builder =
            BatchBuilder::with_limit(StreamId::new("s1").unwrap(), first, limit).unwrap();
        let pool = 1 + rng.below(12);
        let mut synced = true;
        let mut accepted = 0usize;
        let mut closed: Vec<TelemetryBatch> = Vec::new();
        let mut exhausted = false;
        for _ in 0..(50 + rng.below(1200)) {
            if rng.chance(3) {
                synced = !synced;
            }
            let reading = random_reading(&mut rng, pool, synced);
            let open_before = builder.open_len();
            match builder.push(reading) {
                Ok(batch) => {
                    accepted += 1;
                    closed.extend(batch);
                }
                Err(PushError::SeqExhausted) => {
                    exhausted = true;
                    // The builder is unchanged: the open batch is still there, unfinished.
                    assert_eq!(builder.open_len(), open_before, "round {round}");
                    assert_eq!(builder.next_seq(), Seq::new(u64::MAX), "round {round}");
                    break;
                }
                Err(error) => panic!("round {round}: {error}"),
            }
            if let Some(size) = builder.open_len() {
                assert!(size <= limit, "round {round}: open batch {size} > {limit}");
            }
        }
        if exhausted {
            exhaustions += 1;
            // Pushing more never wraps to a used number.
            for _ in 0..3 {
                let reading = random_reading(&mut rng, pool, synced);
                match builder.push(reading) {
                    Ok(None) => accepted += 1,
                    Ok(Some(_)) => panic!("round {round}: a batch closed after exhaustion"),
                    Err(PushError::SeqExhausted) => {}
                    Err(error) => panic!("round {round}: {error}"),
                }
            }
        }
        let open_size = builder.open_len();
        closed.extend(builder.finish());
        let mut records = 0;
        for (index, batch) in closed.iter().enumerate() {
            assert_eq!(
                batch.seq.get(),
                first.get() + u64::try_from(index).unwrap(),
                "round {round} batch {index}"
            );
            let bytes = batch.encode().unwrap();
            assert!(
                bytes.len() <= limit,
                "round {round}: {} > {limit}",
                bytes.len()
            );
            assert_eq!(
                TelemetryBatch::decode(&bytes).unwrap(),
                *batch,
                "round {round}"
            );
            records += batch.record_count();
            if index + 1 == closed.len() {
                assert_eq!(
                    open_size,
                    Some(bytes.len()),
                    "round {round}: open_len is not the size"
                );
            }
        }
        assert_eq!(records, accepted, "round {round}: readings lost or added");
    }
    assert!(
        exhaustions > 0 || rounds < 20,
        "no round reached the end of the sequence space"
    );
}

fn random_entry(rng: &mut Rng, index: u64) -> LogEntry {
    let size = rng.pick(&[0, 10, 100, 1000, 4096, 5000]);
    let mut entry = LogEntry::new(
        UnixMillis::new(index),
        rng.pick(&[LogLevel::Error, LogLevel::Warn, LogLevel::Info]),
        "t".repeat(rng.below(200)),
        "é".repeat(size / 2 + rng.below(3)),
    );
    for field in 0..rng.below(40) {
        entry = entry.with_field(
            format!("f{field}{}", "n".repeat(rng.below(70))),
            "v".repeat(rng.below(1100)),
        );
    }
    entry
}

#[test]
fn random_logs_and_live_values_split_within_their_limits() {
    let rounds = env_number("FUZZ_SPLIT_ROUNDS", 6);
    let mut rng = Rng(env_number("FUZZ_SEED", 13) ^ 0x1091);
    for round in 0..rounds {
        let entries: Vec<LogEntry> = (0..rng.below(120))
            .map(|index| random_entry(&mut rng, u64::try_from(index).unwrap()))
            .collect();
        let dropped = if rng.chance(50) { 0 } else { rng.next() };
        let batches = LogBatch::split(entries.clone(), dropped).unwrap();
        let mut flat = Vec::new();
        for (index, batch) in batches.iter().enumerate() {
            let bytes = batch.encode().unwrap();
            assert!(
                bytes.len() <= Channel::Log.max_payload_bytes(),
                "round {round}"
            );
            assert_eq!(LogBatch::decode(&bytes).unwrap(), *batch);
            assert_eq!(batch.dropped, if index == 0 { dropped } else { 0 });
            flat.extend(batch.entries.clone());
        }
        assert_eq!(flat, entries, "round {round}");

        let parts = LogDump::split(
            RequestId::new("r1").unwrap(),
            LogCoverage::Partial,
            Some(UnixMillis::new(5)),
            entries.clone(),
        )
        .unwrap();
        assert!(!parts.is_empty());
        let mut flat = Vec::new();
        for (index, part) in parts.iter().enumerate() {
            let bytes = part.encode().unwrap();
            assert!(
                bytes.len() <= Channel::LogDump.max_payload_bytes(),
                "round {round}"
            );
            assert_eq!(LogDump::decode(&bytes).unwrap(), *part);
            assert_eq!(usize::try_from(part.part).unwrap(), index);
            assert_eq!(part.last, index + 1 == parts.len());
            flat.extend(part.entries.clone());
        }
        assert_eq!(flat, entries, "round {round}");

        // Live values: series with many samples; distinct devices so that no key repeats.
        let pool = 1 + rng.below(6);
        let series: Vec<Series> = (0..1 + rng.below(8))
            .map(|n| Series {
                device: DeviceId::new(format!("d{n}")).unwrap(),
                point: PointKey::new(format!("p{}", rng.below(pool))).unwrap(),
                samples: (0..1 + rng.below(1500))
                    .map(|_| Sample {
                        at: UnixMillis::new(rng.next()),
                        value: random_value(&mut rng),
                    })
                    .collect(),
            })
            .collect();
        let messages = LiveSamples::split(series.clone()).unwrap();
        let mut seen = Vec::new();
        for message in &messages {
            let bytes = message.encode().unwrap();
            assert!(
                bytes.len() <= Channel::Live.max_payload_bytes(),
                "round {round}"
            );
            assert_eq!(
                LiveSamples::decode(&bytes).unwrap(),
                *message,
                "round {round}"
            );
            for entry in &message.series {
                for sample in &entry.samples {
                    seen.push((entry.device.clone(), entry.point.clone(), sample.clone()));
                }
            }
        }
        let expected: Vec<_> = series
            .iter()
            .flat_map(|entry| {
                entry
                    .samples
                    .iter()
                    .map(|sample| (entry.device.clone(), entry.point.clone(), sample.clone()))
            })
            .collect();
        assert_eq!(seen, expected, "round {round}");
    }
}
