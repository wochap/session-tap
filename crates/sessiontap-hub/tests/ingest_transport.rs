use chrono::Utc;
use sessiontap_core::{
    domain::{
        InvocationId, PublicAgentView, PublicChildAgentReason, PublicChildAgentView, PublicField,
        PublicReasonKind, PublicStatus,
    },
    protocol::{SourceEnvelope, SourceIdentity},
};
use sessiontap_hub::{
    ingest::{IngestedRequest, handle_ingest},
    store::HubStore,
};
use std::collections::BTreeSet;

fn view(status: PublicStatus) -> PublicAgentView {
    PublicAgentView {
        invocation_id: InvocationId::new(),
        provider: "company-claude".into(),
        status,
        reason: None,
        cwd: "/work".into(),
        created_at: Utc::now(),
        updated_at: Utc::now(),
        session: None,
        metadata: None,
        usage: None,
        repository: None,
        children: None,
    }
}
fn child(agent_id: &str, status: PublicStatus) -> PublicChildAgentView {
    PublicChildAgentView {
        agent_id: agent_id.into(),
        agent_type: "Explore".into(),
        status,
        reason: None,
        started_at: Utc::now(),
        updated_at: Utc::now(),
    }
}

fn request(value: serde_json::Value) -> IngestedRequest {
    IngestedRequest {
        method: "POST".into(),
        path: "/ingest".into(),
        bearer: None,
        body: serde_json::to_vec(&value).unwrap(),
    }
}

#[test]
fn ingestion_discards_unknown_private_fields_and_deduplicates_delivery() {
    let store = HubStore::memory().unwrap();
    let idle = view(PublicStatus::Idle);
    let mut raw = serde_json::to_value(SourceEnvelope::Snapshot {
        schema_version: 1,
        source: SourceIdentity {
            id: "sandbox".into(),
            display_name: None,
        },
        revision: 1,
        views: vec![idle.clone()],
    })
    .unwrap();
    raw["credential"] = serde_json::json!("PRIVATE");
    raw["multiplexer"] = serde_json::json!({"pane":"PRIVATE"});
    assert_eq!(handle_ingest(&store, None, &request(raw)).status, 200);

    let mut running = idle;
    running.status = PublicStatus::Running;
    running.updated_at = Utc::now();
    let update = SourceEnvelope::Update {
        schema_version: 1,
        source_id: "sandbox".into(),
        delivery_id: "delivery-1".into(),
        revision: 2,
        changed: BTreeSet::from([PublicField::Status]),
        view: Box::new(running),
    };
    let body = serde_json::to_value(update).unwrap();
    assert_eq!(
        handle_ingest(&store, None, &request(body.clone())).body["status"],
        "applied"
    );
    assert_eq!(
        handle_ingest(&store, None, &request(body)).body["status"],
        "duplicate"
    );
    let (_, _, agents) = store.merged().unwrap();
    let serialized = serde_json::to_string(&agents).unwrap();
    assert!(!serialized.contains("PRIVATE"));
    assert!(!serialized.contains("multiplexer"));
}

#[test]
fn child_only_update_is_applied_with_children_changed() {
    let store = HubStore::memory().unwrap();
    let mut stopped = view(PublicStatus::Stopped);
    stopped.children = Some(vec![child("agent-1", PublicStatus::Running)]);
    let snapshot = SourceEnvelope::Snapshot {
        schema_version: 1,
        source: SourceIdentity {
            id: "sandbox".into(),
            display_name: None,
        },
        revision: 1,
        views: vec![stopped.clone()],
    };
    assert_eq!(
        handle_ingest(
            &store,
            None,
            &request(serde_json::to_value(snapshot).unwrap())
        )
        .status,
        200
    );

    let mut blocked = stopped;
    let mut approval = child("agent-1", PublicStatus::Blocked);
    approval.reason = Some(PublicChildAgentReason {
        kind: Some(PublicReasonKind::Approval),
        summary: Some("shell".into()),
    });
    blocked.children = Some(vec![approval]);
    let update = SourceEnvelope::Update {
        schema_version: 1,
        source_id: "sandbox".into(),
        delivery_id: "delivery-children".into(),
        revision: 2,
        changed: BTreeSet::from([PublicField::Children]),
        view: Box::new(blocked.clone()),
    };
    let response = handle_ingest(
        &store,
        None,
        &request(serde_json::to_value(update).unwrap()),
    );
    assert_eq!(response.body["status"], "applied");
    let (_, _, agents) = store.merged().unwrap();
    assert_eq!(agents[0].view, blocked);
    assert_eq!(agents[0].view.status, PublicStatus::Stopped);
}

#[test]
fn reasonless_interruption_projection_is_not_completion_routable() {
    let store = HubStore::memory().unwrap();
    let idle = view(PublicStatus::Idle);
    handle_ingest(
        &store,
        None,
        &request(
            serde_json::to_value(SourceEnvelope::Snapshot {
                schema_version: 1,
                source: SourceIdentity {
                    id: "sandbox".into(),
                    display_name: None,
                },
                revision: 1,
                views: vec![idle.clone()],
            })
            .unwrap(),
        ),
    );

    let mut interrupted = idle;
    interrupted.status = PublicStatus::Stopped;
    interrupted.reason = None;
    interrupted.updated_at = Utc::now();
    let update = SourceEnvelope::Update {
        schema_version: 1,
        source_id: "sandbox".into(),
        delivery_id: "interrupted".into(),
        revision: 2,
        changed: BTreeSet::from([PublicField::Status]),
        view: Box::new(interrupted),
    };
    assert_eq!(
        handle_ingest(
            &store,
            None,
            &request(serde_json::to_value(update).unwrap())
        )
        .body["status"],
        "applied"
    );
    let (_, _, agents) = store.merged().unwrap();
    assert_eq!(agents[0].view.status, PublicStatus::Stopped);
    assert!(agents[0].view.reason.is_none());
    let serialized = serde_json::to_string(&agents).unwrap();
    assert!(!serialized.contains("completed"));
    assert!(!serialized.contains("failed"));
}

/// Sends raw bytes to `serve_connection` over TCP and returns the status
/// code, error code, and whether any publication resulted.
async fn exchange(raw: Vec<u8>, max_body: usize) -> (u16, serde_json::Value, bool) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let store = std::sync::Arc::new(HubStore::memory().unwrap());
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        sessiontap_hub::ingest::serve_connection(stream, store, None, max_body).await
    });
    let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
    client.write_all(&raw).await.unwrap();
    client.shutdown().await.unwrap();
    let mut response = String::new();
    client.read_to_string(&mut response).await.unwrap();
    let published = server.await.unwrap().is_some();
    let status = response.split_whitespace().nth(1).unwrap().parse().unwrap();
    let body = response.split_once("\r\n\r\n").unwrap().1;
    (status, serde_json::from_str(body).unwrap(), published)
}

fn snapshot_body() -> Vec<u8> {
    serde_json::to_vec(&SourceEnvelope::Snapshot {
        schema_version: 1,
        source: SourceIdentity {
            id: "sandbox".into(),
            display_name: None,
        },
        revision: 1,
        views: vec![view(PublicStatus::Idle)],
    })
    .unwrap()
}

fn post(headers: &str, body: &[u8]) -> Vec<u8> {
    let mut raw = format!("POST /ingest HTTP/1.1\r\nhost: hub\r\n{headers}\r\n").into_bytes();
    raw.extend_from_slice(body);
    raw
}

#[tokio::test]
async fn transport_rejections_use_distinct_status_codes() {
    let body = snapshot_body();
    let cases: Vec<(Vec<u8>, u16, &str)> = vec![
        (b"not http at all".to_vec(), 400, "malformed_request"),
        (b"POST /ingest\r\n\r\n".to_vec(), 400, "malformed_request"),
        (post("", &body), 411, "length_required"),
        (
            post("content-length: many\r\n", &body),
            411,
            "length_required",
        ),
        (
            post(&format!("content-length: {}\r\n", 1 << 20), b""),
            413,
            "payload_too_large",
        ),
        (
            post(&format!("x-big: {}\r\n", "a".repeat(70 * 1024)), b""),
            431,
            "headers_too_large",
        ),
    ];
    for (raw, status, code) in cases {
        let (got, body, published) = exchange(raw, 64 * 1024).await;
        assert_eq!((got, body["error"].as_str()), (status, Some(code)));
        assert!(!published);
    }
}

#[tokio::test]
async fn valid_envelope_is_still_accepted() {
    let body = snapshot_body();
    let (status, response, published) = exchange(
        post(&format!("content-length: {}\r\n", body.len()), &body),
        64 * 1024,
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(response["status"], "applied");
    assert!(published);
    let (status, _, _) = exchange(b"GET /health HTTP/1.1\r\n\r\n".to_vec(), 64 * 1024).await;
    assert_eq!(status, 200);
}
