use crate::store::{HubStore, Reject, SnapshotAccept, UpdateAccept};
use anyhow::Result;
use sessiontap_core::{
    domain::{PublicAgentView, PublicField},
    protocol::SourceEnvelope,
};
use sessiontap_infra::{
    http::{HttpLimits, HttpReadError, read_http_request},
    token::{constant_time_eq, read_private_token},
};
use std::{collections::BTreeSet, path::Path, sync::Arc};
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

/// Hub ingestion header limit; oversized headers are answered with 431.
pub const MAX_HEADER_BYTES: usize = 64 * 1024;

/// Reads one ingestion request, extracting the bearer token.
pub async fn read_request(
    stream: &mut TcpStream,
    max_body_bytes: usize,
) -> Result<IngestedRequest, HttpReadError> {
    let request = read_http_request(
        stream,
        HttpLimits {
            max_header_bytes: MAX_HEADER_BYTES,
            max_body_bytes,
        },
    )
    .await?;
    let bearer = request
        .header("authorization")
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::to_owned);
    Ok(IngestedRequest {
        method: request.method,
        path: request.path,
        bearer,
        body: request.body,
    })
}

fn token_matches(provided: Option<&str>, token_file: Option<&str>) -> bool {
    let Some(path) = token_file else {
        return true;
    };
    let Ok(expected) = read_private_token(Path::new(path)) else {
        return false;
    };
    provided.is_some_and(|provided| constant_time_eq(provided.as_bytes(), expected.as_bytes()))
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
        411 => "Length Required",
        413 => "Payload Too Large",
        431 => "Request Header Fields Too Large",
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

/// Closes our side, then discards a bounded amount of unread request input
/// so the kernel does not answer the peer with a reset that could destroy
/// the rejection response before the client reads it.
async fn drain_and_close(stream: &mut TcpStream) {
    const DRAIN_LIMIT: usize = 1024 * 1024;
    let _ = stream.shutdown().await;
    let drain = async {
        let mut chunk = [0_u8; 4096];
        let mut total = 0;
        while total < DRAIN_LIMIT {
            match stream.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(count) => total += count,
            }
        }
    };
    let _ = tokio::time::timeout(std::time::Duration::from_millis(500), drain).await;
}

/// Maps a transport-level read failure to its status code and error code.
/// I/O failures have no response because the connection is unusable.
#[must_use]
pub fn transport_rejection(error: &HttpReadError) -> IngestOutcome {
    let (status, code) = match error {
        HttpReadError::Malformed(_) | HttpReadError::Io(_) => (400, "malformed_request"),
        HttpReadError::LengthRequired => (411, "length_required"),
        HttpReadError::BodyTooLarge => (413, "payload_too_large"),
        HttpReadError::HeadersTooLarge => (431, "headers_too_large"),
    };
    outcome(status, serde_json::json!({ "error": code }), None)
}

pub async fn serve_connection(
    mut stream: TcpStream,
    store: Arc<HubStore>,
    token_file: Option<String>,
    max_body_bytes: usize,
) -> Option<HubPublication> {
    let request = match read_request(&mut stream, max_body_bytes).await {
        Ok(request) => request,
        Err(HttpReadError::Io(_)) => return None,
        Err(error) => {
            let _ = write_response(&mut stream, &transport_rejection(&error)).await;
            drain_and_close(&mut stream).await;
            return None;
        }
    };
    let result = handle_ingest(&store, token_file.as_deref(), &request);
    let publication = result.publication.clone();
    let _ = write_response(&mut stream, &result).await;
    publication
}
