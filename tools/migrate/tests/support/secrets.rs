//! A fake Kubernetes API that keeps Secrets of one namespace in memory and answers the two
//! requests the migration Job makes: `GET` and JSON merge `PATCH` of one Secret, with
//! `resourceVersion` preconditions (HTTP 409) as the API server applies them. Plain HTTP on
//! 127.0.0.1, which the Job's settings accept for tests only. It records every request line,
//! content type and whether the token matched, never a body.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use migrate::config::PasswordSettings;
use serde_json::{Map, Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use super::{random_hex, scratch_dir};

pub const NAMESPACE: &str = "release-ns";

/// One Secret as the fake keeps it.
#[derive(Clone, Default)]
pub struct Stored {
    pub data: BTreeMap<String, Vec<u8>>,
    pub annotations: BTreeMap<String, String>,
    pub resource_version: u64,
}

/// One request the fake saw.
#[derive(Clone, Debug)]
pub struct Seen {
    /// For example `PATCH /api/v1/namespaces/release-ns/secrets/a?fieldManager=trustiot-migrate`.
    pub line: String,
    pub content_type: String,
    pub authorized: bool,
}

#[derive(Default)]
struct State {
    secrets: BTreeMap<String, Stored>,
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
                ..Stored::default()
            },
        );
    }

    /// Deletes the Secret, as `kubectl delete secret` would.
    pub fn delete(&self, name: &str) {
        self.state().secrets.remove(name);
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
        let mut parts = line.split(' ');
        let (method, target) = (
            parts.next().unwrap_or_default(),
            parts.next().unwrap_or_default(),
        );
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        let prefix = format!("/api/v1/namespaces/{NAMESPACE}/secrets/");
        let Some(name) = path
            .strip_prefix(&prefix)
            .filter(|name| !name.contains('/'))
        else {
            return status(404, "NotFound");
        };
        match method {
            "GET" if query.is_empty() => match state.secrets.get(name) {
                Some(secret) => (200, render(name, secret)),
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
                    if let Some(secret) = state.secrets.get_mut(name) {
                        secret.resource_version = version;
                    }
                }
                let Ok(patch) = serde_json::from_slice::<Value>(body) else {
                    return status(400, "BadRequest");
                };
                let version = state.bump();
                let rewrite_to = state.rewrite_to.clone();
                let Some(secret) = state.secrets.get_mut(name) else {
                    return status(404, "NotFound");
                };
                match apply(secret, &patch) {
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
                        (200, render(name, secret))
                    }
                    Err(code) => status(code, "Refused"),
                }
            }
            _ => status(405, "MethodNotAllowed"),
        }
    }
}

/// Applies a merge patch of `metadata.resourceVersion`, `metadata.annotations` and `data`; any
/// other field is refused (400), and a stale `resourceVersion` is a conflict (409).
fn apply(secret: &mut Stored, patch: &Value) -> Result<(), u16> {
    let patch = patch.as_object().ok_or(400_u16)?;
    if patch.keys().any(|key| key != "metadata" && key != "data") {
        return Err(400);
    }
    let empty = Map::new();
    let metadata = match patch.get("metadata") {
        Some(value) => value.as_object().ok_or(400_u16)?,
        None => &empty,
    };
    if metadata
        .keys()
        .any(|key| key != "resourceVersion" && key != "annotations")
    {
        return Err(400);
    }
    if let Some(version) = metadata.get("resourceVersion")
        && version.as_str() != Some(secret.resource_version.to_string().as_str())
    {
        return Err(409);
    }
    if let Some(annotations) = metadata.get("annotations") {
        for (key, value) in annotations.as_object().ok_or(400_u16)? {
            match value {
                Value::Null => {
                    secret.annotations.remove(key);
                }
                Value::String(text) => {
                    secret.annotations.insert(key.clone(), text.clone());
                }
                _ => return Err(400),
            }
        }
    }
    if let Some(data) = patch.get("data") {
        for (key, value) in data.as_object().ok_or(400_u16)? {
            match value {
                Value::Null => {
                    secret.data.remove(key);
                }
                Value::String(text) => {
                    let bytes = STANDARD.decode(text).map_err(|_| 400_u16)?;
                    secret.data.insert(key.clone(), bytes);
                }
                _ => return Err(400),
            }
        }
    }
    Ok(())
}

fn render(name: &str, secret: &Stored) -> String {
    let data: Map<String, Value> = secret
        .data
        .iter()
        .map(|(key, value)| (key.clone(), Value::String(STANDARD.encode(value))))
        .collect();
    json!({
        "kind": "Secret",
        "apiVersion": "v1",
        "metadata": {
            "name": name,
            "namespace": NAMESPACE,
            "resourceVersion": secret.resource_version.to_string(),
            "annotations": secret.annotations,
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
