//! The MQTT broker's dynamic-security configuration and the cloud services' broker accounts
//! (Laysi/trustiot-v2#84; deploy/chart/README.md, "MQTT broker").
//!
//! Gateways authenticate with their client certificate on the TLS listener, and Mosquitto takes
//! their username from the certificate's CN. Cloud services authenticate on the broker's internal
//! listener, which the mesh reaches only from the services the chart lists (AuthorizationPolicy
//! and NetworkPolicy by ServiceAccount), with a username and a password:
//!
//! - the username is `service:<service>` ([`service_username`]). A gateway ID cannot contain `:`
//!   (`protocol::GatewayId`), so no gateway certificate can carry a service's name;
//! - the password is generated in the cluster, never by a person (docs/architecture/secrets.md):
//!   [`converge`], run as `migrate mqtt-broker-accounts` by the broker pod's init container,
//!   fills the Secret the chart declares empty, one key per service, with the same rules as the
//!   migration Job fills its password Secrets (`passwords::fill`), and writes only a PBKDF2 hash
//!   of each password into the dynamic-security file ([`encode_password`]);
//! - each service gets the role [`service_role`], whose rules come from [`SERVICES`]: it receives
//!   the `up` channels it consumes and publishes to the `down` channels it sends, for every
//!   gateway. The certs service also gets [`DYNSEC_ADMIN_ROLE`], for the registration and
//!   revocation of gateway clients (Laysi/trustiot-v2#82).
//!
//! The init container runs before every start of the broker, so the dynamic-security file always
//! holds the roles of this release and a hash of the passwords the Secret holds; it keeps every
//! gateway client and anything else the broker stored ([`reconcile`]). The broker's default
//! access denies everything a role does not allow, and there is no anonymous group.

use std::collections::BTreeSet;
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Path;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use hmac::{Hmac, KeyInit, Mac};
use protocol::acl::{
    AclRule, AclType, GATEWAY_ALLOW_WILDCARD_SUBS, GATEWAY_ROLE, gateway_role_acls,
};
use protocol::topic::Channel;
use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Value, json};
use sha2::Sha512;

use crate::config::MqttAccountSettings;
use crate::passwords::{self, Access, Api, Hints};
use crate::secret::Password;
use crate::{Error, Report};

/// What one cloud service may do on the broker.
#[derive(Clone, Copy, Debug)]
pub struct ServiceAccess {
    /// The service's name under `services` in the chart, also its key in the accounts Secret.
    pub service: &'static str,
    /// The `up` channels it consumes, from every gateway.
    pub receives: &'static [Channel],
    /// The `down` channels it publishes, to every gateway.
    pub sends: &'static [Channel],
    /// Whether it may use the dynamic-security control topic ([`DYNSEC_ADMIN_ROLE`]).
    pub dynsec_admin: bool,
}

/// The cloud services that may hold a broker account, and what each may do. The senders follow
/// the topic table of crates/protocol/README.md; a service's issue changes its entry here.
pub const SERVICES: [ServiceAccess; 3] = [
    ServiceAccess {
        service: "ingest",
        receives: &[Channel::Telemetry, Channel::Log],
        sends: &[Channel::TelemetryAck],
        dynsec_admin: false,
    },
    ServiceAccess {
        service: "gateway-ops",
        receives: &[
            Channel::Status,
            Channel::Heartbeat,
            Channel::ConfigReport,
            Channel::Live,
            Channel::LogDump,
        ],
        sends: &[Channel::Config, Channel::LiveControl, Channel::LogRequest],
        // Not an administrator (decided 2026-10-23, docs/architecture/secrets.md): the admin role
        // controls the whole broker, and this service also carries live control and gateway data.
        // Laysi/trustiot-v2#82 sets this when its flow needs gateway-ops to manage clients.
        dynsec_admin: false,
    },
    ServiceAccess {
        service: "certs",
        receives: &[],
        sends: &[],
        dynsec_admin: true,
    },
];

/// Usernames of the services' accounts start with this; no gateway ID can (it has no `:`).
pub const SERVICE_USERNAME_PREFIX: &str = "service:";

/// Names of the services' roles start with this.
pub const SERVICE_ROLE_PREFIX: &str = "trustiot-service-";

/// The role that may send dynamic-security commands and read their responses: what Mosquitto's
/// own dynamic-security admin role allows, and nothing else. A holder can change every client and
/// role, so it is effectively in control of the broker.
pub const DYNSEC_ADMIN_ROLE: &str = "trustiot-dynsec-admin";

/// The dynamic-security control topic tree.
pub const DYNSEC_CONTROL_TOPIC: &str = "$CONTROL/dynamic-security/#";

/// PBKDF2 iterations of the password hashes, Mosquitto's default (`PW_DEFAULT_ITERATIONS`). The
/// passwords are 256 random bits, so the count does not protect them; it only has to be accepted.
pub const PASSWORD_ITERATIONS: u32 = 1000;

/// Salt and hash length of Mosquitto's `$7$` (PBKDF2-HMAC-SHA512) hashes.
const HASH_BYTES: usize = 64;

/// The service's entry in [`SERVICES`].
#[must_use]
pub fn service(name: &str) -> Option<&'static ServiceAccess> {
    SERVICES.iter().find(|access| access.service == name)
}

/// `service:<service>`.
#[must_use]
pub fn service_username(service: &str) -> String {
    format!("{SERVICE_USERNAME_PREFIX}{service}")
}

/// `trustiot-service-<service>`.
#[must_use]
pub fn service_role(service: &str) -> String {
    format!("{SERVICE_ROLE_PREFIX}{service}")
}

fn rule(acltype: AclType, topic: String) -> AclRule {
    AclRule {
        acltype,
        topic,
        priority: 0,
        allow: true,
    }
}

/// The rules of a service's role. For each channel it receives: subscribing to the channel's
/// filter for every gateway, as is or as a shared subscription of any group, unsubscribing from
/// it, and receiving its messages. For each channel it sends: publishing to it for every gateway.
#[must_use]
pub fn service_role_acls(access: &ServiceAccess) -> Vec<AclRule> {
    let mut rules = Vec::new();
    for channel in access.receives {
        let filter = channel.filter();
        let shared = format!("$share/+/{filter}");
        for topic in [&filter, &shared] {
            rules.push(rule(AclType::SubscribePattern, topic.clone()));
            rules.push(rule(AclType::UnsubscribePattern, topic.clone()));
        }
        rules.push(rule(AclType::PublishClientReceive, filter));
    }
    for channel in access.sends {
        rules.push(rule(AclType::PublishClientSend, channel.filter()));
    }
    rules
}

/// The rules of [`DYNSEC_ADMIN_ROLE`].
#[must_use]
pub fn dynsec_admin_acls() -> Vec<AclRule> {
    [
        AclType::PublishClientSend,
        AclType::PublishClientReceive,
        AclType::SubscribePattern,
        AclType::UnsubscribePattern,
    ]
    .into_iter()
    .map(|acltype| rule(acltype, DYNSEC_CONTROL_TOPIC.to_owned()))
    .collect()
}

/// The broker's default access: everything a role does not allow is denied, including receiving
/// (Mosquitto's own default lets a client receive whatever it managed to subscribe to) and
/// unsubscribing.
#[must_use]
pub fn default_acl_access() -> Value {
    json!({
        "publishClientSend": false,
        "publishClientReceive": false,
        "subscribe": false,
        "unsubscribe": false,
    })
}

fn hmac_sha512(key: &[u8], message: &[u8]) -> [u8; HASH_BYTES] {
    // HMAC accepts keys of any length, so this never fails.
    let mut mac = <Hmac<Sha512> as KeyInit>::new_from_slice(key)
        .unwrap_or_else(|_| unreachable!("HMAC-SHA-512 takes keys of any length"));
    mac.update(message);
    mac.finalize().into_bytes().into()
}

/// PBKDF2-HMAC-SHA512 with one 64-byte block, what Mosquitto computes for a `$7$` hash.
#[must_use]
pub fn pbkdf2_sha512(password: &[u8], salt: &[u8], iterations: u32) -> [u8; HASH_BYTES] {
    let mut first = salt.to_vec();
    first.extend_from_slice(&1_u32.to_be_bytes());
    let mut block = hmac_sha512(password, &first);
    let mut derived = block;
    for _ in 1..iterations {
        block = hmac_sha512(password, &block);
        for (out, byte) in derived.iter_mut().zip(block) {
            *out ^= byte;
        }
    }
    derived
}

/// Mosquitto's `encoded_password` for `password`: `$7$<iterations>$<salt>$<hash>`, with the salt
/// and the PBKDF2-HMAC-SHA512 hash in standard Base64 (libcommon/password_common.c).
#[must_use]
pub fn encode_password(password: &[u8], salt: &[u8; HASH_BYTES], iterations: u32) -> String {
    let hash = pbkdf2_sha512(password, salt, iterations);
    format!(
        "$7${iterations}${}${}",
        STANDARD.encode(salt),
        STANDARD.encode(hash)
    )
}

/// [`encode_password`] with a fresh salt from the operating system's CSPRNG.
///
/// # Errors
///
/// When there is no randomness.
pub fn hash_password(password: &Password) -> Result<String, Error> {
    let mut salt = [0_u8; HASH_BYTES];
    getrandom::fill(&mut salt).map_err(|error| Error::Random(error.to_string()))?;
    Ok(encode_password(
        password.expose().as_bytes(),
        &salt,
        PASSWORD_ITERATIONS,
    ))
}

/// The roles this release keeps in the file: the gateway role, the dynamic-security admin role
/// and one role per service in [`SERVICES`].
#[must_use]
pub fn managed_roles() -> Vec<Value> {
    let mut roles = vec![
        json!({
            "rolename": GATEWAY_ROLE,
            "textname": "TrustIoT gateway (chart-managed)",
            "allowwildcardsubs": GATEWAY_ALLOW_WILDCARD_SUBS,
            "acls": gateway_role_acls(),
        }),
        json!({
            "rolename": DYNSEC_ADMIN_ROLE,
            "textname": "Dynamic security administration (chart-managed)",
            "acls": dynsec_admin_acls(),
        }),
    ];
    roles.extend(SERVICES.iter().map(|access| {
        json!({
            "rolename": service_role(access.service),
            "textname": format!("TrustIoT service {} (chart-managed)", access.service),
            "acls": service_role_acls(access),
        })
    }));
    roles
}

fn is_managed_role(name: &str) -> bool {
    name == GATEWAY_ROLE || name == DYNSEC_ADMIN_ROLE || name.starts_with(SERVICE_ROLE_PREFIX)
}

/// The client of one service account, with the hash of its password.
fn service_client(access: &ServiceAccess, encoded_password: String) -> Value {
    let mut roles = vec![json!({ "rolename": service_role(access.service), "priority": 0 })];
    if access.dynsec_admin {
        roles.push(json!({ "rolename": DYNSEC_ADMIN_ROLE, "priority": 0 }));
    }
    json!({
        "username": service_username(access.service),
        "textname": format!("TrustIoT service {} (chart-managed)", access.service),
        "encoded_password": encoded_password,
        "roles": roles,
    })
}

/// The objects of `root[key]`, an array; an absent key is an empty one.
fn array_of(root: &mut Map<String, Value>, key: &str) -> Result<Vec<Value>, String> {
    match root.remove(key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => Ok(items),
        Some(_) => Err(format!("`{key}` is not an array")),
    }
}

fn string_field<'a>(item: &'a Value, key: &str) -> Option<&'a str> {
    item.get(key).and_then(Value::as_str)
}

/// A JSON value in which no object repeats a key. serde_json keeps the last of two equal keys and
/// Mosquitto's parser (cJSON) the first, so a file with a repeated key would be read one way here
/// and another way by the broker: [`reconcile`] refuses it instead of guessing.
struct Unrepeated(Value);

impl<'de> Deserialize<'de> for Unrepeated {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer
            .deserialize_any(UnrepeatedVisitor)
            .map(Unrepeated)
    }
}

struct UnrepeatedVisitor;

impl<'de> Visitor<'de> for UnrepeatedVisitor {
    type Value = Value;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a JSON value")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Value, E> {
        Ok(Value::Bool(value))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Value, E> {
        Ok(Value::from(value))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Value, E> {
        Ok(Value::from(value))
    }

    fn visit_f64<E>(self, value: f64) -> Result<Value, E> {
        Ok(serde_json::Number::from_f64(value).map_or(Value::Null, Value::Number))
    }

    fn visit_str<E>(self, value: &str) -> Result<Value, E> {
        Ok(Value::String(value.to_owned()))
    }

    fn visit_string<E>(self, value: String) -> Result<Value, E> {
        Ok(Value::String(value))
    }

    fn visit_unit<E>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
        let mut items = Vec::new();
        while let Some(Unrepeated(item)) = seq.next_element()? {
            items.push(item);
        }
        Ok(Value::Array(items))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
        let mut object = Map::new();
        while let Some(key) = map.next_key::<String>()? {
            if object.contains_key(&key) {
                return Err(de::Error::custom(format_args!(
                    "an object repeats the key {key:?}"
                )));
            }
            let Unrepeated(value) = map.next_value()?;
            object.insert(key, value);
        }
        Ok(Value::Object(object))
    }
}

/// The type dynamic security expects of a field it reads.
#[derive(Clone, Copy)]
enum Kind {
    /// A non-empty string: a name the broker looks entries up by.
    Name,
    /// A string.
    Text,
    /// `true` or `false`.
    Bool,
    /// A number.
    Number,
    /// Any value: the release replaces or removes the field.
    Replaced,
}

impl Kind {
    fn accepts(self, value: &Value) -> bool {
        match self {
            Self::Name => value.as_str().is_some_and(|text| !text.is_empty()),
            Self::Text => value.is_string(),
            Self::Bool => value.is_boolean(),
            Self::Number => value.is_number(),
            Self::Replaced => true,
        }
    }

    fn expected(self) -> &'static str {
        match self {
            Self::Name => "a non-empty string",
            Self::Text => "a string",
            Self::Bool => "true or false",
            Self::Number => "a number",
            Self::Replaced => "anything",
        }
    }
}

/// The fields dynamic security reads (Mosquitto 2.1, plugins/dynamic-security: config.c,
/// roles.c, clients.c, groups.c) and the type it reads each with. `roles`, `clients`, `groups`
/// and `acls` are arrays, which [`check_kept`] walks.
const ROOT_FIELDS: &[(&str, Kind)] = &[
    ("changeIndex", Kind::Number),
    ("defaultACLAccess", Kind::Replaced),
    ("anonymousGroup", Kind::Replaced),
    ("roles", Kind::Replaced),
    ("clients", Kind::Replaced),
    ("groups", Kind::Replaced),
];
const ROLE_FIELDS: &[(&str, Kind)] = &[
    ("rolename", Kind::Name),
    ("textname", Kind::Text),
    ("textdescription", Kind::Text),
    ("allowwildcardsubs", Kind::Bool),
    ("acls", Kind::Replaced),
];
const ACL_FIELDS: &[(&str, Kind)] = &[
    ("acltype", Kind::Name),
    ("topic", Kind::Name),
    ("priority", Kind::Number),
    ("allow", Kind::Bool),
];
const CLIENT_FIELDS: &[(&str, Kind)] = &[
    ("username", Kind::Name),
    ("clientid", Kind::Text),
    ("textname", Kind::Text),
    ("textdescription", Kind::Text),
    ("disabled", Kind::Bool),
    ("encoded_password", Kind::Text),
    ("password", Kind::Text),
    ("salt", Kind::Text),
    ("iterations", Kind::Number),
    ("roles", Kind::Replaced),
];
const GROUP_FIELDS: &[(&str, Kind)] = &[
    ("groupname", Kind::Name),
    ("textname", Kind::Text),
    ("textdescription", Kind::Text),
    ("roles", Kind::Replaced),
    ("clients", Kind::Replaced),
];
const ROLE_REFERENCE_FIELDS: &[(&str, Kind)] =
    &[("rolename", Kind::Name), ("priority", Kind::Number)];
const CLIENT_REFERENCE_FIELDS: &[(&str, Kind)] =
    &[("username", Kind::Name), ("priority", Kind::Number)];

/// The ACL types a role's rule can have; the broker compares them ignoring ASCII case.
const ACL_TYPES: [&str; 6] = [
    "publishClientSend",
    "publishClientReceive",
    "subscribeLiteral",
    "subscribePattern",
    "unsubscribeLiteral",
    "unsubscribePattern",
];

/// Checks one object of the file the way the broker reads it. Dynamic security looks a field up
/// ignoring ASCII case and takes the first match (cJSON_GetObjectItem), and reads a field of
/// another type as absent; fields it does not read are kept as they are.
fn check_object(
    object: &Map<String, Value>,
    fields: &[(&str, Kind)],
    required: &[&str],
    at: &str,
) -> Result<(), String> {
    let mut folded: Vec<(String, &str)> = Vec::new();
    for key in object.keys() {
        let lower = key.to_ascii_lowercase();
        if let Some((_, other)) = folded.iter().find(|(seen, _)| *seen == lower) {
            return Err(format!(
                "{at} has both `{other}` and `{key}`, which the broker reads as one field"
            ));
        }
        folded.push((lower, key));
    }
    for key in object.keys() {
        if let Some((field, _)) = fields
            .iter()
            .find(|(field, _)| field.eq_ignore_ascii_case(key) && field != key)
        {
            return Err(format!("{at} spells `{field}` as `{key}`"));
        }
    }
    for (field, kind) in fields {
        match object.get(*field) {
            None if required.contains(field) => {
                return Err(format!("{at} has no `{field}`"));
            }
            Some(value) if !kind.accepts(value) => {
                return Err(format!("{at}: `{field}` is not {}", kind.expected()));
            }
            _ => {}
        }
    }
    Ok(())
}

/// [`check_object`] for an entry of an array.
fn check_entry<'a>(
    item: &'a Value,
    fields: &[(&str, Kind)],
    required: &[&str],
    at: &str,
) -> Result<&'a Map<String, Value>, String> {
    let object = item
        .as_object()
        .ok_or_else(|| format!("{at} is not a JSON object"))?;
    check_object(object, fields, required, at)?;
    Ok(object)
}

/// The array `object[key]`, checked entry by entry with `check`; an absent key is an empty one.
fn check_array(
    object: &Map<String, Value>,
    key: &str,
    at: &str,
    mut check: impl FnMut(&Value, &str) -> Result<(), String>,
) -> Result<(), String> {
    match object.get(key) {
        None => Ok(()),
        Some(Value::Array(items)) => items
            .iter()
            .enumerate()
            .try_for_each(|(index, item)| check(item, &format!("{at}, `{key}` entry {index}"))),
        Some(_) => Err(format!("{at}: `{key}` is not an array")),
    }
}

/// Adds `name` to the names of one array, which must not have it yet: the broker keeps the first
/// entry of a name and drops the others without a word.
fn unique<'a>(names: &mut BTreeSet<&'a str>, name: &'a str, at: &str) -> Result<(), String> {
    if names.insert(name) {
        Ok(())
    } else {
        Err(format!(
            "{at} repeats the name `{name}`; the broker would keep only the first entry with it"
        ))
    }
}

/// The label of entry `index` of `array` in an error, with the entry's name when it has one.
fn entry_label(array: &str, index: usize, item: &Value, name_field: &str) -> String {
    match string_field(item, name_field) {
        Some(name) if !name.is_empty() => format!("`{array}` entry {index} (`{name}`)"),
        _ => format!("`{array}` entry {index}"),
    }
}

/// Checks a role kept from the broker's file.
fn check_role<'a>(role: &'a Value, at: &str, names: &mut BTreeSet<&'a str>) -> Result<(), String> {
    let object = check_entry(role, ROLE_FIELDS, &["rolename"], at)?;
    unique(
        names,
        string_field(role, "rolename").unwrap_or_default(),
        at,
    )?;
    let mut rules = BTreeSet::new();
    check_array(object, "acls", at, |rule, at| {
        check_entry(rule, ACL_FIELDS, &["acltype", "topic"], at)?;
        let acltype = string_field(rule, "acltype").unwrap_or_default();
        let Some(known) = ACL_TYPES
            .iter()
            .find(|known| known.eq_ignore_ascii_case(acltype))
        else {
            return Err(format!(
                "{at}: `acltype` {acltype:?} is none of {}",
                ACL_TYPES.join(", ")
            ));
        };
        let topic = string_field(rule, "topic").unwrap_or_default();
        if rules.insert((*known, topic.to_owned())) {
            Ok(())
        } else {
            Err(format!(
                "{at} repeats the {known} rule for {topic:?}; the broker would keep only the first"
            ))
        }
    })
}

/// Checks the role references of a client or group kept from the broker's file.
fn check_role_references(object: &Map<String, Value>, at: &str) -> Result<(), String> {
    check_array(object, "roles", at, |reference, at| {
        check_entry(reference, ROLE_REFERENCE_FIELDS, &["rolename"], at).map(|_| ())
    })
}

/// Checks the roles, clients and groups kept from the broker's file (the release writes its own
/// entries fresh): each must be one the broker loads as it is written. Dynamic security skips an
/// entry without its name, with a name an earlier entry has, or with an ACL type it does not
/// know, and reads a field of another type, or one whose name differs in case from another, as
/// something else. It would then drop a deny rule, a client's `disabled` flag or client ID, or
/// let a hand-edited entry shadow one of the release's, without a word in its log.
fn check_kept(
    roles: &[(usize, Value)],
    clients: &[(usize, Value)],
    groups: &[(usize, Value)],
) -> Result<(), String> {
    let mut names = BTreeSet::new();
    for (index, role) in roles {
        check_role(
            role,
            &entry_label("roles", *index, role, "rolename"),
            &mut names,
        )?;
    }
    let mut names = BTreeSet::new();
    for (index, client) in clients {
        let at = entry_label("clients", *index, client, "username");
        let object = check_entry(client, CLIENT_FIELDS, &["username"], &at)?;
        unique(
            &mut names,
            string_field(client, "username").unwrap_or_default(),
            &at,
        )?;
        check_role_references(object, &at)?;
    }
    let mut names = BTreeSet::new();
    for (index, group) in groups {
        let at = entry_label("groups", *index, group, "groupname");
        let object = check_entry(group, GROUP_FIELDS, &["groupname"], &at)?;
        unique(
            &mut names,
            string_field(group, "groupname").unwrap_or_default(),
            &at,
        )?;
        check_role_references(object, &at)?;
        check_array(object, "clients", &at, |reference, at| {
            check_entry(reference, CLIENT_REFERENCE_FIELDS, &["username"], at).map(|_| ())
        })?;
    }
    Ok(())
}

/// The dynamic-security file with this release's access rules and service accounts.
///
/// `existing` is the file the broker kept (None on a fresh volume); `accounts` holds each service
/// that has an account and the `encoded_password` of its password. The result has the default
/// access of [`default_acl_access`], no anonymous group, the roles of [`managed_roles`] in place
/// of any older version of them (and no other `trustiot-service-*` role), and exactly the service
/// clients of `accounts`, which no group lists. Everything else is kept as the broker stored it:
/// the gateway clients, their groups, and the broker's own fields.
///
/// # Errors
///
/// When `existing` is not a JSON object with arrays where dynamic security keeps them, or an
/// entry it keeps is not one the broker would load as it is written ([`check_kept`]). The file is
/// then left alone, and the broker does not start: replacing the file would drop every gateway
/// client, and the broker would refuse it, or skip or misread the entry without a word.
pub fn reconcile(
    existing: Option<&str>,
    accounts: &[(&ServiceAccess, String)],
) -> Result<String, String> {
    let mut root = match existing {
        None => Map::new(),
        Some(text) => match serde_json::from_str::<Unrepeated>(text) {
            Ok(Unrepeated(Value::Object(root))) => root,
            Ok(_) => return Err("it is not a JSON object".to_owned()),
            // Only Unrepeated raises a data error: every JSON value is accepted otherwise.
            Err(error) if error.is_data() => {
                return Err(format!(
                    "{error}; the broker would read the first of the two, this tool the last"
                ));
            }
            Err(error) => return Err(format!("it is not valid JSON: {error}")),
        },
    };
    check_object(&root, ROOT_FIELDS, &[], "the file")?;
    root.insert("defaultACLAccess".to_owned(), default_acl_access());
    root.remove("anonymousGroup");

    // What the release manages is replaced and needs no check; the rest is kept, at its index.
    let kept_roles: Vec<(usize, Value)> = array_of(&mut root, "roles")?
        .into_iter()
        .enumerate()
        .filter(|(_, role)| !string_field(role, "rolename").is_some_and(is_managed_role))
        .collect();
    let kept_clients: Vec<(usize, Value)> = array_of(&mut root, "clients")?
        .into_iter()
        .enumerate()
        .filter(|(_, client)| {
            !string_field(client, "username")
                .is_some_and(|name| name.starts_with(SERVICE_USERNAME_PREFIX))
        })
        .collect();
    // Groups stay as they are; the file must have the array for the broker to keep its own.
    let groups: Vec<(usize, Value)> = array_of(&mut root, "groups")?
        .into_iter()
        .enumerate()
        .collect();
    check_kept(&kept_roles, &kept_clients, &groups)?;

    let mut roles: Vec<Value> = kept_roles.into_iter().map(|(_, role)| role).collect();
    roles.extend(managed_roles());
    root.insert("roles".to_owned(), Value::Array(roles));

    let mut clients: Vec<Value> = kept_clients.into_iter().map(|(_, client)| client).collect();
    clients.extend(
        accounts
            .iter()
            .map(|(access, encoded)| service_client(access, encoded.clone())),
    );
    root.insert("clients".to_owned(), Value::Array(clients));

    // A service account holds exactly the roles of its entry in SERVICES, so no group lists it.
    let groups: Vec<Value> = groups
        .into_iter()
        .map(|(_, mut group)| {
            if let Some(Value::Array(members)) = group.get_mut("clients") {
                members.retain(|member| {
                    !string_field(member, "username")
                        .is_some_and(|name| name.starts_with(SERVICE_USERNAME_PREFIX))
                });
            }
            group
        })
        .collect();
    root.insert("groups".to_owned(), Value::Array(groups));

    serde_json::to_string_pretty(&Value::Object(root))
        .map_err(|error| format!("cannot serialize it: {error}"))
}

/// The services named in `list` (comma-separated, may be empty).
///
/// # Errors
///
/// When a name is not in [`SERVICES`] or appears twice.
pub fn parse_services(list: &str) -> Result<Vec<&'static ServiceAccess>, String> {
    let mut seen = BTreeSet::new();
    let mut services = Vec::new();
    for name in list
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
    {
        let access = service(name).ok_or_else(|| {
            format!(
                "lists a service that may not hold a broker account (those that may: {})",
                SERVICES
                    .iter()
                    .map(|access| access.service)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;
        if !seen.insert(name) {
            return Err("lists a service twice".to_owned());
        }
        services.push(access);
    }
    Ok(services)
}

/// The init container's hints: it fills the Secret with the broker pod's ServiceAccount.
const INIT_HINTS: Hints = Hints {
    missing: "does not exist. The chart declares it without data and the MQTT broker's init \
              container fills it: run helm upgrade, which creates it again",
    refused: "the chart's Role lets the MQTT broker's ServiceAccount get and patch exactly this \
              Secret",
};

/// The init container: makes sure the Secret holds a password of the requested generation for
/// every service in `settings.services`, and writes the dynamic-security file with their hashes
/// ([`reconcile`]; mode 0600, replaced atomically).
///
/// # Errors
///
/// When the settings name an unknown service, the Secret is missing or being deleted, the API
/// refuses access or does not answer within `settings.wait`, there is no randomness, or the file
/// cannot be read, understood or written. The pod then restarts the init container, and the
/// broker does not start.
pub async fn converge(settings: &MqttAccountSettings, report: &mut Report) -> Result<(), Error> {
    let services = parse_services(&settings.services).map_err(Error::MqttBroker)?;
    let requested = passwords::parse_generation(&settings.generation).ok_or_else(|| {
        Error::MqttBroker(format!(
            "TRUSTIOT__ACCOUNTS__GENERATION: {:?} is not a password generation (1 to 9 decimal \
             digits)",
            settings.generation
        ))
    })?;
    let mut accounts = Vec::new();
    if !services.is_empty() {
        let access = Access {
            api_url: &settings.api_url,
            namespace: &settings.namespace,
            token_file: &settings.token_file,
            ca_file: &settings.ca_file,
        };
        let api = Api::new(access, settings.wait, INIT_HINTS)?;
        let owners: Vec<&str> = services.iter().map(|access| access.service).collect();
        let mut filled =
            passwords::fill(&api, &settings.secret, &owners, requested, report).await?;
        for access in &services {
            let password = filled.remove(access.service).ok_or_else(|| Error::Secret {
                secret: settings.secret.clone(),
                problem: format!("no password for {} after filling it", access.service),
            })?;
            accounts.push((*access, hash_password(&password)?));
        }
    }
    let file = &settings.dynsec_file;
    let existing = match std::fs::read_to_string(file) {
        Ok(text) => Some(text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(Error::MqttBroker(format!(
                "cannot read the dynamic-security file {}: {error}",
                file.display()
            )));
        }
    };
    let fresh = existing.is_none();
    let text = reconcile(existing.as_deref(), &accounts).map_err(|problem| {
        Error::MqttBroker(format!(
            "the dynamic-security file {} cannot be used: {problem}. It is left as it is and the \
             broker does not start: correct it, restore it from a backup, or remove it to start \
             without gateway clients (deploy/chart/README.md, \"MQTT broker\")",
            file.display()
        ))
    })?;
    write_atomically(file, text.as_bytes(), 0o600).map_err(|error| {
        Error::MqttBroker(format!(
            "cannot write the dynamic-security file {}: {error}",
            file.display()
        ))
    })?;
    if fresh {
        report.change(format!(
            "created the dynamic-security file {}",
            file.display()
        ));
    }
    tracing::info!(
        file = %file.display(),
        services = ?accounts.iter().map(|(access, _)| access.service).collect::<Vec<_>>(),
        "the broker's access rules and service accounts are in place"
    );
    Ok(())
}

/// Writes `bytes` to `path` with `mode`: into a new file next to it, flushed to disk, then
/// renamed over it, so a reader never sees a half-written file.
pub(crate) fn write_atomically(path: &Path, bytes: &[u8], mode: u32) -> std::io::Result<()> {
    let name = path
        .file_name()
        .ok_or_else(|| std::io::Error::other("the path names no file"))?;
    let mut temporary_name = std::ffi::OsString::from(".");
    temporary_name.push(name);
    temporary_name.push(".new");
    let temporary = path.with_file_name(temporary_name);
    // Left by an earlier start that stopped halfway.
    match std::fs::remove_file(&temporary) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(&temporary)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&temporary, path)?;
    if let Some(parent) = path.parent() {
        std::fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use protocol::topic::Direction;

    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    #[test]
    fn pbkdf2_matches_known_vectors() {
        // `openssl kdf -keylen 64 -kdfopt digest:SHA512 -kdfopt pass:password -kdfopt salt:salt
        // -kdfopt iter:<n> PBKDF2`.
        let vectors = [
            (
                1,
                "867f70cf1ade02cff3752599a3a53dc4af34c7a669815ae5d513554e1c8cf252c02d470a285a0501bad999bfe943c08f050235d7d68b1da55e63f73b60a57fce",
            ),
            (
                2,
                "e1d9c16aa681708a45f5c7c4e215ceb66e011a2e9f0040713f18aefdb866d53cf76cab2868a39b9f7840edce4fef5a82be67335c77a6068e04112754f27ccf4e",
            ),
            (
                1000,
                "afe6c5530785b6cc6b1c6453384731bd5ee432ee549fd42fb6695779ad8a1c5bf59de69c48f774efc4007d5298f9033c0241d5ab69305e7b64eceeb8d834cfec",
            ),
        ];
        for (iterations, expected) in vectors {
            assert_eq!(
                hex(&pbkdf2_sha512(b"password", b"salt", iterations)),
                expected
            );
        }
    }

    #[test]
    fn encoded_passwords_have_mosquittos_layout() {
        let salt = [7_u8; HASH_BYTES];
        let encoded = encode_password(b"secret", &salt, PASSWORD_ITERATIONS);
        let parts: Vec<&str> = encoded.split('$').collect();
        assert_eq!(parts.len(), 5, "{encoded}");
        assert_eq!((parts[0], parts[1], parts[2]), ("", "7", "1000"));
        assert_eq!(STANDARD.decode(parts[3]).expect("salt").as_slice(), salt);
        assert_eq!(
            STANDARD.decode(parts[4]).expect("hash"),
            pbkdf2_sha512(b"secret", &salt, PASSWORD_ITERATIONS)
        );
        let password = Password::generate().expect("randomness");
        assert_ne!(
            hash_password(&password).expect("randomness"),
            hash_password(&password).expect("randomness"),
            "every hash gets a fresh salt"
        );
    }

    #[test]
    fn every_channel_has_its_cloud_side() {
        for channel in Channel::ALL {
            let receivers = SERVICES
                .iter()
                .filter(|access| access.receives.contains(&channel))
                .count();
            let senders = SERVICES
                .iter()
                .filter(|access| access.sends.contains(&channel))
                .count();
            match channel.direction() {
                Direction::Up => {
                    assert!(receivers >= 1, "nobody receives {}", channel.name());
                    assert_eq!(senders, 0, "a service publishes {}", channel.name());
                }
                Direction::Down => {
                    assert_eq!(senders, 1, "{} needs exactly one sender", channel.name());
                    assert_eq!(receivers, 0, "a service receives {}", channel.name());
                }
            }
        }
        let names: BTreeSet<&str> = SERVICES.iter().map(|access| access.service).collect();
        assert_eq!(names.len(), SERVICES.len(), "service names are unique");
        assert_eq!(
            SERVICES
                .iter()
                .filter(|access| access.dynsec_admin)
                .map(|access| access.service)
                .collect::<Vec<_>>(),
            ["certs"],
            "only the certs service administers dynamic security (Laysi/trustiot-v2#82)"
        );
    }

    #[test]
    fn service_rules_cover_shared_subscriptions_and_nothing_else() {
        let ingest = service("ingest").expect("ingest");
        let rules = service_role_acls(ingest);
        let as_json: Vec<Value> = rules
            .iter()
            .map(|rule| serde_json::to_value(rule).expect("serializable"))
            .collect();
        assert!(as_json.contains(&json!({
            "acltype": "subscribePattern",
            "topic": "$share/+/trustiot/v1/gw/+/up/telemetry",
            "priority": 0,
            "allow": true,
        })));
        assert!(as_json.contains(&json!({
            "acltype": "publishClientReceive",
            "topic": "trustiot/v1/gw/+/up/telemetry",
            "priority": 0,
            "allow": true,
        })));
        assert!(as_json.contains(&json!({
            "acltype": "publishClientSend",
            "topic": "trustiot/v1/gw/+/down/telemetry-ack",
            "priority": 0,
            "allow": true,
        })));
        // Two channels received (five rules each) and one sent.
        assert_eq!(rules.len(), 11);
        assert!(rules.iter().all(|rule| rule.allow));
        assert!(service_role_acls(service("certs").expect("certs")).is_empty());
        assert!(
            service("mqtt-bridge").is_none(),
            "the bridge has no account"
        );
    }

    fn accounts() -> Vec<(&'static ServiceAccess, String)> {
        vec![
            (service("ingest").expect("ingest"), "$7$hash-i".to_owned()),
            (service("certs").expect("certs"), "$7$hash-c".to_owned()),
        ]
    }

    fn parsed(text: &str) -> Value {
        serde_json::from_str(text).expect("JSON")
    }

    fn names<'a>(root: &'a Value, array: &str, key: &str) -> Vec<&'a str> {
        root[array]
            .as_array()
            .expect("array")
            .iter()
            .filter_map(|item| item[key].as_str())
            .collect()
    }

    #[test]
    fn a_fresh_file_denies_by_default_and_holds_the_accounts() {
        let root = parsed(&reconcile(None, &accounts()).expect("reconciled"));
        assert_eq!(root["defaultACLAccess"], default_acl_access());
        assert!(root.get("anonymousGroup").is_none());
        assert_eq!(
            names(&root, "clients", "username"),
            ["service:ingest", "service:certs"]
        );
        let roles = names(&root, "roles", "rolename");
        for role in [
            GATEWAY_ROLE,
            DYNSEC_ADMIN_ROLE,
            "trustiot-service-ingest",
            "trustiot-service-gateway-ops",
            "trustiot-service-certs",
        ] {
            assert!(roles.contains(&role), "{role} missing from {roles:?}");
        }
        let certs = &root["clients"][1];
        assert_eq!(certs["encoded_password"], "$7$hash-c");
        assert_eq!(
            certs["roles"],
            json!([
                { "rolename": "trustiot-service-certs", "priority": 0 },
                { "rolename": DYNSEC_ADMIN_ROLE, "priority": 0 },
            ])
        );
        assert_eq!(root["groups"], json!([]));
        let gateway = root["roles"]
            .as_array()
            .expect("roles")
            .iter()
            .find(|role| role["rolename"] == GATEWAY_ROLE)
            .expect("gateway role");
        assert_eq!(
            gateway["acls"],
            serde_json::to_value(gateway_role_acls()).expect("serializable")
        );
        assert_eq!(
            gateway["allowwildcardsubs"],
            json!(false),
            "a gateway subscribes by exact topic, never with a wildcard"
        );
    }

    #[test]
    fn reconciling_keeps_gateways_and_replaces_what_the_release_manages() {
        let existing = json!({
            "changeIndex": 12,
            "defaultACLAccess": {
                "publishClientSend": true,
                "publishClientReceive": true,
                "subscribe": true,
                "unsubscribe": true,
            },
            "anonymousGroup": "unauthenticated",
            "clients": [
                { "username": "gw-0001", "roles": [{ "rolename": GATEWAY_ROLE }] },
                { "username": "service:ingest", "encoded_password": "$7$old" },
                { "username": "service:retired", "encoded_password": "$7$old" },
            ],
            "groups": [{ "groupname": "unauthenticated", "roles": [] }],
            "roles": [
                { "rolename": GATEWAY_ROLE, "allowwildcardsubs": true, "acls": [
                    { "acltype": "publishClientSend", "topic": "trustiot/v1/gw/%u/up/#", "allow": true }
                ]},
                { "rolename": "trustiot-service-retired", "acls": [] },
                { "rolename": "operator-notes", "acls": [] },
            ],
        })
        .to_string();
        let root = parsed(&reconcile(Some(&existing), &accounts()).expect("reconciled"));
        assert_eq!(root["changeIndex"], 12, "the broker's own fields stay");
        assert_eq!(root["defaultACLAccess"], default_acl_access());
        assert!(root.get("anonymousGroup").is_none());
        assert_eq!(
            names(&root, "clients", "username"),
            ["gw-0001", "service:ingest", "service:certs"]
        );
        assert_eq!(root["clients"][1]["encoded_password"], "$7$hash-i");
        assert_eq!(names(&root, "groups", "groupname"), ["unauthenticated"]);
        let roles = names(&root, "roles", "rolename");
        assert!(roles.contains(&"operator-notes"));
        assert!(!roles.contains(&"trustiot-service-retired"));
        assert_eq!(
            roles.iter().filter(|name| **name == GATEWAY_ROLE).count(),
            1
        );
        let gateway = root["roles"]
            .as_array()
            .expect("roles")
            .iter()
            .find(|role| role["rolename"] == GATEWAY_ROLE)
            .expect("gateway role");
        assert!(
            !gateway["acls"].to_string().contains("up/#"),
            "the old wildcard rule is replaced"
        );
        assert_eq!(
            gateway["allowwildcardsubs"],
            json!(false),
            "a role stored with wildcard subscriptions allowed is replaced"
        );
        // A second start with the same accounts changes nothing.
        let again = reconcile(Some(&root.to_string()), &accounts()).expect("reconciled");
        assert_eq!(parsed(&again), root);
    }

    #[test]
    fn files_the_broker_cannot_use_are_left_alone() {
        for broken in [
            "",
            "[]",
            "{\"clients\": {}}",
            "{\"roles\": 3}",
            "not json",
            // A repeated key: the broker reads the first, serde_json would keep the last.
            "{\"clients\": [{\"username\": \"gw-1\"}], \"clients\": []}",
            "{\"clients\": [{\"username\": \"gw-1\", \"username\": \"gw-2\"}]}",
        ] {
            assert!(
                reconcile(Some(broken), &accounts()).is_err(),
                "{broken:?} was accepted"
            );
        }
        let problem = reconcile(Some("{\"clients\": [], \"clients\": []}"), &accounts())
            .expect_err("a repeated key");
        assert!(
            problem.contains("repeats the key \"clients\"") && problem.contains("the first"),
            "{problem}"
        );
        // Numbers, null and nesting read as serde_json reads them.
        let text =
            "{\"changeIndex\": 18446744073709551615, \"note\": [1.5, -2, null, {\"a\": true}]}";
        let root = parsed(&reconcile(Some(text), &accounts()).expect("accepted"));
        assert_eq!(root["changeIndex"], json!(18_446_744_073_709_551_615_u64));
        assert_eq!(root["note"], json!([1.5, -2, null, { "a": true }]));
    }

    #[test]
    fn entries_the_broker_would_skip_or_misread_are_refused() {
        let gateway = json!({ "username": "gw-1", "roles": [{ "rolename": GATEWAY_ROLE }] });
        let cases = [
            (
                json!({ "roles": ["notes"] }),
                "`roles` entry 0 is not a JSON object",
            ),
            (
                json!({ "roles": [{ "textname": "x" }] }),
                "has no `rolename`",
            ),
            (
                json!({ "roles": [{ "rolename": "" }] }),
                "`rolename` is not a non-empty string",
            ),
            (
                json!({ "roles": [{ "rolename": "a" }, { "rolename": "a" }] }),
                "`roles` entry 1 (`a`) repeats the name `a`",
            ),
            (
                json!({ "roles": [{ "RoleName": GATEWAY_ROLE, "acls": [] }] }),
                "spells `rolename` as `RoleName`",
            ),
            (
                json!({ "roles": [{ "rolename": "a", "acls": {} }] }),
                "`acls` is not an array",
            ),
            (
                json!({ "roles": [{ "rolename": "a", "acls": [{ "acltype": "subscribe", "topic": "x" }] }] }),
                "`acltype` \"subscribe\" is none of",
            ),
            (
                json!({ "roles": [{ "rolename": "a", "acls": [{ "acltype": "publishClientSend", "topic": "x", "allow": "false" }] }] }),
                "`allow` is not true or false",
            ),
            (
                json!({ "roles": [{ "rolename": "a", "acls": [{ "acltype": "publishClientSend" }] }] }),
                "`acls` entry 0 has no `topic`",
            ),
            (
                json!({ "roles": [{ "rolename": "a", "acls": [
                    { "acltype": "subscribePattern", "topic": "x", "allow": false },
                    { "acltype": "SUBSCRIBEPATTERN", "topic": "x", "allow": true },
                ] }] }),
                "repeats the subscribePattern rule for \"x\"",
            ),
            (
                json!({ "clients": [gateway.clone(), { "username": "gw-2", "disabled": "true" }] }),
                "`clients` entry 1 (`gw-2`): `disabled` is not true or false",
            ),
            (
                json!({ "clients": [{ "username": "gw-2", "clientid": 5 }] }),
                "`clientid` is not a string",
            ),
            (
                json!({ "clients": [{ "username": "gw-2", "Username": "service:ingest" }] }),
                "which the broker reads as one field",
            ),
            (
                json!({ "clients": [gateway.clone(), gateway.clone()] }),
                "repeats the name `gw-1`",
            ),
            (
                json!({ "clients": [{ "username": "gw-2", "roles": [{ "rolename": 3 }] }] }),
                "`roles` entry 0: `rolename` is not a non-empty string",
            ),
            (
                json!({ "clients": [{ "username": "gw-2", "roles": {} }] }),
                "`roles` is not an array",
            ),
            (
                json!({ "groups": [{ "groupname": "g", "clients": [{ "priority": 1 }] }] }),
                "`groups` entry 0 (`g`), `clients` entry 0 has no `username`",
            ),
            (
                json!({ "groups": [{ "groupname": "g" }, { "groupname": "g" }] }),
                "repeats the name `g`",
            ),
            (
                json!({ "DefaultACLAccess": { "publishClientSend": true } }),
                "the file spells `defaultACLAccess` as `DefaultACLAccess`",
            ),
            (
                json!({ "AnonymousGroup": "g" }),
                "spells `anonymousGroup` as `AnonymousGroup`",
            ),
            (
                json!({ "changeIndex": "3" }),
                "`changeIndex` is not a number",
            ),
        ];
        for (existing, expected) in cases {
            let problem = reconcile(Some(&existing.to_string()), &accounts())
                .expect_err(&format!("{existing} was accepted"));
            assert!(problem.contains(expected), "{existing}: {problem}");
        }
    }

    #[test]
    fn entries_the_broker_reads_as_written_are_kept() {
        let existing = json!({
            "changeIndex": 3,
            "roles": [
                // Replaced by the release however it looks.
                { "rolename": "trustiot-service-ingest", "acls": "broken" },
                { "rolename": "notes", "allowwildcardsubs": false, "acls": [
                    { "acltype": "SUBSCRIBEPATTERN", "topic": "notes/#", "priority": 1, "allow": true },
                    { "acltype": "publishClientReceive", "topic": "notes/#", "allow": false },
                ]},
            ],
            "clients": [
                { "username": "service:ingest", "roles": "broken" },
                { "username": "gw-1", "clientid": "gw-1", "disabled": true, "note": 5,
                  "roles": [{ "rolename": GATEWAY_ROLE, "priority": -1 }] },
            ],
            "groups": [{ "groupname": "fleet", "roles": [{ "rolename": "notes" }],
                         "clients": [{ "username": "gw-1", "priority": 1 },
                                     { "username": "service:ingest" }] }],
        });
        let root = parsed(&reconcile(Some(&existing.to_string()), &accounts()).expect("accepted"));
        let kept = &root["clients"][0];
        assert_eq!(kept["username"], "gw-1");
        assert_eq!(kept["disabled"], true);
        assert_eq!(kept["note"], 5, "fields the broker does not read stay");
        assert_eq!(
            root["groups"],
            json!([{ "groupname": "fleet", "roles": [{ "rolename": "notes" }],
                     "clients": [{ "username": "gw-1", "priority": 1 }] }]),
            "no group lists a service account"
        );
        assert_eq!(root["roles"][0], existing["roles"][1]);
        assert_eq!(
            names(&root, "clients", "username"),
            ["gw-1", "service:ingest", "service:certs"]
        );
    }

    #[test]
    fn service_lists() {
        let parsed = parse_services(" ingest, gateway-ops ,certs").expect("valid");
        assert_eq!(
            parsed
                .iter()
                .map(|access| access.service)
                .collect::<Vec<_>>(),
            ["ingest", "gateway-ops", "certs"]
        );
        assert!(parse_services("").expect("empty").is_empty());
        assert!(parse_services("ingest,ingest").is_err());
        assert!(parse_services("mqtt-bridge").is_err());
        assert!(parse_services("assistant").is_err());
    }

    #[test]
    fn files_are_replaced_whole_with_their_mode() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = std::env::temp_dir().join(format!(
            "mqtt-accounts-{}-{}",
            std::process::id(),
            hex(&Password::generate()
                .expect("randomness")
                .expose()
                .as_bytes()[..8])
        ));
        std::fs::create_dir(&dir).expect("temporary directory");
        let path = dir.join("dynamic-security.json");
        write_atomically(&path, b"first", 0o600).expect("written");
        write_atomically(&path, b"second", 0o600).expect("replaced");
        assert_eq!(std::fs::read(&path).expect("readable"), b"second");
        let mode = std::fs::metadata(&path)
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        assert_eq!(
            std::fs::read_dir(&dir).expect("listing").count(),
            1,
            "no temporary file is left"
        );
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }
}
