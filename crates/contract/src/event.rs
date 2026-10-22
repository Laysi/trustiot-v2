//! Events that cross service boundaries (decision 4): a service writes them to its outbox in
//! the same transaction as the business data, and consumers process them idempotently.
//!
//! Every event travels in an [`EventEnvelope`] that names its type, payload version, tenant,
//! time, and producer. The payload schema of each type evolves by versions: a consumer built
//! with version N still decodes every older version, so stored events can be replayed after an
//! upgrade. The evolution rules are in this crate's `README.md`.

use chrono::{DateTime, SubsecRound, Utc};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::id::{EventId, TenantId, UserId};
use crate::json::JsonError;
use crate::naming::is_dotted_name;
use crate::service::ServiceName;

/// An event payload type. Implementations live in this crate, next to the other contracts of
/// the producing service.
///
/// ```
/// use contract::{Event, EventEnvelope, ServiceName, TenantId};
///
/// #[derive(serde::Serialize, serde::Deserialize)]
/// struct WidgetRenamed {
///     widget_id: String,
///     display_name: String,
/// }
///
/// impl Event for WidgetRenamed {
///     const TYPE: &'static str = "example.widget_renamed";
///     const VERSION: u32 = 1;
///     const PRODUCER: ServiceName = ServiceName::Assets;
/// }
///
/// let event = WidgetRenamed { widget_id: "w-1".into(), display_name: "Pump A".into() };
/// let envelope = EventEnvelope::new(&event, Some(TenantId::generate()))?;
/// assert_eq!(envelope.event_type, "example.widget_renamed");
/// # Ok::<(), contract::EncodeError>(())
/// ```
///
/// A malformed [`Event::TYPE`] fails the build where the event is first wrapped:
///
/// ```compile_fail,E0080
/// # use contract::{Event, EventEnvelope, ServiceName};
/// #[derive(serde::Serialize, serde::Deserialize)]
/// struct WidgetRenamed {
///     widget_id: String,
/// }
///
/// impl Event for WidgetRenamed {
///     const TYPE: &'static str = "WidgetRenamed";
///     const VERSION: u32 = 1;
///     const PRODUCER: ServiceName = ServiceName::Assets;
/// }
///
/// let _ = EventEnvelope::new(&WidgetRenamed { widget_id: "w-1".into() }, None);
/// ```
pub trait Event: Serialize + DeserializeOwned {
    /// The globally unique type name, `<domain>.<event>` in lower snake case, for example
    /// `identity.tenant_created`. Never renamed or reused.
    const TYPE: &'static str;

    /// The current payload version, starting at 1. Bump it for every incompatible change and
    /// teach [`Event::decode_payload`] the previous version.
    const VERSION: u32;

    /// The only service that produces this event.
    const PRODUCER: ServiceName;

    /// Decodes a payload written at `version`, which is between 1 and [`Event::VERSION`]. The
    /// default decodes the current version only; once an event has more than one version,
    /// override it to convert each older payload into the current type.
    ///
    /// # Errors
    ///
    /// [`DecodeError::UnknownVersion`] for a version without a decoder,
    /// [`DecodeError::PayloadNotAnObject`] when the payload is not a JSON object, and
    /// [`DecodeError::InvalidPayload`] when the payload does not match that version's schema.
    fn decode_payload(version: u32, payload: Value) -> Result<Self, DecodeError> {
        if version == Self::VERSION {
            parse_payload(Self::TYPE, version, payload)
        } else {
            Err(DecodeError::UnknownVersion {
                event_type: Self::TYPE,
                version,
            })
        }
    }
}

/// Parses one version of an event payload into `T`; the building block of
/// [`Event::decode_payload`] implementations.
///
/// # Errors
///
/// [`DecodeError::PayloadNotAnObject`] when the payload is not a JSON object (serde would
/// otherwise accept a positional array for a struct), and [`DecodeError::InvalidPayload`] when
/// it does not match `T`.
pub fn parse_payload<T: DeserializeOwned>(
    event_type: &'static str,
    version: u32,
    payload: Value,
) -> Result<T, DecodeError> {
    if !payload.is_object() {
        return Err(DecodeError::PayloadNotAnObject {
            event_type,
            version,
        });
    }
    serde_json::from_value(payload).map_err(|error| DecodeError::InvalidPayload {
        event_type,
        version,
        cause: JsonError::from(error),
    })
}

/// The wire form of an event, as stored in an outbox and delivered to consumers.
///
/// Consumers ignore envelope fields they do not know, so later fields are added as optional
/// ones.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EventEnvelope {
    /// Unique per event (UUIDv7); consumers deduplicate deliveries by it.
    pub id: EventId,
    /// [`Event::TYPE`] of the payload.
    #[serde(rename = "type")]
    pub event_type: String,
    /// [`Event::VERSION`] the payload was written with.
    pub version: u32,
    /// The tenant the event belongs to; `None` only for platform-wide events.
    pub tenant_id: Option<TenantId>,
    /// When the change happened, in UTC with millisecond precision.
    pub occurred_at: DateTime<Utc>,
    /// [`Event::PRODUCER`] of the payload.
    pub producer: ServiceName,
    /// The event itself, always a JSON object.
    pub payload: Value,
    /// Who made the change, when the producer says (Laysi/trustiot-v2#130): the event log keeps
    /// it as the operation log's "who". Absent in envelopes written before the field existed
    /// and for changes nobody in particular made.
    ///
    /// Decoded leniently: an actor this build cannot decode (a new kind, a service it does not
    /// know, a malformed user ID) becomes [`Actor::Unrecognized`] and never fails the envelope,
    /// so consumers that ignore the actor are never affected by it.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "lenient_actor"
    )]
    pub actor: Option<Actor>,
}

/// The envelope's `actor`: absent or `null` is `None`; any other value this build cannot
/// decode as an [`Actor`] is `Some(Actor::Unrecognized)`.
fn lenient_actor<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Actor>, D::Error> {
    let value = Option::<Value>::deserialize(deserializer)?;
    Ok(value.map(|value| serde_json::from_value(value).unwrap_or(Actor::Unrecognized)))
}

/// Who made the change an event describes; the operation log's "who" (checklist K2,
/// Laysi/trustiot-v2#130).
///
/// On the wire it is an object tagged by `kind`: `{"kind": "user", "user_id": "…"}` or
/// `{"kind": "service", "service": "assets"}`. A kind this build does not know decodes as
/// [`Actor::Unrecognized`] instead of failing, so a consumer built before a new kind was added
/// still decodes the envelope; only add kinds, never rename one. Inside an envelope, any actor
/// this build cannot decode (a service it does not know, a malformed user ID) is
/// `Unrecognized` too ([`EventEnvelope::actor`]).
///
/// ```
/// // `contract::Actor` is the internal token's `act` claim; this one is in `contract::event`.
/// use contract::UserId;
/// use contract::event::Actor;
///
/// let actor: Actor = serde_json::from_value(serde_json::json!({"kind": "robot"}))?;
/// assert_eq!(actor, Actor::Unrecognized);
/// assert!(serde_json::to_value(actor).is_err(), "Unrecognized is decode only");
/// let user = UserId::generate();
/// assert_eq!(
///     serde_json::to_value(Actor::User { user_id: user })?,
///     serde_json::json!({"kind": "user", "user_id": user.to_string()})
/// );
/// # Ok::<(), serde_json::Error>(())
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Actor {
    /// A person, through the API: the subject of the request's internal identity token. A
    /// platform administrator acting in the back office is one too.
    User {
        /// The user.
        user_id: UserId,
    },
    /// A service's own work with no person behind it: a scheduled job or an event consumer.
    /// The event log records a service actor only when it names the event's producer itself;
    /// any other service is recorded as no actor.
    Service {
        /// The service.
        service: ServiceName,
    },
    /// A kind this build does not know, written by a newer producer. Decode only: serializing
    /// it fails, so an envelope that carries it can never be written to an outbox (where the
    /// event log would retry it forever, waiting for an upgrade that never comes).
    #[serde(other, skip_serializing)]
    Unrecognized,
}

impl EventEnvelope {
    /// Wraps `event` in a new envelope with a fresh ID and the current time. Pass the tenant of
    /// the transaction that writes the event, or `None` for a platform-wide event.
    ///
    /// # Errors
    ///
    /// When the event does not serialize to a JSON object.
    pub fn new<E: Event>(event: &E, tenant_id: Option<TenantId>) -> Result<Self, EncodeError> {
        const {
            assert!(
                is_dotted_name(E::TYPE),
                "Event::TYPE must be `<domain>.<event>` in lower snake case"
            );
            assert!(E::VERSION >= 1, "Event::VERSION starts at 1");
        }
        let payload = serde_json::to_value(event).map_err(|error| EncodeError::Serialize {
            event_type: E::TYPE,
            cause: JsonError::from(error),
        })?;
        if !payload.is_object() {
            return Err(EncodeError::NotAnObject {
                event_type: E::TYPE,
            });
        }
        Ok(Self {
            id: EventId::generate(),
            event_type: E::TYPE.to_owned(),
            version: E::VERSION,
            tenant_id,
            occurred_at: Utc::now().trunc_subsecs(3),
            producer: E::PRODUCER,
            payload,
            actor: None,
        })
    }

    /// The same envelope with `actor` as the one who made the change.
    #[must_use]
    pub fn with_actor(mut self, actor: Actor) -> Self {
        self.actor = Some(actor);
        self
    }

    /// Whether the envelope holds an `E`, judged by its type name alone.
    #[must_use]
    pub fn is<E: Event>(&self) -> bool {
        self.event_type == E::TYPE
    }

    /// Decodes the payload as an `E`, upgrading an older version to the current one.
    ///
    /// # Errors
    ///
    /// When the envelope holds another type, comes from another producer, has a version this
    /// build does not know, or has a payload that is not a JSON object or does not match its
    /// version. Only [`DecodeError::NewerVersion`] is worth retrying, after the consumer is
    /// upgraded.
    pub fn decode<E: Event>(&self) -> Result<E, DecodeError> {
        self.check::<E>()?;
        E::decode_payload(self.version, self.payload.clone())
    }

    /// Like [`EventEnvelope::decode`], without copying the payload.
    ///
    /// # Errors
    ///
    /// As [`EventEnvelope::decode`].
    pub fn into_event<E: Event>(self) -> Result<E, DecodeError> {
        self.check::<E>()?;
        E::decode_payload(self.version, self.payload)
    }

    fn check<E: Event>(&self) -> Result<(), DecodeError> {
        if self.event_type != E::TYPE {
            return Err(DecodeError::WrongType {
                expected: E::TYPE,
                found: self.event_type.clone(),
            });
        }
        if self.producer != E::PRODUCER {
            return Err(DecodeError::WrongProducer {
                event_type: E::TYPE,
                expected: E::PRODUCER,
                found: self.producer,
            });
        }
        if self.version > E::VERSION {
            return Err(DecodeError::NewerVersion {
                event_type: E::TYPE,
                found: self.version,
                newest_known: E::VERSION,
            });
        }
        if self.version == 0 {
            return Err(DecodeError::UnknownVersion {
                event_type: E::TYPE,
                version: 0,
            });
        }
        // Checked here as well as in `parse_payload`, so a `decode_payload` override that
        // deserializes on its own cannot accept a positional array either.
        if !self.payload.is_object() {
            return Err(DecodeError::PayloadNotAnObject {
                event_type: E::TYPE,
                version: self.version,
            });
        }
        Ok(())
    }
}

/// An event could not be wrapped in an envelope.
#[derive(Debug, thiserror::Error)]
pub enum EncodeError {
    /// The event could not be serialized.
    #[error("event `{event_type}` cannot be serialized: {cause}")]
    Serialize {
        /// The event type.
        event_type: &'static str,
        /// What failed, without payload values.
        cause: JsonError,
    },
    /// The event serialized to something other than a JSON object, so fields could never be
    /// added to it compatibly.
    #[error("event `{event_type}` must serialize to a JSON object")]
    NotAnObject {
        /// The event type.
        event_type: &'static str,
    },
}

/// An envelope could not be decoded as the requested event.
#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    /// The envelope holds another event type.
    #[error("event type is `{found}`, expected `{expected}`")]
    WrongType {
        /// The requested type.
        expected: &'static str,
        /// The envelope's type.
        found: String,
    },
    /// The envelope names a producer that does not produce this event.
    #[error("event `{event_type}` comes from `{found}`, but only `{expected}` produces it")]
    WrongProducer {
        /// The event type.
        event_type: &'static str,
        /// The producer of the type.
        expected: ServiceName,
        /// The envelope's producer.
        found: ServiceName,
    },
    /// The payload was written by a newer producer than this build knows. Retry after the
    /// consumer is upgraded; do not discard the event.
    #[error(
        "event `{event_type}` version {found} is newer than this build knows ({newest_known}); \
         upgrade the consumer"
    )]
    NewerVersion {
        /// The event type.
        event_type: &'static str,
        /// The envelope's version.
        found: u32,
        /// [`Event::VERSION`] of this build.
        newest_known: u32,
    },
    /// No decoder exists for this version.
    #[error("event `{event_type}` has no decoder for version {version}")]
    UnknownVersion {
        /// The event type.
        event_type: &'static str,
        /// The envelope's version.
        version: u32,
    },
    /// The payload is not a JSON object, which every payload must be.
    #[error("event `{event_type}` version {version} has a payload that is not a JSON object")]
    PayloadNotAnObject {
        /// The event type.
        event_type: &'static str,
        /// The envelope's version.
        version: u32,
    },
    /// The payload does not match the schema of its version.
    #[error("event `{event_type}` version {version} has an invalid payload: {cause}")]
    InvalidPayload {
        /// The event type.
        event_type: &'static str,
        /// The envelope's version.
        version: u32,
        /// What failed, without payload values.
        cause: JsonError,
    },
}

impl DecodeError {
    /// Whether processing the same envelope can succeed later: only for
    /// [`DecodeError::NewerVersion`], once the consumer is upgraded. Every other error repeats
    /// on every delivery.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::NewerVersion { .. })
    }
}
