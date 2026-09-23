//! Configured event sinks. Each sink kind is built once from `SinkConfig` at
//! startup; the outbox worker only sees the [`Sink`] trait.

mod http;
mod hub;
mod stdout;

pub use http::HttpSink;
pub use hub::HubSink;
pub use stdout::StdoutSink;

use anyhow::{Context, Result};
use async_trait::async_trait;
use sessiontap_core::{
    config::{SinkConfig, validate_sink_url},
    domain::PublicField,
};
use sessiontap_infra::token::read_private_token;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
};

/// Result of one delivery attempt, mapped by the worker onto outbox actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryOutcome {
    /// Delivered; remove from the outbox.
    Ack,
    /// Transient failure; retry with backoff.
    Retry,
    /// The receiver has no baseline for this source; resend a snapshot first.
    SnapshotRequired,
    /// Permanent rejection; subject to the bounded drop policy.
    Reject,
}

#[async_trait]
pub trait Sink: Send + Sync {
    fn name(&self) -> &str;

    /// Sinks that need a source snapshot before incremental updates.
    fn needs_baseline(&self) -> bool {
        false
    }

    async fn deliver(&self, payload: &[u8]) -> DeliveryOutcome;

    /// Delivers a serialized source snapshot envelope.
    async fn deliver_snapshot(&self, _payload: &[u8]) -> DeliveryOutcome {
        DeliveryOutcome::Ack
    }
}

/// Bearer token source shared by HTTP and hub sinks. A token file takes
/// precedence over an environment variable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenSource {
    None,
    Env(String),
    File(PathBuf),
}

impl TokenSource {
    #[must_use]
    pub fn from_config(token_env: Option<&str>, token_file: Option<&str>) -> Self {
        match (token_file, token_env) {
            (Some(path), _) => Self::File(path.into()),
            (None, Some(name)) => Self::Env(name.to_owned()),
            (None, None) => Self::None,
        }
    }

    /// Reads the current token. An unset environment variable yields no
    /// token; a token file must be private and not a symlink.
    pub fn token(&self) -> Result<Option<String>> {
        match self {
            Self::None => Ok(None),
            Self::Env(name) => Ok(std::env::var(name).ok()),
            Self::File(path) => read_private_token(path)
                .map(Some)
                .with_context(|| format!("token file {}", path.display())),
        }
    }
}

/// Builds every enabled sink, validating URLs once and sharing one HTTP
/// client.
pub fn build_sinks(
    configs: &BTreeMap<String, SinkConfig>,
) -> Result<BTreeMap<String, Box<dyn Sink>>> {
    let client = reqwest::Client::new();
    let mut sinks: BTreeMap<String, Box<dyn Sink>> = BTreeMap::new();
    for (name, config) in configs.iter().filter(|(_, config)| config.enabled()) {
        let fields = config
            .public_fields()
            .map_err(|error| anyhow::anyhow!("sink '{name}': {error}"))?;
        let sink: Box<dyn Sink> = match config {
            SinkConfig::Stdout { .. } => Box::new(StdoutSink::new(name.clone(), fields)),
            SinkConfig::Http {
                url,
                token_env,
                token_file,
                ..
            } => {
                validate_sink_url(url, &[])
                    .map_err(|error| anyhow::anyhow!("sink '{name}': {error}"))?;
                Box::new(HttpSink {
                    name: name.clone(),
                    url: url.clone(),
                    auth: TokenSource::from_config(token_env.as_deref(), token_file.as_deref()),
                    timeout: config.timeout(),
                    fields,
                    client: client.clone(),
                })
            }
            SinkConfig::Hub {
                url,
                token_env,
                token_file,
                trusted_addresses,
                ..
            } => {
                validate_sink_url(url, trusted_addresses)
                    .map_err(|error| anyhow::anyhow!("sink '{name}': {error}"))?;
                Box::new(HubSink {
                    name: name.clone(),
                    url: url.clone(),
                    auth: TokenSource::from_config(token_env.as_deref(), token_file.as_deref()),
                    timeout: config.timeout(),
                    client: client.clone(),
                })
            }
        };
        sinks.insert(name.clone(), sink);
    }
    Ok(sinks)
}

/// Restricts an update envelope to the selected public fields plus the
/// invocation identity. An empty selection returns the payload unchanged;
/// `None` means the payload is not a recognizable update envelope.
pub(crate) fn project_fields(payload: &[u8], fields: &BTreeSet<PublicField>) -> Option<Vec<u8>> {
    if fields.is_empty() {
        return Some(payload.to_vec());
    }
    let selected: BTreeSet<String> = fields
        .iter()
        .filter_map(|field| match serde_json::to_value(field) {
            Ok(serde_json::Value::String(name)) => Some(name),
            _ => None,
        })
        .collect();
    let mut envelope: serde_json::Value = serde_json::from_slice(payload).ok()?;
    let object = envelope.as_object_mut()?;
    if object.get("type")?.as_str()? != "update" {
        return None;
    }
    object
        .get_mut("view")?
        .as_object_mut()?
        .retain(|key, _| key == "invocation_id" || selected.contains(key));
    object
        .get_mut("changed")?
        .as_array_mut()?
        .retain(|field| field.as_str().is_some_and(|name| selected.contains(name)));
    serde_json::to_vec(&envelope).ok()
}

/// Posts a JSON body with the sink's timeout and bearer token. Token read
/// failures surface as errors so callers can treat them as transient.
pub(crate) async fn post(
    client: &reqwest::Client,
    url: &str,
    auth: &TokenSource,
    timeout: std::time::Duration,
    body: Vec<u8>,
) -> Result<reqwest::Response> {
    let mut request = client
        .post(url)
        .timeout(timeout)
        .header("content-type", "application/json")
        .body(body);
    if let Some(token) = auth.token()? {
        request = request.bearer_auth(token);
    }
    Ok(request.send().await?)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    /// Serves one canned response per accepted connection and returns the
    /// received request bodies.
    pub(crate) async fn responder(
        responses: Vec<&'static str>,
    ) -> (String, tokio::task::JoinHandle<Vec<Vec<u8>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/ingest", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let mut bodies = Vec::new();
            for response in responses {
                let (mut stream, _) = listener.accept().await.unwrap();
                bodies.push(read_body(&mut stream).await);
                stream.write_all(response.as_bytes()).await.unwrap();
            }
            bodies
        });
        (url, task)
    }

    async fn read_body(stream: &mut tokio::net::TcpStream) -> Vec<u8> {
        let mut bytes = Vec::new();
        let header_end = loop {
            let mut chunk = [0_u8; 4096];
            let count = stream.read(&mut chunk).await.unwrap();
            bytes.extend_from_slice(&chunk[..count]);
            if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                break end + 4;
            }
        };
        let headers = String::from_utf8_lossy(&bytes[..header_end]).to_ascii_lowercase();
        let length = headers
            .lines()
            .find_map(|line| line.strip_prefix("content-length: "))
            .map_or(0, |value| value.trim().parse::<usize>().unwrap());
        while bytes.len() - header_end < length {
            let mut chunk = [0_u8; 4096];
            let count = stream.read(&mut chunk).await.unwrap();
            bytes.extend_from_slice(&chunk[..count]);
        }
        bytes[header_end..header_end + length].to_vec()
    }

    pub(crate) fn update_payload() -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "type": "update",
            "schema_version": 1,
            "source_id": "local",
            "delivery_id": "d1",
            "revision": 3,
            "changed": ["status", "cwd", "usage"],
            "view": {
                "invocation_id": "00000000-0000-4000-8000-000000000001",
                "provider": "claude",
                "status": "running",
                "cwd": "/work",
                "created_at": "2026-01-01T00:00:00Z",
                "updated_at": "2026-01-01T00:00:00Z",
                "usage": {"input_tokens": 5}
            }
        }))
        .unwrap()
    }

    fn config(toml: &str) -> BTreeMap<String, SinkConfig> {
        toml::from_str::<sessiontap_core::config::Config>(toml)
            .unwrap()
            .sinks
    }

    #[test]
    fn non_loopback_plain_http_sink_is_rejected_at_build() {
        let error = build_sinks(&config(
            "version=1\n[sinks.remote]\ntype='http'\nenabled=true\nurl='http://example.com/hook'\n",
        ))
        .err()
        .unwrap();
        assert!(error.to_string().contains("'remote'"), "{error}");
        let sinks = build_sinks(&config(
            "version=1\n[sinks.local]\ntype='http'\nenabled=true\nurl='http://127.0.0.1:9/hook'\n[sinks.off]\ntype='stdout'\n",
        ))
        .unwrap();
        assert_eq!(sinks.keys().collect::<Vec<_>>(), ["local"]);
        assert!(!sinks["local"].needs_baseline());
    }

    #[test]
    fn hub_sink_needs_baseline() {
        let sinks = build_sinks(&config(
            "version=1\nsource_id='h'\n[sinks.hub]\ntype='hub'\nenabled=true\nurl='http://127.0.0.1:9/ingest'\n",
        ))
        .unwrap();
        assert!(sinks["hub"].needs_baseline());
        assert_eq!(sinks["hub"].name(), "hub");
    }

    #[test]
    fn empty_selection_keeps_payload_and_selection_projects_view_and_changed() {
        let payload = update_payload();
        assert_eq!(project_fields(&payload, &BTreeSet::new()).unwrap(), payload);
        let projected: serde_json::Value = serde_json::from_slice(
            &project_fields(
                &payload,
                &BTreeSet::from([PublicField::Status, PublicField::Usage]),
            )
            .unwrap(),
        )
        .unwrap();
        let view = projected["view"].as_object().unwrap();
        assert_eq!(
            view.keys().collect::<BTreeSet<_>>(),
            BTreeSet::from([
                &"invocation_id".to_owned(),
                &"status".to_owned(),
                &"usage".to_owned()
            ])
        );
        assert_eq!(projected["changed"], serde_json::json!(["status", "usage"]));
        assert_eq!(projected["delivery_id"], "d1");
        assert!(
            project_fields(
                b"{\"type\":\"snapshot\"}",
                &BTreeSet::from([PublicField::Status])
            )
            .is_none()
        );
    }

    #[test]
    fn children_is_a_selectable_public_field() {
        let sinks = config(
            "version=1\n[sinks.out]\ntype='stdout'\nenabled=true\nfields=['status','children']\n",
        );
        assert_eq!(
            sinks["out"].public_fields().unwrap(),
            BTreeSet::from([PublicField::Status, PublicField::Children])
        );
        let payload = serde_json::to_vec(&serde_json::json!({
            "type": "update",
            "schema_version": 1,
            "source_id": "local",
            "delivery_id": "d2",
            "revision": 4,
            "changed": ["updated_at", "children"],
            "view": {
                "invocation_id": "00000000-0000-4000-8000-000000000001",
                "provider": "claude",
                "status": "stopped",
                "cwd": "/work",
                "created_at": "2026-01-01T00:00:00Z",
                "updated_at": "2026-01-01T00:00:01Z",
                "children": [{
                    "agent_id": "agent-1",
                    "agent_type": "Explore",
                    "status": "running",
                    "started_at": "2026-01-01T00:00:00Z",
                    "updated_at": "2026-01-01T00:00:01Z"
                }]
            }
        }))
        .unwrap();
        let projected: serde_json::Value = serde_json::from_slice(
            &project_fields(&payload, &BTreeSet::from([PublicField::Children])).unwrap(),
        )
        .unwrap();
        assert_eq!(projected["changed"], serde_json::json!(["children"]));
        assert_eq!(projected["view"]["children"][0]["agent_id"], "agent-1");
        assert!(projected["view"].get("status").is_none());
    }

    #[test]
    fn token_source_prefers_private_file_and_rejects_open_or_linked_files() {
        use std::{
            fs,
            os::unix::fs::{PermissionsExt, symlink},
        };
        let temp = tempfile::tempdir().unwrap();
        let token = temp.path().join("token");
        fs::write(&token, "secret\n").unwrap();
        fs::set_permissions(&token, fs::Permissions::from_mode(0o644)).unwrap();
        let source = TokenSource::from_config(Some("UNUSED"), token.to_str());
        assert_eq!(source, TokenSource::File(token.clone()));
        assert!(source.token().is_err());
        fs::set_permissions(&token, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(source.token().unwrap().as_deref(), Some("secret"));
        let link = temp.path().join("link");
        symlink(&token, &link).unwrap();
        assert!(TokenSource::File(link).token().is_err());
        assert!(
            TokenSource::Env("SESSIONTAP_TEST_UNSET_TOKEN".into())
                .token()
                .unwrap()
                .is_none()
        );
        assert_eq!(fs::read_to_string(token).unwrap(), "secret\n");
    }
}
