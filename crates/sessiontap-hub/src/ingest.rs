use anyhow::{Result, bail};
use sessiontap_core::domain::{ActiveAttention, InvocationSnapshot};
use sessiontap_core::protocol::HubEnvelope;
use sessiontap_core::protocol::HubEventMetadata;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::store::{HubStore, Reject, SnapshotAccept, UpdateAccept};

/// Everything downstream consumers (live stream, routing) need about one
/// durably accepted update.
#[derive(Debug, Clone)]
pub struct AcceptedUpdate {
    pub hub_revision: u64,
    pub source_id: String,
    pub event_id: String,
    pub event: HubEventMetadata,
    pub snapshot: InvocationSnapshot,
    pub attention: Option<ActiveAttention>,
    pub changed: Vec<String>,
    pub first_seen: bool,
}

/// Publication to downstream consumers after a durable ingestion commit.
/// Source snapshots carry no update payload; live consumers re-baseline from
/// the persisted merged view instead.
#[derive(Debug, Clone)]
pub enum HubPublication {
    Update(Box<AcceptedUpdate>),
    SnapshotApplied { hub_revision: u64 },
}

pub struct IngestOutcome {
    pub status: u16,
    pub body: serde_json::Value,
    pub publication: Option<HubPublication>,
}

pub struct IngestedRequest {
    pub method: String,
    pub path: String,
    pub bearer: Option<String>,
    pub body: Vec<u8>,
}

/// Reads one HTTP/1.1 request with bounded headers and body.
pub async fn read_request(
    stream: &mut TcpStream,
    max_body_bytes: usize,
) -> Result<IngestedRequest> {
    let mut bytes = Vec::with_capacity(4096);
    let header_end = loop {
        if bytes.len() > 64 * 1024 {
            bail!("request headers too large");
        }
        if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
            break end + 4;
        }
        let mut chunk = [0_u8; 4096];
        let count = stream.read(&mut chunk).await?;
        if count == 0 {
            bail!("incomplete HTTP request");
        }
        bytes.extend_from_slice(&chunk[..count]);
    };
    let headers = String::from_utf8_lossy(&bytes[..header_end]);
    let mut lines = headers.lines();
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_owned();
    let path = parts.next().unwrap_or_default().to_owned();
    let mut content_length = 0_usize;
    let mut bearer = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim().to_ascii_lowercase();
        let value = value.trim();
        if name == "content-length" {
            content_length = value.parse().unwrap_or(0);
        }
        if name == "authorization" {
            bearer = value.strip_prefix("Bearer ").map(str::to_owned);
        }
    }
    if content_length > max_body_bytes {
        bail!("request body exceeds limit");
    }
    while bytes.len() - header_end < content_length {
        let mut chunk = [0_u8; 4096];
        let count = stream.read(&mut chunk).await?;
        if count == 0 {
            bail!("incomplete HTTP body");
        }
        bytes.extend_from_slice(&chunk[..count]);
    }
    Ok(IngestedRequest {
        method,
        path,
        bearer,
        body: bytes[header_end..header_end + content_length].to_vec(),
    })
}

fn expected_token(token_file: &str) -> Result<String> {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    let meta = fs::symlink_metadata(token_file)?;
    if meta.file_type().is_symlink() || meta.permissions().mode() & 0o077 != 0 {
        bail!("hub token file must be private and not a symlink");
    }
    Ok(fs::read_to_string(token_file)?.trim().to_owned())
}

fn token_matches(provided: Option<&str>, token_file: Option<&str>) -> bool {
    let Some(token_file) = token_file else {
        return true;
    };
    let Ok(expected) = expected_token(token_file) else {
        return false;
    };
    provided.is_some_and(|provided| {
        let provided = provided.as_bytes();
        let expected = expected.as_bytes();
        provided.len() == expected.len()
            && provided
                .iter()
                .zip(expected)
                .fold(0_u8, |acc, (a, b)| acc | (a ^ b))
                == 0
    })
}

/// Validates one ingestion request and applies it to the store. Pure with
/// respect to transport: callers map the outcome onto HTTP responses and
/// downstream publication.
pub fn handle_ingest(
    store: &HubStore,
    token_file: Option<&str>,
    request: &IngestedRequest,
) -> IngestOutcome {
    if request.method == "GET" && request.path == "/health" {
        let revision = store.revision().unwrap_or(0);
        return IngestOutcome {
            status: 200,
            body: serde_json::json!({ "status": "ok", "revision": revision }),
            publication: None,
        };
    }
    if request.method != "POST" {
        return IngestOutcome {
            status: 405,
            body: serde_json::json!({ "error": "method_not_allowed" }),
            publication: None,
        };
    }
    if !token_matches(request.bearer.as_deref(), token_file) {
        return IngestOutcome {
            status: 401,
            body: serde_json::json!({ "error": "unauthorized" }),
            publication: None,
        };
    }
    let envelope: HubEnvelope = match serde_json::from_slice(&request.body) {
        Ok(envelope) => envelope,
        Err(_) => {
            return IngestOutcome {
                status: 400,
                body: serde_json::json!({ "error": "malformed_envelope" }),
                publication: None,
            };
        }
    };
    match envelope {
        HubEnvelope::Snapshot { .. } => match store.ingest_snapshot(&envelope) {
            Ok(SnapshotAccept::Applied { hub_revision }) => IngestOutcome {
                status: 200,
                body: serde_json::json!({ "status": "applied", "hub_revision": hub_revision }),
                publication: Some(HubPublication::SnapshotApplied { hub_revision }),
            },
            Ok(SnapshotAccept::Stale) => IngestOutcome {
                status: 200,
                body: serde_json::json!({ "status": "stale" }),
                publication: None,
            },
            Err(reject) => reject_outcome(reject),
        },
        HubEnvelope::Update {
            ref source_id,
            ref event_id,
            ref event,
            ref snapshot,
            ref attention,
            ..
        } => match store.ingest_update(&envelope) {
            Ok(UpdateAccept::Applied {
                hub_revision,
                changed,
                first_seen,
            }) => IngestOutcome {
                status: 200,
                body: serde_json::json!({ "status": "applied", "hub_revision": hub_revision }),
                publication: Some(HubPublication::Update(Box::new(AcceptedUpdate {
                    hub_revision,
                    source_id: source_id.clone(),
                    event_id: event_id.clone(),
                    event: event.clone(),
                    snapshot: (**snapshot).clone(),
                    attention: attention.clone(),
                    changed,
                    first_seen,
                }))),
            },
            Ok(UpdateAccept::Duplicate) => IngestOutcome {
                status: 200,
                body: serde_json::json!({ "status": "duplicate" }),
                publication: None,
            },
            Ok(UpdateAccept::Stale) => IngestOutcome {
                status: 200,
                body: serde_json::json!({ "status": "stale" }),
                publication: None,
            },
            Err(reject) => reject_outcome(reject),
        },
    }
}

fn reject_outcome(reject: Reject) -> IngestOutcome {
    match reject {
        Reject::SnapshotRequired => IngestOutcome {
            status: 409,
            body: serde_json::json!({ "error": "snapshot_required" }),
            publication: None,
        },
        Reject::UnsupportedVersion(version) => IngestOutcome {
            status: 400,
            body: serde_json::json!({ "error": "unsupported_schema_version", "version": version }),
            publication: None,
        },
        Reject::Malformed(_) => IngestOutcome {
            status: 400,
            body: serde_json::json!({ "error": "malformed_envelope" }),
            publication: None,
        },
    }
}

pub async fn write_response(stream: &mut TcpStream, outcome: &IngestOutcome) -> Result<()> {
    let reason = match outcome.status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        405 => "Method Not Allowed",
        409 => "Conflict",
        413 => "Payload Too Large",
        _ => "Error",
    };
    let body = serde_json::to_vec(&outcome.body)?;
    let head = format!(
        "HTTP/1.1 {} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        outcome.status,
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(&body).await?;
    Ok(())
}

pub async fn serve_connection(
    mut stream: TcpStream,
    store: Arc<HubStore>,
    token_file: Option<String>,
    max_body_bytes: usize,
) -> Option<HubPublication> {
    let request = match read_request(&mut stream, max_body_bytes).await {
        Ok(request) => request,
        Err(_) => {
            let outcome = IngestOutcome {
                status: 413,
                body: serde_json::json!({ "error": "payload_too_large" }),
                publication: None,
            };
            let _ = write_response(&mut stream, &outcome).await;
            return None;
        }
    };
    let outcome = handle_ingest(&store, token_file.as_deref(), &request);
    let publication = outcome.publication.clone();
    let _ = write_response(&mut stream, &outcome).await;
    publication
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use sessiontap_core::{
        domain::{
            Activity, Capabilities, EventKind, InvocationId, Lifecycle, ProcessMetadata,
            PublicStatus,
        },
        protocol::{HUB_SCHEMA_VERSION, HubEventMetadata, SourceCapabilities, SourceIdentity},
    };

    fn snapshot_fixture() -> InvocationSnapshot {
        let now = Utc::now();
        InvocationSnapshot {
            schema_version: 1,
            revision: 1,
            invocation_id: InvocationId::new(),
            provider: "claude".into(),
            executable: "claude".into(),
            args: vec![],
            cwd: "/tmp".into(),
            process: ProcessMetadata::default(),
            created_at: now,
            updated_at: now,
            lifecycle: Lifecycle::Alive,
            activity: Activity::Idle,
            status: PublicStatus::Idle,
            provider_session: None,
            provider_metadata: None,
            usage: None,
            repository: None,
            multiplexer: None,
            capabilities: Capabilities::default(),
            turn_generation: 0,
            completed_generation: None,
        }
    }

    fn snapshot_request(source: &str, revision: u64) -> IngestedRequest {
        let envelope = HubEnvelope::Snapshot {
            schema_version: HUB_SCHEMA_VERSION,
            source: SourceIdentity {
                id: source.into(),
                display_name: None,
                capabilities: SourceCapabilities::default(),
            },
            revision,
            invocations: vec![snapshot_fixture()],
            active_attention: Default::default(),
        };
        IngestedRequest {
            method: "POST".into(),
            path: "/ingest".into(),
            bearer: None,
            body: serde_json::to_vec(&envelope).unwrap(),
        }
    }

    fn update_request(source: &str, event_id: &str, revision: u64) -> IngestedRequest {
        let now = Utc::now();
        let envelope = HubEnvelope::Update {
            schema_version: HUB_SCHEMA_VERSION,
            source_id: source.into(),
            event_id: event_id.into(),
            revision,
            event: HubEventMetadata {
                kind: EventKind::Working,
                observed_at: now,
                received_at: now,
                failure: None,
                turn_id: None,
            },
            snapshot: Box::new(snapshot_fixture()),
            attention: None,
        };
        IngestedRequest {
            method: "POST".into(),
            path: "/ingest".into(),
            bearer: None,
            body: serde_json::to_vec(&envelope).unwrap(),
        }
    }

    #[test]
    fn malformed_and_unversioned_envelopes_are_rejected_without_state() {
        let store = HubStore::memory().unwrap();
        let outcome = handle_ingest(
            &store,
            None,
            &IngestedRequest {
                method: "POST".into(),
                path: "/ingest".into(),
                bearer: None,
                body: b"{not json".to_vec(),
            },
        );
        assert_eq!(outcome.status, 400);
        assert!(outcome.publication.is_none());
        assert_eq!(store.revision().unwrap(), 0);
    }

    #[test]
    fn update_for_unknown_source_returns_snapshot_required() {
        let store = HubStore::memory().unwrap();
        let outcome = handle_ingest(&store, None, &update_request("host", "e1", 1));
        assert_eq!(outcome.status, 409);
        assert_eq!(outcome.body["error"], "snapshot_required");
    }

    #[test]
    fn snapshot_then_update_is_applied_and_duplicates_ack() {
        let store = HubStore::memory().unwrap();
        let applied = handle_ingest(&store, None, &snapshot_request("host", 1));
        assert_eq!(applied.status, 200);
        assert_eq!(applied.body["status"], "applied");
        let first = handle_ingest(&store, None, &update_request("host", "e1", 2));
        assert_eq!(first.status, 200);
        assert!(first.publication.is_some());
        let retry = handle_ingest(&store, None, &update_request("host", "e1", 2));
        assert_eq!(retry.status, 200);
        assert_eq!(retry.body["status"], "duplicate");
        assert!(retry.publication.is_none());
        // stale revisions are acknowledged and never reach consumers
        let stale = handle_ingest(&store, None, &update_request("host", "stale", 1));
        assert_eq!(stale.status, 200);
        assert_eq!(stale.body["status"], "stale");
        assert!(stale.publication.is_none());
        // rejected deliveries never reach consumers either
        let unknown = handle_ingest(&store, None, &update_request("other", "x", 1));
        assert_eq!(unknown.status, 409);
        assert!(unknown.publication.is_none());
    }

    #[test]
    fn bearer_token_is_enforced_when_configured() {
        let temp = tempfile::tempdir().unwrap();
        let token_path = temp.path().join("token");
        std::fs::write(&token_path, "secret-token\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&token_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let store = HubStore::memory().unwrap();
        let token = token_path.to_str().unwrap();

        let mut request = snapshot_request("host", 1);
        let outcome = handle_ingest(&store, Some(token), &request);
        assert_eq!(outcome.status, 401);

        request.bearer = Some("wrong".into());
        assert_eq!(handle_ingest(&store, Some(token), &request).status, 401);

        request.bearer = Some("secret-token".into());
        assert_eq!(handle_ingest(&store, Some(token), &request).status, 200);
    }

    #[test]
    fn world_readable_token_file_denies_access() {
        let temp = tempfile::tempdir().unwrap();
        let token_path = temp.path().join("token");
        std::fs::write(&token_path, "secret-token\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&token_path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let store = HubStore::memory().unwrap();
        let mut request = snapshot_request("host", 1);
        request.bearer = Some("secret-token".into());
        assert_eq!(
            handle_ingest(&store, token_path.to_str(), &request).status,
            401
        );
    }

    #[test]
    fn unsupported_schema_version_is_reported() {
        let store = HubStore::memory().unwrap();
        let mut request = snapshot_request("host", 1);
        let mut value: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
        value["schema_version"] = serde_json::json!(99);
        request.body = serde_json::to_vec(&value).unwrap();
        let outcome = handle_ingest(&store, None, &request);
        assert_eq!(outcome.status, 400);
        assert_eq!(outcome.body["error"], "unsupported_schema_version");
    }

    #[test]
    fn health_endpoint_reports_revision_without_auth() {
        let temp = tempfile::tempdir().unwrap();
        let token_path = temp.path().join("token");
        std::fs::write(&token_path, "secret-token\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&token_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let store = HubStore::memory().unwrap();
        let outcome = handle_ingest(
            &store,
            token_path.to_str(),
            &IngestedRequest {
                method: "GET".into(),
                path: "/health".into(),
                bearer: None,
                body: vec![],
            },
        );
        assert_eq!(outcome.status, 200);
        assert_eq!(outcome.body["status"], "ok");
    }
}
