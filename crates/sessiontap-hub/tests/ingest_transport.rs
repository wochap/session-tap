use chrono::Utc;
use sessiontap_core::{
    domain::{
        InvocationId, PublicAgentView, PublicChildAgentReason, PublicChildAgentView, PublicField,
        PublicReasonKind, PublicStatus,
    },
    protocol::{SourceEnvelope, SourceIdentity},
};
use sessiontap_hub::{
    config::SourceAuth,
    ingest::{IngestAuth, IngestedRequest, handle_ingest},
    store::HubStore,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

fn open() -> IngestAuth {
    IngestAuth::default()
}

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
        terminal: None,
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
    assert_eq!(handle_ingest(&store, &open(), &request(raw)).status, 200);

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
        handle_ingest(&store, &open(), &request(body.clone())).body["status"],
        "applied"
    );
    assert_eq!(
        handle_ingest(&store, &open(), &request(body)).body["status"],
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
            &open(),
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
        &open(),
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
        &open(),
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
            &open(),
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
    exchange_with(raw, max_body, open()).await
}

async fn exchange_with(
    raw: Vec<u8>,
    max_body: usize,
    auth: IngestAuth,
) -> (u16, serde_json::Value, bool) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let store = std::sync::Arc::new(HubStore::memory().unwrap());
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        sessiontap_hub::ingest::serve_connection(
            stream,
            store,
            Arc::new(auth),
            max_body,
            Default::default(),
        )
        .await
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
}

#[tokio::test]
async fn health_reveals_no_revision_with_or_without_sources() {
    let temp = tempfile::tempdir().unwrap();
    let (auth, _) = auth_with(&temp, &[("host", "host-token")]);
    for auth in [open(), auth] {
        let (status, body, _) =
            exchange_with(b"GET /health HTTP/1.1\r\n\r\n".to_vec(), 64 * 1024, auth).await;
        assert_eq!(status, 200);
        assert_eq!(body, serde_json::json!({"status":"ok"}));
    }
}

#[tokio::test]
async fn authenticated_transport_answers_401_and_403() {
    let temp = tempfile::tempdir().unwrap();
    let (auth, _) = auth_with(&temp, &[("sandbox", "sandbox-token")]);
    let body = snapshot_body();
    let length = format!("content-length: {}\r\n", body.len());
    let (status, response, published) =
        exchange_with(post(&length, &body), 64 * 1024, auth.clone()).await;
    assert_eq!(
        (status, response["error"].as_str()),
        (401, Some("unauthorized"))
    );
    assert!(!published);
    let headers = format!("{length}authorization: Bearer sandbox-token\r\n");
    let (status, _, published) = exchange_with(post(&headers, &body), 64 * 1024, auth).await;
    assert_eq!(status, 200);
    assert!(published);
}

#[test]
fn tombstoned_update_is_acknowledged_without_publication() {
    let store = HubStore::memory().unwrap();
    let mut stopped = view(PublicStatus::Stopped);
    let post = |envelope: &SourceEnvelope| IngestedRequest {
        method: "POST".into(),
        path: "/v1/envelopes".into(),
        bearer: None,
        body: serde_json::to_vec(envelope).unwrap(),
    };
    let snapshot = SourceEnvelope::Snapshot {
        schema_version: 1,
        source: SourceIdentity {
            id: "host".into(),
            display_name: None,
        },
        revision: 1,
        views: vec![stopped.clone()],
    };
    assert!(
        handle_ingest(&store, &open(), &post(&snapshot))
            .publication
            .is_some()
    );
    store
        .forget("host", &stopped.invocation_id.to_string())
        .unwrap();
    stopped.updated_at = Utc::now() + chrono::Duration::seconds(1);
    let update = SourceEnvelope::Update {
        schema_version: 1,
        source_id: "host".into(),
        delivery_id: "late".into(),
        revision: 2,
        changed: BTreeSet::from([PublicField::UpdatedAt]),
        view: Box::new(stopped),
    };
    let outcome = handle_ingest(&store, &open(), &post(&update));
    assert_eq!(outcome.status, 200);
    assert_eq!(outcome.body["status"], "suppressed");
    assert!(outcome.publication.is_none());
    assert!(store.merged().unwrap().2.is_empty());
}

/// Writes one private token file per `(source, token)` pair, returning the
/// auth and the token file paths keyed by source.
fn auth_with(
    temp: &tempfile::TempDir,
    tokens: &[(&str, &str)],
) -> (IngestAuth, BTreeMap<String, std::path::PathBuf>) {
    use std::os::unix::fs::PermissionsExt;
    let mut sources = BTreeMap::new();
    let mut paths = BTreeMap::new();
    for (source, token) in tokens {
        let path = temp.path().join(format!("{source}.token"));
        std::fs::write(&path, token).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        sources.insert(
            (*source).to_owned(),
            SourceAuth {
                token_file: path.to_string_lossy().into_owned(),
            },
        );
        paths.insert((*source).to_owned(), path);
    }
    (IngestAuth::new(&sources), paths)
}

fn authed(bearer: Option<&str>, envelope: &SourceEnvelope) -> IngestedRequest {
    IngestedRequest {
        method: "POST".into(),
        path: "/ingest".into(),
        bearer: bearer.map(str::to_owned),
        body: serde_json::to_vec(envelope).unwrap(),
    }
}

fn snapshot_for(source: &str, views: Vec<PublicAgentView>) -> SourceEnvelope {
    SourceEnvelope::Snapshot {
        schema_version: 1,
        source: SourceIdentity {
            id: source.into(),
            display_name: None,
        },
        revision: 1,
        views,
    }
}

fn update_for(source: &str, mut view: PublicAgentView, delivery: &str) -> SourceEnvelope {
    view.status = PublicStatus::Running;
    view.updated_at = Utc::now() + chrono::Duration::seconds(1);
    SourceEnvelope::Update {
        schema_version: 1,
        source_id: source.into(),
        delivery_id: delivery.into(),
        revision: 2,
        changed: BTreeSet::from([PublicField::Status]),
        view: Box::new(view),
    }
}

#[test]
fn missing_or_unknown_bearer_is_unauthorized_before_parsing() {
    let temp = tempfile::tempdir().unwrap();
    let (auth, _) = auth_with(&temp, &[("host", "host-token")]);
    let store = HubStore::memory().unwrap();
    let snapshot = snapshot_for("host", vec![view(PublicStatus::Idle)]);
    let missing = handle_ingest(&store, &auth, &authed(None, &snapshot));
    assert_eq!(
        (missing.status, missing.body["error"].as_str()),
        (401, Some("unauthorized"))
    );
    let malformed = IngestedRequest {
        method: "POST".into(),
        path: "/ingest".into(),
        bearer: Some("wrong".into()),
        body: b"{not json".to_vec(),
    };
    let unknown = handle_ingest(&store, &auth, &malformed);
    assert_eq!(
        (unknown.status, unknown.body["error"].as_str()),
        (401, Some("unauthorized"))
    );
    assert!(unknown.publication.is_none());
    assert!(store.merged().unwrap().2.is_empty());
}

#[test]
fn source_token_cannot_write_another_source() {
    let temp = tempfile::tempdir().unwrap();
    let (auth, _) = auth_with(
        &temp,
        &[("host", "host-token"), ("sandbox", "sandbox-token")],
    );
    let store = HubStore::memory().unwrap();
    let host_view = view(PublicStatus::Idle);
    let snapshot = snapshot_for("host", vec![host_view.clone()]);
    assert_eq!(
        handle_ingest(&store, &auth, &authed(Some("host-token"), &snapshot)).status,
        200
    );
    let before = serde_json::to_string(&store.merged().unwrap().2).unwrap();
    let revision = store.revision().unwrap();

    let forged_snapshot = snapshot_for("host", vec![]);
    let forged_update = update_for("host", host_view, "forged");
    for envelope in [&forged_snapshot, &forged_update] {
        let outcome = handle_ingest(&store, &auth, &authed(Some("sandbox-token"), envelope));
        assert_eq!(
            (outcome.status, outcome.body["error"].as_str()),
            (403, Some("source_not_permitted"))
        );
        assert!(outcome.publication.is_none());
    }
    assert_eq!(
        serde_json::to_string(&store.merged().unwrap().2).unwrap(),
        before
    );
    assert_eq!(store.revision().unwrap(), revision);

    let rogue = snapshot_for("rogue", vec![view(PublicStatus::Idle)]);
    let outcome = handle_ingest(&store, &auth, &authed(Some("sandbox-token"), &rogue));
    assert_eq!(outcome.status, 403);
    assert_eq!(store.revision().unwrap(), revision);
}

#[test]
fn own_source_snapshot_and_update_apply() {
    let temp = tempfile::tempdir().unwrap();
    let (auth, _) = auth_with(&temp, &[("sandbox", "sandbox-token")]);
    let store = HubStore::memory().unwrap();
    let idle = view(PublicStatus::Idle);
    let snapshot = snapshot_for("sandbox", vec![idle.clone()]);
    let outcome = handle_ingest(&store, &auth, &authed(Some("sandbox-token"), &snapshot));
    assert_eq!(outcome.body["status"], "applied");
    let update = update_for("sandbox", idle, "own");
    let outcome = handle_ingest(&store, &auth, &authed(Some("sandbox-token"), &update));
    assert_eq!(outcome.body["status"], "applied");
    assert!(outcome.publication.is_some());
}

#[test]
fn shared_token_file_binds_every_source_using_it() {
    let temp = tempfile::tempdir().unwrap();
    let (_, paths) = auth_with(&temp, &[("shared", "shared-token")]);
    let file = paths["shared"].to_string_lossy().into_owned();
    let sources = BTreeMap::from([
        (
            "host".to_owned(),
            SourceAuth {
                token_file: file.clone(),
            },
        ),
        ("sandbox".to_owned(), SourceAuth { token_file: file }),
    ]);
    let auth = IngestAuth::new(&sources);
    let store = HubStore::memory().unwrap();
    for source in ["host", "sandbox"] {
        let snapshot = snapshot_for(source, vec![view(PublicStatus::Idle)]);
        assert_eq!(
            handle_ingest(&store, &auth, &authed(Some("shared-token"), &snapshot)).status,
            200
        );
    }
}

#[test]
fn non_private_token_file_authorizes_nothing() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    let (auth, paths) = auth_with(&temp, &[("host", "host-token")]);
    std::fs::set_permissions(&paths["host"], std::fs::Permissions::from_mode(0o644)).unwrap();
    let store = HubStore::memory().unwrap();
    let snapshot = snapshot_for("host", vec![view(PublicStatus::Idle)]);
    let outcome = handle_ingest(&store, &auth, &authed(Some("host-token"), &snapshot));
    assert_eq!(outcome.status, 401);
}

#[test]
fn rotated_token_takes_effect_on_next_request() {
    let temp = tempfile::tempdir().unwrap();
    let (auth, paths) = auth_with(&temp, &[("host", "old-token")]);
    let store = HubStore::memory().unwrap();
    let snapshot = snapshot_for("host", vec![view(PublicStatus::Idle)]);
    assert_eq!(
        handle_ingest(&store, &auth, &authed(Some("old-token"), &snapshot)).status,
        200
    );
    std::fs::write(&paths["host"], "new-token").unwrap();
    assert_eq!(
        handle_ingest(&store, &auth, &authed(Some("old-token"), &snapshot)).status,
        401
    );
    assert_eq!(
        handle_ingest(&store, &auth, &authed(Some("new-token"), &snapshot)).status,
        200
    );
}

#[test]
fn no_op_update_is_rejected_with_detail_and_changes_nothing() {
    let store = HubStore::memory().unwrap();
    let idle = view(PublicStatus::Idle);
    assert_eq!(
        handle_ingest(
            &store,
            &open(),
            &authed(None, &snapshot_for("sandbox", vec![idle.clone()]))
        )
        .status,
        200
    );
    let before = serde_json::to_value(store.merged().unwrap().2).unwrap();
    let unchanged = SourceEnvelope::Update {
        schema_version: 1,
        source_id: "sandbox".into(),
        delivery_id: "noop".into(),
        revision: 2,
        changed: BTreeSet::from([PublicField::Status]),
        view: Box::new(idle),
    };
    let outcome = handle_ingest(&store, &open(), &authed(None, &unchanged));
    assert_eq!(outcome.status, 400);
    assert_eq!(outcome.body["error"], "malformed_envelope");
    let detail = outcome.body["detail"].as_str().unwrap();
    assert!(!detail.is_empty());
    assert!(outcome.publication.is_none());
    assert_eq!(
        serde_json::to_value(store.merged().unwrap().2).unwrap(),
        before
    );
}
