//! Artificial anomaly segments (checklist M7): a spike, an outage or a changeover inserted into a
//! dataset at times the caller chooses, each labelled with the triggers it is expected to cause.
//!
//! An anomaly comes from the index's `[[anomalies]]` table or from test code
//! ([`crate::Dataset::with_anomalies`]). Its `start` and `end` are on the dataset's time base and
//! move with the same [`TimeShift`] as the data. It changes the values of the readings of the
//! points it names whose reading time lies in `start..=end` (both ends included), and for a
//! cumulative point also the readings after it; no reading appears or disappears, so the index's
//! rows, batches and per-point counts stay true. What each kind does to each kind of point is in
//! `datasets/lab/README.md`, section 「人工異常片段」.
//!
//! The trigger names in `expect` are labels for later alarm and analysis work; nothing here
//! evaluates them.

use std::collections::BTreeSet;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::index::{Index, MAX_TEXT_BYTES, Point, PointKind, ValueType, check_key};
use crate::segment::{Reading, Value};
use crate::time::{self, TimeShift};

/// The largest integer every `f64` between it and its negation represents exactly.
const EXACT_FLOAT_INTEGER: i64 = 1 << 53;

/// An artificial anomaly segment and the triggers it is expected to cause.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Anomaly {
    /// A key of the dataset (`index::is_key`), unique among its anomalies.
    pub key: String,
    pub kind: AnomalyKind,
    /// The device whose points it changes.
    pub device: String,
    /// The points it changes; each must have a reading inside the window.
    pub points: Vec<String>,
    /// The first reading time it covers, on the dataset's time base (shifted with the data).
    #[serde(
        deserialize_with = "crate::time::deserialize",
        serialize_with = "crate::time::serialize"
    )]
    pub start: DateTime<Utc>,
    /// The last reading time it covers (inclusive).
    #[serde(
        deserialize_with = "crate::time::deserialize",
        serialize_with = "crate::time::serialize"
    )]
    pub end: DateTime<Utc>,
    /// Spike only: the value the points read inside the window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<Number>,
    /// Changeover only, and required when it names a text state point: the text that point
    /// reads inside the window, for example `setup`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    /// What it is expected to cause: at least one label.
    pub expect: Vec<Expectation>,
}

/// The kinds of artificial anomaly (checklist M7).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AnomalyKind {
    /// 突波: a gauge reads `value` for a short time.
    Spike,
    /// 停機: the device stops; it is still read, and its readings show it stopped.
    Outage,
    /// 換料: the device stops to be set up for another product; a text state reads `state`.
    Changeover,
}

impl AnomalyKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Spike => "spike",
            Self::Outage => "outage",
            Self::Changeover => "changeover",
        }
    }
}

/// A spike's value: an integer or a float, as written in the index.
#[derive(Clone, Copy, Debug, PartialEq, Deserialize, Serialize)]
#[serde(untagged)]
pub enum Number {
    Int(i64),
    Float(f64),
}

impl From<i64> for Number {
    fn from(value: i64) -> Self {
        Self::Int(value)
    }
}

impl From<f64> for Number {
    fn from(value: f64) -> Self {
        Self::Float(value)
    }
}

/// One expected trigger of an anomaly.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Expectation {
    /// The rule or detector, by key (`index::is_key`).
    pub trigger: String,
    /// Whether it is expected to fire.
    pub fires: bool,
}

impl Anomaly {
    fn new(
        kind: AnomalyKind,
        key: &str,
        device: &str,
        points: &[&str],
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Self {
        Self {
            key: key.to_owned(),
            kind,
            device: device.to_owned(),
            points: points.iter().map(|point| (*point).to_owned()).collect(),
            start,
            end,
            value: None,
            state: None,
            expect: Vec::new(),
        }
    }

    /// A spike: every reading of the gauge `points` in `start..=end` reads `value`.
    #[must_use]
    pub fn spike(
        key: &str,
        device: &str,
        points: &[&str],
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        value: impl Into<Number>,
    ) -> Self {
        Self {
            value: Some(value.into()),
            ..Self::new(AnomalyKind::Spike, key, device, points, start, end)
        }
    }

    /// An outage: the device's `points` show it stopped in `start..=end`.
    #[must_use]
    pub fn outage(
        key: &str,
        device: &str,
        points: &[&str],
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Self {
        Self::new(AnomalyKind::Outage, key, device, points, start, end)
    }

    /// A changeover: an outage whose text state `points` read `state` in `start..=end`.
    #[must_use]
    pub fn changeover(
        key: &str,
        device: &str,
        points: &[&str],
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        state: &str,
    ) -> Self {
        Self {
            state: Some(state.to_owned()),
            ..Self::new(AnomalyKind::Changeover, key, device, points, start, end)
        }
    }

    /// Adds an expected trigger: `trigger` is expected to fire (`fires`) or to stay quiet.
    #[must_use]
    pub fn expecting(mut self, trigger: &str, fires: bool) -> Self {
        self.expect.push(Expectation {
            trigger: trigger.to_owned(),
            fires,
        });
        self
    }

    /// Whether the reading time `ts` lies in the window.
    #[must_use]
    pub fn covers(&self, ts: DateTime<Utc>) -> bool {
        self.start <= ts && ts <= self.end
    }

    /// The anomaly with its window moved by `shift`, as it lies in a loaded dataset.
    ///
    /// # Errors
    ///
    /// When a moved time falls outside what ClickHouse holds.
    pub fn shifted(&self, shift: TimeShift) -> Result<Self, String> {
        Ok(Self {
            start: shift.apply(self.start)?,
            end: shift.apply(self.end)?,
            ..self.clone()
        })
    }

    /// The rules that need only the index: the device, its points and what this kind does to
    /// each, the parameters of the kind, and the labels.
    fn validate(&self, index: &Index) -> Result<(), String> {
        let key = self.key.as_str();
        check_key("anomaly", key)?;
        let device = index
            .device(&self.device)
            .ok_or_else(|| format!("anomaly `{key}`: device `{}` is not listed", self.device))?;
        if self.points.is_empty() {
            return Err(format!("anomaly `{key}` names no points"));
        }
        let mut seen = BTreeSet::new();
        let mut text_points = 0_usize;
        for name in &self.points {
            if !seen.insert(name.as_str()) {
                return Err(format!("anomaly `{key}`: point `{name}` is named twice"));
            }
            let point = device
                .points
                .iter()
                .find(|point| &point.key == name)
                .ok_or_else(|| {
                    format!(
                        "anomaly `{key}`: device `{}` has no point `{name}`",
                        self.device
                    )
                })?;
            if point.value_type == ValueType::String {
                text_points += 1;
            }
            self.effect(point)?;
        }
        if self.start > self.end {
            return Err(format!("anomaly `{key}` starts after it ends"));
        }
        match self.kind {
            AnomalyKind::Spike if self.state.is_some() => {
                return Err(format!(
                    "anomaly `{key}`: a spike takes `value`, not `state`"
                ));
            }
            AnomalyKind::Outage if self.value.is_some() || self.state.is_some() => {
                return Err(format!(
                    "anomaly `{key}`: an outage takes neither `value` nor `state`"
                ));
            }
            AnomalyKind::Changeover if self.value.is_some() => {
                return Err(format!(
                    "anomaly `{key}`: a changeover takes `state`, not `value`"
                ));
            }
            AnomalyKind::Changeover if self.state.is_some() && text_points == 0 => {
                return Err(format!(
                    "anomaly `{key}`: `state` is the value of a text state point, and none is named"
                ));
            }
            _ => {}
        }
        if self.expect.is_empty() {
            return Err(format!(
                "anomaly `{key}` lists no expected triggers (`expect`)"
            ));
        }
        let mut triggers = BTreeSet::new();
        for expectation in &self.expect {
            check_key(&format!("anomaly `{key}`: trigger"), &expectation.trigger)?;
            if !triggers.insert(expectation.trigger.as_str()) {
                return Err(format!(
                    "anomaly `{key}`: trigger `{}` is listed twice",
                    expectation.trigger
                ));
            }
        }
        Ok(())
    }

    /// What this anomaly does to `point`'s readings in the window.
    fn effect(&self, point: &Point) -> Result<Effect, String> {
        let key = self.key.as_str();
        let refuse = |what: &str| {
            Err(format!(
                "anomaly `{key}`: {what}; `{}` is a {} {} point",
                point.key,
                kind_name(point.kind),
                point.value_type.as_str()
            ))
        };
        match (self.kind, point.kind, point.value_type) {
            (
                AnomalyKind::Spike,
                PointKind::Gauge,
                value_type @ (ValueType::Float | ValueType::Int),
            ) => {
                let Some(value) = self.value else {
                    return Err(format!("anomaly `{key}`: a spike needs `value`"));
                };
                spike_value(key, value, value_type).map(Effect::Set)
            }
            (AnomalyKind::Spike, ..) => refuse("a spike changes gauge points of type float or int"),
            (_, PointKind::Gauge, ValueType::Float) => Ok(Effect::Set(Value::Float(0.0))),
            (_, PointKind::Gauge, ValueType::Int) => Ok(Effect::Set(Value::Int(0))),
            (_, PointKind::Cumulative, ValueType::Float | ValueType::Int) => Ok(Effect::Hold),
            (_, PointKind::State, ValueType::Bool) => Ok(Effect::Set(Value::Bool(false))),
            (AnomalyKind::Changeover, PointKind::State, ValueType::String) => match &self.state {
                Some(state) if !state.is_empty() && state.len() <= MAX_TEXT_BYTES => {
                    Ok(Effect::Set(Value::String(state.clone())))
                }
                Some(_) => Err(format!(
                    "anomaly `{key}`: `state` is 1 to {MAX_TEXT_BYTES} bytes"
                )),
                None => Err(format!(
                    "anomaly `{key}`: a changeover that names the text point `{}` needs `state`",
                    point.key
                )),
            },
            (AnomalyKind::Outage, PointKind::State, ValueType::String) => refuse(
                "an outage has no value for a text state (a changeover sets one with `state`)",
            ),
            _ => refuse("no stopped value is defined for this point"),
        }
    }
}

const fn kind_name(kind: PointKind) -> &'static str {
    match kind {
        PointKind::Gauge => "gauge",
        PointKind::Cumulative => "cumulative",
        PointKind::State => "state",
    }
}

fn spike_value(key: &str, value: Number, value_type: ValueType) -> Result<Value, String> {
    match (value, value_type) {
        (Number::Float(float), ValueType::Float) if float.is_finite() => Ok(Value::Float(float)),
        (Number::Int(int), ValueType::Float)
            if int.unsigned_abs() <= EXACT_FLOAT_INTEGER.unsigned_abs() =>
        {
            #[expect(
                clippy::cast_precision_loss,
                reason = "the magnitude is at most 2^53, which f64 holds exactly"
            )]
            Ok(Value::Float(int as f64))
        }
        (Number::Int(int), ValueType::Int) => Ok(Value::Int(int)),
        (Number::Float(_), ValueType::Int) => Err(format!(
            "anomaly `{key}`: an int point's spike value is an integer"
        )),
        _ => Err(format!(
            "anomaly `{key}`: the spike value is not a finite number a float holds exactly"
        )),
    }
}

/// What an anomaly does to one point's readings in its window.
enum Effect {
    /// Every reading in the window reads this value.
    Set(Value),
    /// A cumulative point stops counting in the window, and counts on from there afterwards.
    Hold,
}

/// Checks a dataset's anomalies against its index: each one's own rules, unique keys, and no two
/// windows that overlap on the same point.
///
/// # Errors
///
/// The first rule broken.
pub(crate) fn validate_all(index: &Index, anomalies: &[Anomaly]) -> Result<(), String> {
    let mut keys = BTreeSet::new();
    for anomaly in anomalies {
        anomaly.validate(index)?;
        if !keys.insert(anomaly.key.as_str()) {
            return Err(format!("anomaly `{}` appears twice", anomaly.key));
        }
    }
    for (position, a) in anomalies.iter().enumerate() {
        for b in &anomalies[position + 1..] {
            let shared = a.points.iter().find(|point| b.points.contains(point));
            if let Some(point) = shared
                && a.device == b.device
                && a.start <= b.end
                && b.start <= a.end
            {
                return Err(format!(
                    "anomalies `{}` and `{}` both change {}/{point} in overlapping windows",
                    a.key, b.key, a.device
                ));
            }
        }
    }
    Ok(())
}

/// Applies `anomalies` (already checked by [`validate_all`]) to `readings`, the whole dataset in
/// any order, in order of their start. A point keeps its readings; only values change.
///
/// # Errors
///
/// When a named point has no reading in an anomaly's window, or a cumulative point resets
/// inside an outage or changeover (the counter's level while stopped would be ambiguous).
pub(crate) fn apply(
    index: &Index,
    anomalies: &[Anomaly],
    readings: &mut [Reading],
) -> Result<(), String> {
    let mut ordered: Vec<&Anomaly> = anomalies.iter().collect();
    ordered.sort_by(|a, b| (a.start, &a.key).cmp(&(b.start, &b.key)));
    for anomaly in ordered {
        let device = index
            .device(&anomaly.device)
            .ok_or_else(|| format!("anomaly `{}`: unknown device", anomaly.key))?;
        for name in &anomaly.points {
            let point = device
                .points
                .iter()
                .find(|point| &point.key == name)
                .ok_or_else(|| format!("anomaly `{}`: unknown point `{name}`", anomaly.key))?;
            // This point's readings in reading-time order (arrival order differs with backfill).
            let mut series: Vec<usize> = readings
                .iter()
                .enumerate()
                .filter(|(_, reading)| reading.device == anomaly.device && &reading.point == name)
                .map(|(position, _)| position)
                .collect();
            series.sort_by_key(|&position| readings[position].ts);
            let first = series.partition_point(|&position| readings[position].ts < anomaly.start);
            let last = series.partition_point(|&position| readings[position].ts <= anomaly.end);
            if first == last {
                return Err(format!(
                    "anomaly `{}`: {}/{name} has no reading from {} to {}",
                    anomaly.key,
                    anomaly.device,
                    time::format(anomaly.start),
                    time::format(anomaly.end)
                ));
            }
            match anomaly.effect(point)? {
                Effect::Set(value) => {
                    for &position in &series[first..last] {
                        readings[position].value = value.clone();
                    }
                }
                Effect::Hold => hold(anomaly, point, readings, &series, first, last)?,
            }
        }
    }
    Ok(())
}

/// A cumulative point stops in `series[first..last]`: those readings hold the level it had
/// before the window (the reading before it, or the window's first reading when none comes
/// earlier), and what it counted inside the window is taken off every later reading, so that it
/// counts on from the held level and never runs backwards. A counter with `wraps_at` stays in
/// `0..wraps_at` (modular). Without `wraps_at`, a decrease is a reset: the readings after the
/// first reset following the window keep their values, since a reset restarts the count from
/// zero anyway; a reset inside the window is refused.
fn hold(
    anomaly: &Anomaly,
    point: &Point,
    readings: &mut [Reading],
    series: &[usize],
    first: usize,
    last: usize,
) -> Result<(), String> {
    let held_at = series[first.saturating_sub(1)];
    let reset_inside = || {
        format!(
            "anomaly `{}`: {}/{} resets inside the window; choose a window without a reset",
            anomaly.key, anomaly.device, point.key
        )
    };
    match (readings[held_at].value.clone(), point.wraps_at) {
        (Value::Int(held), Some(wraps_at)) => {
            let wraps_at = i128::from(wraps_at);
            let last_inside = int(&readings[series[last - 1]].value)?;
            let counted = (i128::from(last_inside) - i128::from(held)).rem_euclid(wraps_at);
            for &position in &series[first..last] {
                readings[position].value = Value::Int(held);
            }
            for &position in &series[last..] {
                let value = int(&readings[position].value)?;
                let moved = (i128::from(value) - counted).rem_euclid(wraps_at);
                readings[position].value =
                    Value::Int(i64::try_from(moved).map_err(|_| "internal: wrap".to_owned())?);
            }
        }
        (Value::Int(held), None) => {
            let mut previous = held;
            for &position in &series[first..last] {
                let value = int(&readings[position].value)?;
                if value < previous {
                    return Err(reset_inside());
                }
                previous = value;
                readings[position].value = Value::Int(held);
            }
            let counted = previous - held;
            for &position in &series[last..] {
                let value = int(&readings[position].value)?;
                if value < previous {
                    break;
                }
                previous = value;
                readings[position].value = Value::Int(value - counted);
            }
        }
        (Value::Float(held), _) => {
            let mut previous = held;
            for &position in &series[first..last] {
                let value = float(&readings[position].value)?;
                if value < previous {
                    return Err(reset_inside());
                }
                previous = value;
                readings[position].value = Value::Float(held);
            }
            let counted = previous - held;
            // Rounding of `value - counted` could land a last bit below the level before; the
            // counter never runs backwards, so it stays at least there.
            let mut level = held;
            for &position in &series[last..] {
                let value = float(&readings[position].value)?;
                if value < previous {
                    break;
                }
                previous = value;
                level = (value - counted).max(level);
                readings[position].value = Value::Float(level);
            }
        }
        _ => return Err(format!("anomaly `{}`: internal: not a number", anomaly.key)),
    }
    Ok(())
}

fn int(value: &Value) -> Result<i64, String> {
    match value {
        Value::Int(int) => Ok(*int),
        _ => Err("internal: an int point holds another type".to_owned()),
    }
}

fn float(value: &Value) -> Result<f64, String> {
    match value {
        Value::Float(float) => Ok(*float),
        _ => Err("internal: a float point holds another type".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::tests::VALID;

    fn at(minute: u32) -> DateTime<Utc> {
        time::parse(&format!("2026-01-05T00:{minute:02}:00.000Z")).unwrap()
    }

    fn reading(device: &str, point: &str, minute: u32, value: Value) -> Reading {
        Reading {
            stream: "live".to_owned(),
            batch_seq: 1,
            received_at: at(59),
            device: device.to_owned(),
            point: point.to_owned(),
            ts: at(minute),
            value,
        }
    }

    /// The base index with a gauge and a text state added to meter-2.
    fn index() -> Index {
        Index::parse(&VALID.replace(
            r#"points = [{ key = "running", type = "bool", kind = "state" }]"#,
            r#"points = [
  { key = "running", type = "bool", kind = "state" },
  { key = "mode", type = "string", kind = "state" },
  { key = "power_kw", type = "float", kind = "gauge" },
  { key = "strokes", type = "int", kind = "gauge" },
]"#,
        ))
        .unwrap()
    }

    fn values(readings: &[Reading], point: &str) -> Vec<(u32, Value)> {
        let mut found: Vec<(DateTime<Utc>, Value)> = readings
            .iter()
            .filter(|reading| reading.point == point)
            .map(|reading| (reading.ts, reading.value.clone()))
            .collect();
        found.sort_by_key(|(ts, _)| *ts);
        found
            .into_iter()
            .map(|(ts, value)| {
                let minute = u32::try_from((ts - at(0)).num_minutes()).unwrap();
                (minute, value)
            })
            .collect()
    }

    #[test]
    fn a_spike_sets_the_window_and_nothing_else() {
        let index = index();
        // Arrival order differs from reading time order.
        let mut readings: Vec<Reading> = [3, 0, 1, 2, 4]
            .into_iter()
            .map(|minute| {
                reading(
                    "meter-2",
                    "power_kw",
                    minute,
                    Value::Float(f64::from(minute)),
                )
            })
            .collect();
        let spike = Anomaly::spike("spike-1", "meter-2", &["power_kw"], at(1), at(2), 500_i64)
            .expecting("power-high", true);
        validate_all(&index, std::slice::from_ref(&spike)).unwrap();
        apply(&index, &[spike], &mut readings).unwrap();
        assert_eq!(
            values(&readings, "power_kw"),
            vec![
                (0, Value::Float(0.0)),
                (1, Value::Float(500.0)),
                (2, Value::Float(500.0)),
                (3, Value::Float(3.0)),
                (4, Value::Float(4.0)),
            ]
        );
    }

    #[test]
    fn an_outage_stops_every_kind_of_point() {
        let index = index();
        let mut readings = Vec::new();
        for minute in 0..6 {
            readings.push(reading("meter-2", "running", minute, Value::Bool(true)));
            readings.push(reading("meter-2", "power_kw", minute, Value::Float(40.0)));
            readings.push(reading("meter-2", "strokes", minute, Value::Int(7)));
            // 10, 12, 14, ... kWh
            readings.push(reading(
                "meter-1",
                "energy_kwh",
                minute,
                Value::Float(10.0 + 2.0 * f64::from(minute)),
            ));
        }
        let stop = Anomaly::outage(
            "stop-1",
            "meter-2",
            &["running", "power_kw", "strokes"],
            at(2),
            at(3),
        )
        .expecting("machine-stopped", true);
        let meter = Anomaly::outage("stop-2", "meter-1", &["energy_kwh"], at(2), at(3))
            .expecting("machine-stopped", true);
        let anomalies = [stop, meter];
        validate_all(&index, &anomalies).unwrap();
        apply(&index, &anomalies, &mut readings).unwrap();
        let running: Vec<bool> = values(&readings, "running")
            .into_iter()
            .map(|(_, value)| value == Value::Bool(true))
            .collect();
        assert_eq!(running, [true, true, false, false, true, true]);
        assert_eq!(values(&readings, "power_kw")[2].1, Value::Float(0.0));
        assert_eq!(values(&readings, "strokes")[3].1, Value::Int(0));
        assert_eq!(values(&readings, "strokes")[4].1, Value::Int(7));
        // Held at 12 (the minute-1 level) through minutes 2 and 3; the 4 kWh counted there are
        // taken off everything after, so the meter counts on by 2 per minute from 12.
        let energy: Vec<Value> = values(&readings, "energy_kwh")
            .into_iter()
            .map(|(_, value)| value)
            .collect();
        assert_eq!(
            energy,
            [10.0, 12.0, 12.0, 12.0, 14.0, 16.0].map(Value::Float)
        );
    }

    #[test]
    fn a_held_counter_wraps_and_stops_rebasing_at_a_reset() {
        let index = index();
        // pulses wraps at 65536: 65530, 65534, 2 (wrapped), 6, 10
        let pulses = [65_530, 65_534, 2, 6, 10];
        let mut readings: Vec<Reading> = pulses
            .iter()
            .zip(0..)
            .map(|(&value, minute)| reading("meter-1", "pulses", minute, Value::Int(value)))
            .collect();
        let stop = Anomaly::outage("stop-1", "meter-1", &["pulses"], at(1), at(2))
            .expecting("machine-stopped", true);
        apply(&index, &[stop], &mut readings).unwrap();
        // Held at 65530 for minutes 1 and 2 (8 pulses counted there, across the wrap), then
        // 6 - 8 and 10 - 8 modulo 65536.
        let held: Vec<Value> = values(&readings, "pulses")
            .into_iter()
            .map(|(_, value)| value)
            .collect();
        assert_eq!(held, [65_530, 65_530, 65_530, 65_534, 2].map(Value::Int));

        // A float counter resets to zero after the window: from there on it is untouched.
        let energy = [100.0, 101.5, 103.0, 104.0, 0.0, 1.0];
        let mut readings: Vec<Reading> = energy
            .iter()
            .zip(0..)
            .map(|(&value, minute)| reading("meter-1", "energy_kwh", minute, Value::Float(value)))
            .collect();
        let stop = Anomaly::outage("stop-1", "meter-1", &["energy_kwh"], at(1), at(2))
            .expecting("machine-stopped", true);
        apply(&index, &[stop], &mut readings).unwrap();
        let held: Vec<Value> = values(&readings, "energy_kwh")
            .into_iter()
            .map(|(_, value)| value)
            .collect();
        assert_eq!(
            held,
            [100.0, 100.0, 100.0, 101.0, 0.0, 1.0].map(Value::Float)
        );

        // A reset inside the window is refused.
        let mut readings: Vec<Reading> = [5.0, 0.0, 1.0]
            .iter()
            .zip(0..)
            .map(|(&value, minute)| reading("meter-1", "energy_kwh", minute, Value::Float(value)))
            .collect();
        let stop = Anomaly::outage("stop-1", "meter-1", &["energy_kwh"], at(0), at(1))
            .expecting("machine-stopped", true);
        let error = apply(&index, &[stop], &mut readings).unwrap_err();
        assert!(error.contains("resets inside the window"), "{error}");
    }

    #[test]
    fn a_held_float_counter_never_runs_backwards() {
        let index = index();
        // 100.3 - (100.3 - 0.1) is 0.09999999999999432 in f64: the first reading after the
        // window equals the last inside it, so it must come out at the held level, not below.
        let energy = [0.1, 100.3, 100.3, 101.0];
        assert!(energy[2] - (energy[1] - energy[0]) < energy[0]);
        let mut readings: Vec<Reading> = energy
            .iter()
            .zip(0..)
            .map(|(&value, minute)| reading("meter-1", "energy_kwh", minute, Value::Float(value)))
            .collect();
        let stop = Anomaly::outage("stop-1", "meter-1", &["energy_kwh"], at(1), at(1))
            .expecting("machine-stopped", true);
        apply(&index, &[stop], &mut readings).unwrap();
        let held: Vec<f64> = values(&readings, "energy_kwh")
            .into_iter()
            .map(|(_, value)| match value {
                Value::Float(float) => float,
                other => panic!("{other:?}"),
            })
            .collect();
        assert!(held.windows(2).all(|pair| pair[0] <= pair[1]), "{held:?}");
        assert_eq!(held[..3], [0.1, 0.1, 0.1]);
        assert!(held[3] > 0.1);
    }

    #[test]
    fn a_changeover_sets_the_text_state() {
        let index = index();
        let mut readings: Vec<Reading> = (0..4)
            .map(|minute| reading("meter-2", "mode", minute, Value::String("auto".to_owned())))
            .collect();
        let changeover =
            Anomaly::changeover("change-1", "meter-2", &["mode"], at(1), at(2), "setup")
                .expecting("changeover-detected", true)
                .expecting("machine-stopped", false);
        validate_all(&index, std::slice::from_ref(&changeover)).unwrap();
        apply(&index, &[changeover], &mut readings).unwrap();
        let modes: Vec<Value> = values(&readings, "mode")
            .into_iter()
            .map(|(_, value)| value)
            .collect();
        assert_eq!(
            modes,
            ["auto", "setup", "setup", "auto"].map(|mode| Value::String(mode.to_owned()))
        );
    }

    #[test]
    fn every_rule_refuses_its_violation() {
        let index = index();
        let base = || {
            Anomaly::spike("spike-1", "meter-2", &["power_kw"], at(1), at(2), 9.5)
                .expecting("power-high", true)
        };
        let cases: Vec<(Anomaly, &str)> = vec![
            (
                Anomaly {
                    key: "Spike".to_owned(),
                    ..base()
                },
                "not a key",
            ),
            (
                Anomaly {
                    device: "meter-9".to_owned(),
                    ..base()
                },
                "not listed",
            ),
            (
                Anomaly {
                    points: vec![],
                    ..base()
                },
                "names no points",
            ),
            (
                Anomaly {
                    points: vec!["power_kw".to_owned(), "power_kw".to_owned()],
                    ..base()
                },
                "named twice",
            ),
            (
                Anomaly {
                    points: vec!["missing".to_owned()],
                    ..base()
                },
                "has no point",
            ),
            (
                Anomaly {
                    start: at(3),
                    ..base()
                },
                "starts after it ends",
            ),
            (
                Anomaly {
                    expect: vec![],
                    ..base()
                },
                "no expected triggers",
            ),
            (base().expecting("power-high", false), "listed twice"),
            (base().expecting("Power High", true), "not a key"),
            (
                Anomaly {
                    value: None,
                    ..base()
                },
                "needs `value`",
            ),
            (
                Anomaly {
                    value: Some(Number::Float(f64::INFINITY)),
                    ..base()
                },
                "finite",
            ),
            (
                Anomaly {
                    value: Some(Number::Int(EXACT_FLOAT_INTEGER + 1)),
                    ..base()
                },
                "exactly",
            ),
            (
                Anomaly {
                    points: vec!["strokes".to_owned()],
                    ..base()
                },
                "is an integer",
            ),
            (
                Anomaly {
                    points: vec!["running".to_owned()],
                    ..base()
                },
                "gauge points",
            ),
            (
                Anomaly {
                    state: Some("x".to_owned()),
                    ..base()
                },
                "not `state`",
            ),
            (
                Anomaly::outage("stop-1", "meter-2", &["mode"], at(1), at(2))
                    .expecting("machine-stopped", true),
                "no value for a text state",
            ),
            (
                Anomaly {
                    value: Some(Number::Int(1)),
                    ..Anomaly::outage("stop-1", "meter-2", &["running"], at(1), at(2))
                        .expecting("machine-stopped", true)
                },
                "neither",
            ),
            (
                Anomaly {
                    state: None,
                    ..Anomaly::changeover("change-1", "meter-2", &["mode"], at(1), at(2), "x")
                        .expecting("changeover-detected", true)
                },
                "needs `state`",
            ),
            (
                Anomaly::changeover("change-1", "meter-2", &["running"], at(1), at(2), "x")
                    .expecting("changeover-detected", true),
                "none is named",
            ),
            (
                Anomaly::changeover("change-1", "meter-2", &["mode"], at(1), at(2), "")
                    .expecting("changeover-detected", true),
                "1 to",
            ),
        ];
        for (anomaly, expected) in cases {
            let error = validate_all(&index, std::slice::from_ref(&anomaly))
                .expect_err(&format!("{anomaly:?} was accepted"));
            assert!(error.contains(expected), "`{error}` lacks `{expected}`");
        }

        // Two anomalies: unique keys, and no overlap on one point.
        let twice = validate_all(&index, &[base(), base()]).unwrap_err();
        assert!(twice.contains("appears twice"), "{twice}");
        let overlapping = Anomaly {
            key: "spike-2".to_owned(),
            start: at(2),
            end: at(4),
            ..base()
        };
        let error = validate_all(&index, &[base(), overlapping.clone()]).unwrap_err();
        assert!(error.contains("overlapping"), "{error}");
        let after = Anomaly {
            start: at(3),
            ..overlapping
        };
        validate_all(&index, &[base(), after]).unwrap();
    }

    #[test]
    fn a_window_without_readings_is_refused() {
        let index = index();
        let mut readings = vec![reading("meter-2", "power_kw", 0, Value::Float(1.0))];
        let spike = Anomaly::spike("spike-1", "meter-2", &["power_kw"], at(1), at(2), 9.5)
            .expecting("power-high", true);
        let error = apply(&index, &[spike], &mut readings).unwrap_err();
        assert!(error.contains("has no reading"), "{error}");
    }

    #[test]
    fn index_anomalies_parse_and_shift() {
        let text = format!(
            "{VALID}{}",
            r#"
[[anomalies]]
key = "stop-1"
kind = "outage"
device = "meter-1"
points = ["energy_kwh", "pulses"]
start = "2026-01-05T00:00:30.000Z"
end = "2026-01-05T00:00:40.000Z"
expect = [{ trigger = "machine-stopped", fires = true }]

[[anomalies]]
key = "spike-1"
kind = "spike"
device = "meter-2"
points = ["running"]
start = "2026-01-05T00:00:30.000Z"
end = "2026-01-05T00:00:40.000Z"
value = 3
expect = [{ trigger = "power-high", fires = true }]
"#
        );
        let error = Index::parse(&text).unwrap_err();
        assert!(error.contains("gauge points"), "{error}");
        let index = Index::parse(
            text.split("\n[[anomalies]]\nkey = \"spike-1\"")
                .next()
                .unwrap(),
        )
        .unwrap();
        let anomaly = &index.anomalies[0];
        assert_eq!(anomaly.kind, AnomalyKind::Outage);
        assert_eq!(
            anomaly.expect,
            [Expectation {
                trigger: "machine-stopped".to_owned(),
                fires: true
            }]
        );
        let moved = anomaly.shifted(TimeShift::from_millis(86_400_000)).unwrap();
        assert_eq!(time::format(moved.start), "2026-01-06T00:00:30.000Z");
        assert_eq!(time::format(moved.end), "2026-01-06T00:00:40.000Z");
        assert_eq!(moved.expect, anomaly.expect);
    }
}
