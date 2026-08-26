use chrono::Utc;
use sessiontap_core::{
    domain::{
        Activity, Capabilities, EventKind, InvocationId, InvocationSnapshot, Lifecycle,
        ProcessMetadata, PublicStatus,
    },
    protocol::{
        HUB_SCHEMA_VERSION, HubEnvelope, HubEventMetadata, SourceCapabilities, SourceIdentity,
    },
};
use sessiontap_hub::ingest;
use sessiontap_hub::store::HubStore;
use std::sync::Arc;
use tokio::net::TcpListener;

fn fixture_snapshot() -> InvocationSnapshot {
    let now = Utc::now();
    InvocationSnapshot {
        schema_version: 1,
        revision: 1,
        invocation_id: InvocationId::new(),
        provider: "codex".into(),
        executable: "codex".into(),
        args: vec![],
        cwd: "/tmp".into(),
        process: ProcessMetadata::default(),
        created_at: now,
        updated_at: now,
        lifecycle: Lifecycle::Alive,
        activity: Activity::Idle,
        status: PublicStatus::Idle,
        provider_session: None,
        usage: None,
        repository: None,
        multiplexer: None,
        capabilities: Capabilities::default(),
        turn_generation: 0,
        completed_generation: None,
    }
}

fn snapshot_envelope(
    source: &str,
    revision: u64,
    invocations: Vec<InvocationSnapshot>,
) -> HubEnvelope {
    HubEnvelope::Snapshot {
        schema_version: HUB_SCHEMA_VERSION,
        source: SourceIdentity {
            id: source.into(),
            display_name: None,
            capabilities: SourceCapabilities::default(),
        },
        revision,
        invocations,
        active_attention: Default::default(),
    }
}

fn update_envelope(
    source: &str,
    event_id: &str,
    revision: u64,
    snapshot: InvocationSnapshot,
) -> HubEnvelope {
    let now = Utc::now();
    HubEnvelope::Update {
        schema_version: HUB_SCHEMA_VERSION,
        source_id: source.into(),
        event_id: event_id.into(),
        revision,
        event: HubEventMetadata {
            kind: EventKind::Working,
            observed_at: now,
            received_at: now,
            failure: None,
        },
        snapshot: Box::new(snapshot),
        attention: None,
    }
}

async fn spawn_hub(store: Arc<HubStore>, token_file: Option<String>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let store = Arc::clone(&store);
            let token_file = token_file.clone();
            tokio::spawn(async move {
                let _ = ingest::serve_connection(stream, store, token_file, 1024 * 1024).await;
            });
        }
    });
    format!("http://{address}")
}

async fn post(client: &reqwest::Client, url: &str, envelope: &HubEnvelope) -> serde_json::Value {
    let response = client
        .post(format!("{url}/ingest"))
        .header("content-type", "application/json")
        .body(serde_json::to_vec(envelope).unwrap())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    response.json().await.unwrap()
}

#[tokio::test]
async fn snapshot_then_updates_are_merged_idempotently_over_http() {
    let store = Arc::new(HubStore::memory().unwrap());
    let base = spawn_hub(Arc::clone(&store), None).await;
    let client = reqwest::Client::new();
    let invocation = fixture_snapshot();

    // update before any snapshot is rejected with the repair signal
    let response = client
        .post(format!("{base}/ingest"))
        .json(&update_envelope("host", "early", 1, invocation.clone()))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 409);
    assert_eq!(
        response.json::<serde_json::Value>().await.unwrap()["error"],
        "snapshot_required"
    );

    let applied = post(
        &client,
        &base,
        &snapshot_envelope("host", 1, vec![invocation.clone()]),
    )
    .await;
    assert_eq!(applied["status"], "applied");

    let mut changed = invocation.clone();
    changed.activity = Activity::Working;
    changed.status = PublicStatus::Running;
    changed.revision = 2;
    let applied = post(
        &client,
        &base,
        &update_envelope("host", "event-1", 2, changed),
    )
    .await;
    assert_eq!(applied["status"], "applied");

    // transport duplicate: acknowledged without double application
    let mut changed2 = invocation.clone();
    changed2.revision = 2;
    let retry = post(
        &client,
        &base,
        &update_envelope("host", "event-1", 2, changed2),
    )
    .await;
    assert_eq!(retry["status"], "duplicate");

    // stale revision is acknowledged without replacing newer state
    let stale = post(
        &client,
        &base,
        &update_envelope("host", "old-event", 1, invocation.clone()),
    )
    .await;
    assert_eq!(stale["status"], "stale");

    let (hub_revision, sources, invocations) = store.merged().unwrap();
    assert_eq!(hub_revision, 2);
    assert_eq!(sources[0].revision, 2);
    assert_eq!(invocations.len(), 1);
    assert_eq!(invocations[0].snapshot.activity, Activity::Working);
}

#[tokio::test]
async fn snapshots_from_multiple_sources_coexist() {
    let store = Arc::new(HubStore::memory().unwrap());
    let base = spawn_hub(Arc::clone(&store), None).await;
    let client = reqwest::Client::new();
    let host_agent = fixture_snapshot();
    let sandbox_agent = fixture_snapshot();
    post(
        &client,
        &base,
        &snapshot_envelope("host", 1, vec![host_agent.clone()]),
    )
    .await;
    post(
        &client,
        &base,
        &snapshot_envelope("sandbox", 1, vec![sandbox_agent.clone()]),
    )
    .await;
    let (_, sources, invocations) = store.merged().unwrap();
    assert_eq!(sources.len(), 2);
    assert_eq!(invocations.len(), 2);
}

#[tokio::test]
async fn malformed_and_future_envelopes_are_rejected_without_state() {
    let store = Arc::new(HubStore::memory().unwrap());
    let base = spawn_hub(Arc::clone(&store), None).await;
    let client = reqwest::Client::new();

    let response = client
        .post(format!("{base}/ingest"))
        .body("{not-json")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 400);

    let mut envelope = snapshot_envelope("host", 1, vec![]);
    if let HubEnvelope::Snapshot { schema_version, .. } = &mut envelope {
        *schema_version = 42;
    }
    let response = client
        .post(format!("{base}/ingest"))
        .json(&envelope)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 400);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["error"], "unsupported_schema_version");
    assert_eq!(store.revision().unwrap(), 0);
}

#[tokio::test]
async fn token_authentication_gates_ingestion() {
    let temp = tempfile::tempdir().unwrap();
    let token_path = temp.path().join("hub-token");
    std::fs::write(&token_path, "shared-secret\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&token_path, std::fs::Permissions::from_mode(0o600)).unwrap();

    let store = Arc::new(HubStore::memory().unwrap());
    let base = spawn_hub(
        Arc::clone(&store),
        Some(token_path.to_str().unwrap().into()),
    )
    .await;
    let client = reqwest::Client::new();
    let envelope = snapshot_envelope("host", 1, vec![fixture_snapshot()]);

    let response = client
        .post(format!("{base}/ingest"))
        .json(&envelope)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 401);

    let response = client
        .post(format!("{base}/ingest"))
        .bearer_auth("wrong-token")
        .json(&envelope)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 401);

    let response = client
        .post(format!("{base}/ingest"))
        .bearer_auth("shared-secret")
        .json(&envelope)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(store.revision().unwrap(), 1);
}

#[tokio::test]
async fn health_endpoint_is_public() {
    let temp = tempfile::tempdir().unwrap();
    let token_path = temp.path().join("hub-token");
    std::fs::write(&token_path, "shared-secret\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&token_path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let store = Arc::new(HubStore::memory().unwrap());
    let base = spawn_hub(
        Arc::clone(&store),
        Some(token_path.to_str().unwrap().into()),
    )
    .await;
    let client = reqwest::Client::new();
    let response = client.get(format!("{base}/health")).send().await.unwrap();
    assert_eq!(response.status(), 200);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["status"], "ok");
}

#[tokio::test]
async fn oversize_bodies_are_rejected() {
    let store = Arc::new(HubStore::memory().unwrap());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server_store = Arc::clone(&store);
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let store = Arc::clone(&server_store);
            tokio::spawn(async move {
                // tiny body limit forces rejection
                let _ = ingest::serve_connection(stream, store, None, 128).await;
            });
        }
    });
    let client = reqwest::Client::new();
    let envelope = snapshot_envelope("host", 1, vec![fixture_snapshot()]);
    let response = client
        .post(format!("http://{address}/ingest"))
        .json(&envelope)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 413);
    assert_eq!(store.revision().unwrap(), 0);
}

#[tokio::test]
async fn restart_recovery_serves_the_persisted_merged_state() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("hub.sqlite3");
    let invocation = fixture_snapshot();
    {
        let store = HubStore::open(&path).unwrap();
        store
            .ingest_snapshot(&snapshot_envelope("host", 1, vec![invocation.clone()]))
            .unwrap();
    }
    // hub restart: reopened store serves the persisted merged view
    let store = Arc::new(HubStore::open(&path).unwrap());
    let base = spawn_hub(Arc::clone(&store), None).await;
    let client = reqwest::Client::new();
    let body: serde_json::Value = client
        .get(format!("{base}/health"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["revision"], 1);
    let (_, _, invocations) = store.merged().unwrap();
    assert_eq!(invocations.len(), 1);
    // and a duplicate snapshot at the same revision is treated as stale
    let stale = post(&client, &base, &snapshot_envelope("host", 1, vec![])).await;
    assert_eq!(stale["status"], "stale");
}
