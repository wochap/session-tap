use crate::store::{HubStore, Reject, SnapshotAccept, UpdateAccept};
use anyhow::{Result, bail};
use sessiontap_core::{
    domain::{PublicAgentView, PublicField},
    protocol::SourceEnvelope,
};
use std::{collections::BTreeSet, sync::Arc};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

#[derive(Debug, Clone)]
pub struct AcceptedUpdate {
    pub hub_revision: u64,
    pub source_id: String,
    pub delivery_id: String,
    pub source_revision: u64,
    pub view: PublicAgentView,
    pub changed: BTreeSet<PublicField>,
    pub first_seen: bool,
}

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
    let mut request_line = lines.next().unwrap_or_default().split_whitespace();
    let method = request_line.next().unwrap_or_default().to_owned();
    let path = request_line.next().unwrap_or_default().to_owned();
    let mut content_length = 0;
    let mut bearer = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.trim().eq_ignore_ascii_case("content-length") {
            content_length = value.trim().parse().unwrap_or(0);
        }
        if name.trim().eq_ignore_ascii_case("authorization") {
            bearer = value.trim().strip_prefix("Bearer ").map(str::to_owned);
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

fn expected_token(path: &str) -> Result<String> {
    use std::{fs, os::unix::fs::PermissionsExt};
    let meta = fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() || meta.permissions().mode() & 0o077 != 0 {
        bail!("hub token file must be private and not a symlink");
    }
    Ok(fs::read_to_string(path)?.trim().to_owned())
}
fn token_matches(provided: Option<&str>, token_file: Option<&str>) -> bool {
    let Some(path) = token_file else {
        return true;
    };
    let Ok(expected) = expected_token(path) else {
        return false;
    };
    provided.is_some_and(|provided| {
        let (a, b) = (provided.as_bytes(), expected.as_bytes());
        a.len() == b.len() && a.iter().zip(b).fold(0_u8, |acc, (x, y)| acc | (x ^ y)) == 0
    })
}

pub fn handle_ingest(
    store: &HubStore,
    token_file: Option<&str>,
    request: &IngestedRequest,
) -> IngestOutcome {
    if request.method == "GET" && request.path == "/health" {
        return outcome(
            200,
            serde_json::json!({"status":"ok","revision":store.revision().unwrap_or(0)}),
            None,
        );
    }
    if request.method != "POST" {
        return outcome(405, serde_json::json!({"error":"method_not_allowed"}), None);
    }
    if !token_matches(request.bearer.as_deref(), token_file) {
        return outcome(401, serde_json::json!({"error":"unauthorized"}), None);
    }
    let envelope: SourceEnvelope = match serde_json::from_slice(&request.body) {
        Ok(value) => value,
        Err(_) => return outcome(400, serde_json::json!({"error":"malformed_envelope"}), None),
    };
    match &envelope {
        SourceEnvelope::Snapshot { .. } => match store.ingest_snapshot(&envelope) {
            Ok(SnapshotAccept::Applied { hub_revision }) => outcome(
                200,
                serde_json::json!({"status":"applied","hub_revision":hub_revision}),
                Some(HubPublication::SnapshotApplied { hub_revision }),
            ),
            Ok(SnapshotAccept::Stale) => outcome(200, serde_json::json!({"status":"stale"}), None),
            Err(reject) => reject_outcome(reject),
        },
        SourceEnvelope::Update {
            source_id,
            delivery_id,
            revision,
            view,
            ..
        } => match store.ingest_update(&envelope) {
            Ok(UpdateAccept::Applied {
                hub_revision,
                changed,
                first_seen,
            }) => outcome(
                200,
                serde_json::json!({"status":"applied","hub_revision":hub_revision}),
                Some(HubPublication::Update(Box::new(AcceptedUpdate {
                    hub_revision,
                    source_id: source_id.clone(),
                    delivery_id: delivery_id.clone(),
                    source_revision: *revision,
                    view: (**view).clone(),
                    changed,
                    first_seen,
                }))),
            ),
            Ok(UpdateAccept::Duplicate) => {
                outcome(200, serde_json::json!({"status":"duplicate"}), None)
            }
            Ok(UpdateAccept::Stale) => outcome(200, serde_json::json!({"status":"stale"}), None),
            Err(reject) => reject_outcome(reject),
        },
    }
}
fn outcome(
    status: u16,
    body: serde_json::Value,
    publication: Option<HubPublication>,
) -> IngestOutcome {
    IngestOutcome {
        status,
        body,
        publication,
    }
}
fn reject_outcome(reject: Reject) -> IngestOutcome {
    match reject {
        Reject::SnapshotRequired => {
            outcome(409, serde_json::json!({"error":"snapshot_required"}), None)
        }
        Reject::UnsupportedVersion(version) => outcome(
            400,
            serde_json::json!({"error":"unsupported_schema_version","version":version}),
            None,
        ),
        Reject::Malformed(_) => {
            outcome(400, serde_json::json!({"error":"malformed_envelope"}), None)
        }
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
            let rejected = outcome(413, serde_json::json!({"error":"payload_too_large"}), None);
            let _ = write_response(&mut stream, &rejected).await;
            return None;
        }
    };
    let result = handle_ingest(&store, token_file.as_deref(), &request);
    let publication = result.publication.clone();
    let _ = write_response(&mut stream, &result).await;
    publication
}
