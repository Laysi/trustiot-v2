//! Live values for open device charts (rebuild checklist I3).
//!
//! The cloud opens a lease per chart session with [`LiveAction::Start`], repeats the same
//! request as [`LiveAction::Renew`] while the chart stays open, and ends it with
//! [`LiveAction::Stop`]. A lease also ends by itself `ttl_s` seconds after its last start or
//! renewal, so a lost stop or a vanished browser cannot leave a gateway streaming. While any
//! lease covers a device, the gateway publishes each of its samples to `up/live` right away:
//! outside the buffer, without acknowledgement, at QoS 0. Leases are independent, so several
//! cloud replicas can hold leases on one gateway without coordinating.

use std::collections::HashSet;

use serde::de::{DeserializeOwned, IgnoredAny};
use serde::{Deserialize, Serialize};

use crate::codec::sealed::Token;
use crate::codec::{
    self, DecodeError, EncodeError, Invalid, IssueCode, Issues, Message, V1, encoded_len, head_len,
};
use crate::ids::{DeviceId, LeaseId};
use crate::telemetry::{Series, validate_series};
use crate::topic::Channel;

/// Most devices one lease covers.
pub const MAX_LIVE_DEVICES: usize = 64;
/// Shortest live sampling interval, in milliseconds.
pub const MIN_LIVE_INTERVAL_MS: u32 = 100;
/// Longest live sampling interval, in milliseconds.
pub const MAX_LIVE_INTERVAL_MS: u32 = 60_000;
/// Longest lease without renewal, in seconds.
pub const MAX_LIVE_TTL_S: u32 = 600;

/// `down/live-control`: starts, renews or stops one lease.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveControl {
    /// Wire format major version.
    pub v: V1,
    /// The lease.
    pub lease: LeaseId,
    /// What to do with it.
    pub action: LiveAction,
}

/// What to do with a lease. Encoded as a map whose `op` names the variant; [`crate::decode`]
/// reads the operation first and then only the fields of that variant.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum LiveAction {
    /// Opens the lease, or replaces it if it exists.
    Start(LiveSpec),
    /// Extends the lease; the gateway treats it exactly like `start`, so a renewal also
    /// restores a lease the gateway lost in a restart.
    Renew(LiveSpec),
    /// Ends the lease; ending an unknown lease does nothing.
    Stop,
    /// An operation this build does not know; the gateway ignores the message. Never sent.
    #[serde(other)]
    Unknown,
}

/// The devices a lease covers and how fast to sample them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveSpec {
    /// The devices, 1 to [`MAX_LIVE_DEVICES`], each once.
    #[serde(deserialize_with = "codec::seq")]
    pub devices: Vec<DeviceId>,
    /// The sampling interval: the tenant plan's shortest sampling interval (rebuild checklist
    /// I3), within [`MIN_LIVE_INTERVAL_MS`]..=[`MAX_LIVE_INTERVAL_MS`].
    pub interval_ms: u32,
    /// Seconds until the lease ends unless renewed, 1 to [`MAX_LIVE_TTL_S`].
    pub ttl_s: u32,
}

impl LiveControl {
    /// Encodes the control message; see [`codec::encode`].
    pub fn encode(&self) -> Result<Vec<u8>, EncodeError> {
        codec::encode(self)
    }

    /// Decodes a control message; see [`codec::decode`].
    pub fn decode(payload: &[u8]) -> Result<Self, DecodeError> {
        codec::decode(payload)
    }
}

impl Message for LiveControl {
    const CHANNEL: Channel = Channel::LiveControl;

    fn validate(&self) -> Result<(), Invalid> {
        let spec = match &self.action {
            LiveAction::Start(spec) | LiveAction::Renew(spec) => spec,
            LiveAction::Stop | LiveAction::Unknown => return Ok(()),
        };
        let mut issues = Issues::default();
        if spec.devices.is_empty() {
            issues.push(
                IssueCode::Empty,
                "action.devices",
                "a lease covers at least one device",
            );
        }
        if spec.devices.len() > MAX_LIVE_DEVICES {
            // Repeats are listed only within the limit. A 64 KiB payload holds about 32,000
            // two-byte device IDs; an issue for each repeat cost about 180 bytes per payload
            // byte (Laysi/trustiot-v2#290), and the list is refused anyway.
            issues.push(
                IssueCode::TooMany,
                "action.devices",
                format!("at most {MAX_LIVE_DEVICES} devices per lease"),
            );
        } else {
            let mut seen = HashSet::with_capacity(spec.devices.len());
            for (index, device) in spec.devices.iter().enumerate() {
                if !seen.insert(device) {
                    issues.push(
                        IssueCode::Duplicate,
                        format!("action.devices[{index}]"),
                        format!("device {device} is listed twice"),
                    );
                }
            }
        }
        if !(MIN_LIVE_INTERVAL_MS..=MAX_LIVE_INTERVAL_MS).contains(&spec.interval_ms) {
            issues.push(
                IssueCode::OutOfRange,
                "action.interval_ms",
                format!("must be within {MIN_LIVE_INTERVAL_MS}..={MAX_LIVE_INTERVAL_MS}"),
            );
        }
        if !(1..=MAX_LIVE_TTL_S).contains(&spec.ttl_s) {
            issues.push(
                IssueCode::OutOfRange,
                "action.ttl_s",
                format!("must be within 1..={MAX_LIVE_TTL_S}"),
            );
        }
        issues.finish()
    }

    fn validate_outgoing(&self) -> Result<(), Invalid> {
        if self.action == LiveAction::Unknown {
            return Err(Invalid::single(
                IssueCode::InvalidValue,
                "action.op",
                "an unknown operation cannot be sent",
            ));
        }
        Ok(())
    }

    /// Reads `action.op` first, then decodes the payload with the fields of the operation it
    /// names, so that no entry of the action map is buffered (Laysi/trustiot-v2#290). Both
    /// passes skip unknown keys without building anything; each refuses a repeated field, as a
    /// single pass would.
    fn decode_item(payload: &[u8], _: Token) -> Result<Self, DecodeError> {
        /// The operation names of [`LiveAction`], read like serde reads them for the derived
        /// enum: an unknown name is [`LiveAction::Unknown`].
        #[derive(Deserialize)]
        #[serde(field_identifier, rename_all = "snake_case")]
        enum Op {
            Start,
            Renew,
            Stop,
            #[serde(other)]
            Unknown,
        }

        #[derive(Deserialize)]
        struct Probe {
            action: ActionProbe,
        }

        #[derive(Deserialize)]
        struct ActionProbe {
            op: Op,
        }

        let Probe {
            action: ActionProbe { op },
        } = codec::from_slice(payload)?;
        match op {
            Op::Start => LiveWire::decode(payload, LiveAction::Start),
            Op::Renew => LiveWire::decode(payload, LiveAction::Renew),
            Op::Stop => LiveWire::decode(payload, |IgnoredAny| LiveAction::Stop),
            Op::Unknown => LiveWire::decode(payload, |IgnoredAny| LiveAction::Unknown),
        }
    }
}

/// A [`LiveControl`] whose action holds the fields `F` of one [`LiveAction`] variant: the
/// second decoding pass, once the operation is known. [`LiveWire::decode`] takes it apart and
/// builds the [`LiveControl`] field by field, so a field added to one and not the other does
/// not compile.
#[derive(Deserialize)]
struct LiveWire<F> {
    v: V1,
    lease: LeaseId,
    action: F,
}

impl<F: DeserializeOwned> LiveWire<F> {
    fn decode(
        payload: &[u8],
        action: impl FnOnce(F) -> LiveAction,
    ) -> Result<LiveControl, DecodeError> {
        let Self {
            v,
            lease,
            action: fields,
        } = codec::from_slice(payload)?;
        Ok(LiveControl {
            v,
            lease,
            action: action(fields),
        })
    }
}

/// `up/live`: samples of leased devices, published as they are taken.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LiveSamples {
    /// Wire format major version.
    pub v: V1,
    /// The samples, grouped like telemetry.
    #[serde(deserialize_with = "codec::seq")]
    pub series: Vec<Series>,
}

impl LiveSamples {
    /// Splits the samples of one poll into messages within the channel's size limit, keeping
    /// their order. A series that does not fit whole is continued in the next message. Returns
    /// no message for no samples.
    pub fn split(series: Vec<Series>) -> Result<Vec<Self>, EncodeError> {
        let limit = Self::CHANNEL.max_payload_bytes();
        let empty = encoded_len(&Self {
            v: V1,
            series: Vec::new(),
        })?;
        let mut messages = Vec::new();
        let mut open: Vec<Series> = Vec::new();
        let mut size = empty;
        for entry in series {
            let head = encoded_len(&Series {
                device: entry.device.clone(),
                point: entry.point.clone(),
                samples: Vec::new(),
            })? - head_len(0);
            // Whether the last series of `open` is this entry's continuation.
            let mut continuing = false;
            for sample in entry.samples {
                let sample_len = encoded_len(&sample)?;
                let growth = |open: &[Series], continuing: bool| match open.last() {
                    Some(last) if continuing => {
                        let count = last.samples.len();
                        sample_len + head_len(count + 1) - head_len(count)
                    }
                    _ => {
                        head + head_len(1) + sample_len + head_len(open.len() + 1)
                            - head_len(open.len())
                    }
                };
                let mut grown = size + growth(&open, continuing);
                if grown > limit && !open.is_empty() {
                    messages.push(Self {
                        v: V1,
                        series: std::mem::take(&mut open),
                    });
                    size = empty;
                    continuing = false;
                    grown = size + growth(&open, continuing);
                }
                if grown > limit {
                    return Err(EncodeError::TooLarge { size: grown, limit });
                }
                size = grown;
                match open.last_mut() {
                    Some(last) if continuing => last.samples.push(sample),
                    _ => open.push(Series {
                        device: entry.device.clone(),
                        point: entry.point.clone(),
                        samples: vec![sample],
                    }),
                }
                continuing = true;
            }
        }
        if !open.is_empty() {
            messages.push(Self {
                v: V1,
                series: open,
            });
        }
        Ok(messages)
    }

    /// Encodes the samples; see [`codec::encode`].
    pub fn encode(&self) -> Result<Vec<u8>, EncodeError> {
        codec::encode(self)
    }

    /// Decodes samples; see [`codec::decode`].
    pub fn decode(payload: &[u8]) -> Result<Self, DecodeError> {
        codec::decode(payload)
    }
}

impl Message for LiveSamples {
    const CHANNEL: Channel = Channel::Live;

    fn validate(&self) -> Result<(), Invalid> {
        let mut issues = Issues::default();
        if self.series.is_empty() {
            issues.push(IssueCode::Empty, "series", "at least one series");
        }
        validate_series(&self.series, "series", &mut issues);
        issues.finish()
    }
}
