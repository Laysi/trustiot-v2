//! A fake Kubernetes API that keeps Secrets and ConfigMaps of one namespace in memory and answers
//! the two requests tools/migrate makes: `GET` and JSON merge `PATCH` of one object, with
//! `resourceVersion` preconditions (HTTP 409) as the API server applies them. A Secret's `data`
//! travels in base64, a ConfigMap's as text. Plain HTTP on 127.0.0.1, which the settings accept
//! for tests only. It records every request line, content type and whether the token matched,
//! never a body.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use migrate::config::{AdminSettings, KeySettings, PasswordSettings};
use serde_json::{Map, Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use super::{random_hex, scratch_dir};

pub const NAMESPACE: &str = "release-ns";

/// One Secret or ConfigMap as the fake keeps it.
#[derive(Clone, Default)]
pub struct Stored {
    pub data: BTreeMap<String, Vec<u8>>,
    pub annotations: BTreeMap<String, String>,
    pub labels: BTreeMap<String, String>,
    pub owner_references: Vec<Value>,
    pub finalizers: Vec<String>,
    /// `metadata.deletionTimestamp`, set while the Secret is being deleted.
    pub deletion_timestamp: Option<String>,
    pub resource_version: u64,
    /// `metadata.uid`: new for every object the fake creates.
    pub uid: String,
}

/// One request the fake saw.
#[derive(Clone, Debug)]
pub struct Seen {
    /// For example `PATCH /api/v1/namespaces/release-ns/secrets/a?fieldManager=trustiot-migrate`.
    pub line: String,
    pub content_type: String,
    pub authorized: bool,
}

/// Another writer acting just before a request is answered.
type Act = Box<dyn FnOnce(&mut State) + Send>;

#[derive(Default)]
struct State {
    secrets: BTreeMap<String, Stored>,
    config_maps: BTreeMap<String, Stored>,
    /// Called with the state before the next request whose line starts with the text is
    /// answered, once.
    before: Option<(String, Act)>,
    seen: Vec<Seen>,
    next_version: u64,
    /// Before each of the next patches, another writer changes the Secret.
    interfering_writes: usize,
    /// Every request is answered with this status (and nothing is changed).
    refuse_with: Option<u16>,
    /// Every patched value is stored as this instead, as an admission webhook that rewrites
    /// Secrets would.
    rewrite_to: Option<Vec<u8>>,
}

impl State {
    fn bump(&mut self) -> u64 {
        self.next_version += 1;
        self.next_version
    }

    fn objects(&mut self, kind: Kind) -> &mut BTreeMap<String, Stored> {
        match kind {
            Kind::Secret => &mut self.secrets,
            Kind::ConfigMap => &mut self.config_maps,
        }
    }
}

/// The two kinds of object the fake keeps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Secret,
    ConfigMap,
}

/// The fake API; it stops with the test's runtime.
#[derive(Clone)]
pub struct FakeSecrets {
    pub url: String,
    token: String,
    state: Arc<Mutex<State>>,
}

impl FakeSecrets {
    pub async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let url = format!(
            "http://127.0.0.1:{}",
            listener.local_addr().expect("address").port()
        );
        let fake = Self {
            url,
            token: random_hex(),
            state: Arc::default(),
        };
        let serving = fake.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let fake = serving.clone();
                tokio::spawn(async move {
                    let Some((head, body)) = read_request(&mut stream).await else {
                        return;
                    };
                    let (status, body) = fake.answer(&head, &body);
                    let response = format!(
                        "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        fake
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().expect("the fake's state")
    }

    /// Declares an empty Secret, as the chart renders it.
    pub fn declare(&self, name: &str) {
        let mut state = self.state();
        let version = state.bump();
        state.secrets.insert(
            name.to_owned(),
            Stored {
                resource_version: version,
                uid: format!("00000000-0000-4000-8000-{version:012}"),
                ..Stored::default()
            },
        );
    }

    /// Deletes the Secret and creates another object under its name with the same data and
    /// annotations, as someone who copied it (or restored it from a backup) would: only the
    /// uid differs.
    pub fn recreate(&self, name: &str) {
        let mut state = self.state();
        let version = state.bump();
        let secret = state.secrets.get_mut(name).expect("a declared Secret");
        secret.uid = format!("00000000-0000-4000-8000-{version:012}");
        secret.resource_version = version;
    }

    /// Another writer changes the Secret's metadata or data.
    pub fn edit(&self, name: &str, change: impl FnOnce(&mut Stored)) {
        let mut state = self.state();
        let version = state.bump();
        let secret = state.secrets.get_mut(name).expect("a declared Secret");
        change(secret);
        secret.resource_version = version;
    }

    /// Another writer sets an annotation.
    pub fn annotate(&self, name: &str, key: &str, value: &str) {
        let mut state = self.state();
        let version = state.bump();
        let secret = state.secrets.get_mut(name).expect("a declared Secret");
        secret.annotations.insert(key.to_owned(), value.to_owned());
        secret.resource_version = version;
    }

    /// Deletes the Secret, as `kubectl delete secret` would.
    pub fn delete(&self, name: &str) {
        self.state().secrets.remove(name);
    }

    /// Declares an empty ConfigMap, as the chart renders it.
    pub fn declare_config_map(&self, name: &str) {
        let mut state = self.state();
        let version = state.bump();
        state.config_maps.insert(
            name.to_owned(),
            Stored {
                resource_version: version,
                uid: format!("00000000-0000-4000-8000-{version:012}"),
                ..Stored::default()
            },
        );
    }

    /// The ConfigMap, if it exists.
    pub fn config_map(&self, name: &str) -> Option<Stored> {
        self.state().config_maps.get(name).cloned()
    }

    /// A ConfigMap's value as text.
    pub fn config_map_value(&self, name: &str, key: &str) -> Option<String> {
        self.config_map(name)?
            .data
            .get(key)
            .map(|bytes| String::from_utf8(bytes.clone()).expect("UTF-8"))
    }

    /// Another writer changes the ConfigMap.
    pub fn edit_config_map(&self, name: &str, change: impl FnOnce(&mut Stored)) {
        let mut state = self.state();
        let version = state.bump();
        let config_map = state
            .config_maps
            .get_mut(name)
            .expect("a declared ConfigMap");
        change(config_map);
        config_map.resource_version = version;
    }

    /// Deletes the ConfigMap.
    pub fn delete_config_map(&self, name: &str) {
        self.state().config_maps.remove(name);
    }

    /// Before the next request whose line starts with `line` (for example `GET
    /// /api/v1/namespaces/release-ns/configmaps/`) is answered, `act` changes the Secrets and the
    /// ConfigMaps, as another writer acting at that moment would; every object then gets a new
    /// `resourceVersion`. Runs once.
    pub fn before_next(
        &self,
        line: &str,
        act: impl FnOnce(&mut BTreeMap<String, Stored>, &mut BTreeMap<String, Stored>) + Send + 'static,
    ) {
        let act: Act = Box::new(move |state: &mut State| {
            act(&mut state.secrets, &mut state.config_maps);
            let version = state.bump();
            for stored in state
                .secrets
                .values_mut()
                .chain(state.config_maps.values_mut())
            {
                stored.resource_version = version;
            }
        });
        self.state().before = Some((line.to_owned(), act));
    }

    /// Another writer sets (`Some`) or removes (`None`) a key.
    pub fn write(&self, name: &str, key: &str, value: Option<&[u8]>) {
        let mut state = self.state();
        let version = state.bump();
        let secret = state.secrets.get_mut(name).expect("a declared Secret");
        match value {
            Some(value) => secret.data.insert(key.to_owned(), value.to_vec()),
            None => secret.data.remove(key),
        };
        secret.resource_version = version;
    }

    /// The Secret, if it exists.
    pub fn get(&self, name: &str) -> Option<Stored> {
        self.state().secrets.get(name).cloned()
    }

    /// The value of a key as text.
    pub fn value(&self, name: &str, key: &str) -> Option<String> {
        self.get(name)?
            .data
            .get(key)
            .map(|bytes| String::from_utf8(bytes.clone()).expect("UTF-8"))
    }

    /// Every request so far.
    pub fn seen(&self) -> Vec<Seen> {
        self.state().seen.clone()
    }

    /// Before each of the next `count` patches, another writer changes the Secret first.
    pub fn interfere(&self, count: usize) {
        self.state().interfering_writes = count;
    }

    /// Stores every value a patch writes as `value` instead, until called with `None`.
    pub fn rewrite_patched_values(&self, value: Option<&[u8]>) {
        self.state().rewrite_to = value.map(<[u8]>::to_vec);
    }

    /// Answers every request with `status` until called with `None`.
    pub fn refuse_with(&self, status: Option<u16>) {
        self.state().refuse_with = status;
    }

    /// Run settings for this fake: `postgres` and `collector` as `owner=secret` pairs.
    pub fn settings(
        &self,
        generation: &str,
        postgres: &[(&str, &str)],
        collector: Option<(&str, &str)>,
    ) -> PasswordSettings {
        let dir = scratch_dir("kube-token");
        let token_file = dir.join("token");
        std::fs::write(&token_file, format!("{}\n", self.token)).expect("write the token");
        let pairs = |pairs: &[(&str, &str)]| {
            pairs
                .iter()
                .map(|(owner, secret)| format!("{owner}={secret}"))
                .collect::<Vec<_>>()
                .join(",")
        };
        PasswordSettings {
            api_url: self.url.clone(),
            namespace: NAMESPACE.to_owned(),
            token_file,
            ca_file: dir.join("unused-ca.crt"),
            generation: generation.to_owned(),
            postgres: Some(pairs(postgres)),
            clickhouse_collector: collector.map(|pair| pairs(&[pair])),
            clickhouse_telemetry_query: None,
        }
    }

    /// Settings of the identity service's init container for this fake: it keeps the key set in
    /// `secret` and the public keys in `config_map`, for `generation`.
    pub fn key_settings(&self, secret: &str, config_map: &str, generation: &str) -> KeySettings {
        let dir = scratch_dir("kube-token");
        let token_file = dir.join("token");
        std::fs::write(&token_file, format!("{}\n", self.token)).expect("write the token");
        KeySettings {
            api_url: self.url.clone(),
            namespace: NAMESPACE.to_owned(),
            token_file,
            ca_file: dir.join("unused-ca.crt"),
            secret: secret.to_owned(),
            config_map: config_map.to_owned(),
            generation: generation.to_owned(),
            revoke_before_generation: "0".to_owned(),
            propagation_seconds: 600,
            max_key_age_seconds: 0,
            legacy_key_file: None,
            legacy_public_keys: None,
            wait: Duration::from_secs(120),
        }
    }

    /// Settings of the bundled ClickHouse's init container for this fake: it fills `account` in
    /// `secret` for `generation` and writes the password to `password_file`.
    pub fn admin_settings(
        &self,
        secret: &str,
        account: &str,
        generation: &str,
        password_file: &Path,
    ) -> AdminSettings {
        let dir = scratch_dir("kube-token");
        let token_file = dir.join("token");
        std::fs::write(&token_file, format!("{}\n", self.token)).expect("write the token");
        AdminSettings {
            api_url: self.url.clone(),
            namespace: NAMESPACE.to_owned(),
            token_file,
            ca_file: dir.join("unused-ca.crt"),
            secret: secret.to_owned(),
            account: account.to_owned(),
            generation: generation.to_owned(),
            password_file: password_file.to_path_buf(),
            wait: Duration::from_secs(120),
        }
    }

    fn answer(&self, head: &str, body: &[u8]) -> (u16, String) {
        let mut lines = head.lines();
        let line = lines.next().unwrap_or_default().to_owned();
        let header = |wanted: &str| {
            head.lines()
                .skip(1)
                .find_map(|header| {
                    header
                        .split_once(':')
                        .filter(|(name, _)| name.trim().eq_ignore_ascii_case(wanted))
                        .map(|(_, value)| value.trim().to_owned())
                })
                .unwrap_or_default()
        };
        let authorized = header("authorization") == format!("Bearer {}", self.token);
        let content_type = header("content-type");
        let mut state = self.state();
        state.seen.push(Seen {
            line: line.clone(),
            content_type: content_type.clone(),
            authorized,
        });
        if !authorized {
            return status(401, "Unauthorized");
        }
        if let Some(code) = state.refuse_with {
            return status(code, "Refused");
        }
        if state
            .before
            .as_ref()
            .is_some_and(|(prefix, _)| line.starts_with(prefix.as_str()))
            && let Some((_, act)) = state.before.take()
        {
            act(&mut state);
        }
        let mut parts = line.split(' ');
        let (method, target) = (
            parts.next().unwrap_or_default(),
            parts.next().unwrap_or_default(),
        );
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        let route = |kind: Kind, collection: &str| {
            path.strip_prefix(&format!("/api/v1/namespaces/{NAMESPACE}/{collection}/"))
                .filter(|name| !name.contains('/'))
                .map(|name| (kind, name))
        };
        let Some((kind, name)) =
            route(Kind::Secret, "secrets").or_else(|| route(Kind::ConfigMap, "configmaps"))
        else {
            return status(404, "NotFound");
        };
        match method {
            "GET" if query.is_empty() => match state.objects(kind).get(name) {
                Some(secret) => (200, render(kind, name, secret)),
                None => status(404, "NotFound"),
            },
            "PATCH" => {
                if content_type != "application/merge-patch+json" {
                    return status(415, "UnsupportedMediaType");
                }
                if query != "fieldManager=trustiot-migrate" {
                    return status(400, "BadRequest");
                }
                if state.interfering_writes > 0 {
                    state.interfering_writes -= 1;
                    let version = state.bump();
                    if let Some(secret) = state.objects(kind).get_mut(name) {
                        secret.resource_version = version;
                    }
                }
                let Ok(patch) = serde_json::from_slice::<Value>(body) else {
                    return status(400, "BadRequest");
                };
                let version = state.bump();
                let rewrite_to = state.rewrite_to.clone();
                let Some(secret) = state.objects(kind).get_mut(name) else {
                    return status(404, "NotFound");
                };
                match apply(kind, secret, &patch) {
                    Ok(()) => {
                        if let Some(value) = rewrite_to {
                            for key in patch
                                .get("data")
                                .and_then(Value::as_object)
                                .into_iter()
                                .flat_map(Map::keys)
                            {
                                secret.data.insert(key.clone(), value.clone());
                            }
                        }
                        secret.resource_version = version;
                        (200, render(kind, name, secret))
                    }
                    Err(code) => status(code, "Refused"),
                }
            }
            _ => status(405, "MethodNotAllowed"),
        }
    }
}

/// Applies a merge patch of `metadata.resourceVersion`, `metadata.annotations`,
/// `metadata.labels`, `metadata.ownerReferences`, `metadata.finalizers` (each only set to null)
/// and `data`; any other field is refused (400), and a stale `resourceVersion` is a conflict
/// (409).
fn apply(kind: Kind, secret: &mut Stored, patch: &Value) -> Result<(), u16> {
    let patch = patch.as_object().ok_or(400_u16)?;
    if patch.keys().any(|key| key != "metadata" && key != "data") {
        return Err(400);
    }
    let empty = Map::new();
    let metadata = match patch.get("metadata") {
        Some(value) => value.as_object().ok_or(400_u16)?,
        None => &empty,
    };
    if metadata.keys().any(|key| {
        ![
            "resourceVersion",
            "annotations",
            "labels",
            "ownerReferences",
            "finalizers",
        ]
        .contains(&key.as_str())
    }) {
        return Err(400);
    }
    if let Some(version) = metadata.get("resourceVersion")
        && version.as_str() != Some(secret.resource_version.to_string().as_str())
    {
        return Err(409);
    }
    for (field, map) in [
        ("annotations", &mut secret.annotations),
        ("labels", &mut secret.labels),
    ] {
        let Some(patch) = metadata.get(field) else {
            continue;
        };
        for (key, value) in patch.as_object().ok_or(400_u16)? {
            match value {
                Value::Null => {
                    map.remove(key);
                }
                Value::String(text) => {
                    map.insert(key.clone(), text.clone());
                }
                _ => return Err(400),
            }
        }
    }
    match metadata.get("ownerReferences") {
        None => {}
        Some(Value::Null) => secret.owner_references.clear(),
        Some(_) => return Err(400),
    }
    match metadata.get("finalizers") {
        None => {}
        Some(Value::Null) => secret.finalizers.clear(),
        Some(_) => return Err(400),
    }
    if let Some(data) = patch.get("data") {
        for (key, value) in data.as_object().ok_or(400_u16)? {
            match value {
                Value::Null => {
                    secret.data.remove(key);
                }
                Value::String(text) => {
                    let bytes = match kind {
                        Kind::Secret => STANDARD.decode(text).map_err(|_| 400_u16)?,
                        Kind::ConfigMap => text.clone().into_bytes(),
                    };
                    secret.data.insert(key.clone(), bytes);
                }
                _ => return Err(400),
            }
        }
    }
    Ok(())
}

fn render(kind: Kind, name: &str, secret: &Stored) -> String {
    let data: Map<String, Value> = secret
        .data
        .iter()
        .map(|(key, value)| {
            let value = match kind {
                Kind::Secret => STANDARD.encode(value),
                Kind::ConfigMap => String::from_utf8(value.clone()).expect("UTF-8"),
            };
            (key.clone(), Value::String(value))
        })
        .collect();
    if kind == Kind::ConfigMap {
        return json!({
            "kind": "ConfigMap",
            "apiVersion": "v1",
            "metadata": {
                "name": name,
                "namespace": NAMESPACE,
                "resourceVersion": secret.resource_version.to_string(),
                "uid": secret.uid,
                "annotations": secret.annotations,
                "labels": secret.labels,
                "deletionTimestamp": secret.deletion_timestamp,
            },
            "data": data,
        })
        .to_string();
    }
    json!({
        "kind": "Secret",
        "apiVersion": "v1",
        "metadata": {
            "name": name,
            "namespace": NAMESPACE,
            "resourceVersion": secret.resource_version.to_string(),
            "uid": secret.uid,
            "annotations": secret.annotations,
            "labels": secret.labels,
            "ownerReferences": secret.owner_references,
            "finalizers": secret.finalizers,
            "deletionTimestamp": secret.deletion_timestamp,
        },
        "type": "Opaque",
        "data": data,
    })
    .to_string()
}

fn status(code: u16, reason: &str) -> (u16, String) {
    (
        code,
        json!({"kind": "Status", "status": "Failure", "reason": reason, "code": code}).to_string(),
    )
}

/// The head (request line and headers) and the body of one request.
async fn read_request(stream: &mut tokio::net::TcpStream) -> Option<(String, Vec<u8>)> {
    let mut request = Vec::new();
    let mut buffer = [0_u8; 4096];
    let head_end = loop {
        if let Some(at) = request.windows(4).position(|window| window == b"\r\n\r\n") {
            break at;
        }
        match stream.read(&mut buffer).await {
            Ok(0) | Err(_) => return None,
            Ok(read) => request.extend_from_slice(&buffer[..read]),
        }
    };
    let head = String::from_utf8_lossy(&request[..head_end]).into_owned();
    let length: usize = head
        .lines()
        .find_map(|header| {
            header
                .split_once(':')
                .filter(|(name, _)| name.trim().eq_ignore_ascii_case("content-length"))
                .and_then(|(_, value)| value.trim().parse().ok())
        })
        .unwrap_or(0);
    let mut body = request[head_end + 4..].to_vec();
    while body.len() < length {
        match stream.read(&mut buffer).await {
            Ok(0) | Err(_) => return None,
            Ok(read) => body.extend_from_slice(&buffer[..read]),
        }
    }
    Some((head, body))
}
