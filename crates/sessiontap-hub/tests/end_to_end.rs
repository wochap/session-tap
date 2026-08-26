use chrono::Utc;
use sessiontap_core::{
    domain::{
        Activity, AttentionContext, AttentionSource, Capabilities, EventKind, InvocationId,
        InvocationSnapshot, Lifecycle, ProcessMetadata, PublicStatus,
    },
    protocol::{
        HUB_SCHEMA_VERSION, HubEnvelope, HubEventMetadata, SourceCapabilities, SourceIdentity,
    },
};
use sessiontap_hub::config::{MatchCriteria, Subscription};
use sessiontap_hub::ingest::{self, HubPublication};
use sessiontap_hub::listen::{self, HubRequest, HubStreamEnvelope};
use sessiontap_hub::routing;
use sessiontap_hub::store::HubStore;
use std::{sync::Arc, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, UnixStream},
    sync::broadcast,
};

fn fixture(provider: &str) -> InvocationSnapshot {
    let now = Utc::now();
    InvocationSnapshot {
        schema_version: 1,
        revision: 1,
        invocation_id: InvocationId::new(),
        provider: provider.into(),
        executable: provider.into(),
        args: vec![],
        cwd: "/tmp".into(),
        process: ProcessMetadata::default(),
        created_at: now,
        updated_at: now,
        lifecycle: Lifecycle::Alive,
        activity: Activity::Working,
        status: PublicStatus::Running,
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
            display_name: Some(format!("{source} machine")),
            capabilities: SourceCapabilities::default(),
        },
        revision,
        invocations,
        active_attention: Default::default(),
    }
}

fn waiting_update(
    source: &str,
    event_id: &str,
    revision: u64,
    snapshot: InvocationSnapshot,
    summary: &str,
) -> HubEnvelope {
    let now = Utc::now();
    HubEnvelope::Update {
        schema_version: HUB_SCHEMA_VERSION,
        source_id: source.into(),
        event_id: event_id.into(),
        revision,
        event: HubEventMetadata {
            kind: EventKind::WaitingInput,
            observed_at: now,
            received_at: now,
            failure: None,
        },
        snapshot: Box::new(snapshot),
        attention: Some(sessiontap_core::domain::ActiveAttention {
            kind: EventKind::WaitingInput,
            context: AttentionContext {
                summary: summary.into(),
                source: AttentionSource::Question,
            },
        }),
    }
}

async fn spawn_hub(store: Arc<HubStore>, sender: broadcast::Sender<HubPublication>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let store = Arc::clone(&store);
            let sender = sender.clone();
            tokio::spawn(async move {
                if let Some(publication) =
                    ingest::serve_connection(stream, store, None, 1024 * 1024).await
                {
                    let _ = sender.send(publication);
                }
            });
        }
    });
    format!("http://{address}")
}

async fn post(
    client: &reqwest::Client,
    base: &str,
    envelope: &HubEnvelope,
) -> (u16, serde_json::Value) {
    let response = client
        .post(format!("{base}/ingest"))
        .json(envelope)
        .send()
        .await
        .unwrap();
    let status = response.status().as_u16();
    (status, response.json().await.unwrap())
}

async fn read_envelope(reader: &mut BufReader<UnixStream>) -> HubStreamEnvelope {
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    serde_json::from_str(&line).unwrap()
}

async fn wait_for_file_contents(path: &std::path::Path, expected: &str) {
    for _ in 0..200 {
        if let Ok(contents) = std::fs::read_to_string(path) {
            if contents == expected {
                return;
            }
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("expected {} to contain {expected:?}", path.display());
}

#[tokio::test]
async fn host_and_sandbox_merge_repair_route_and_stream() {
    let temp = tempfile::tempdir().unwrap();
    let store = Arc::new(HubStore::memory().unwrap());
    let (updates, _sender) = broadcast::channel::<HubPublication>(256);
    let base = spawn_hub(Arc::clone(&store), updates.clone()).await;
    let client = reqwest::Client::new();

    // merged live consumer connects before ingestion continues
    let (client_sock, server_sock) = UnixStream::pair().unwrap();
    let listener_task = tokio::spawn(listen::serve_listener(
        server_sock,
        Arc::clone(&store),
        updates.subscribe(),
    ));

    // routing task mirrors the service wiring: only accepted updates arrive
    let marker_dir = temp.path().join("markers");
    std::fs::create_dir(&marker_dir).unwrap();
    let subscription = Subscription {
        name: Some("waiting".into()),
        match_criteria: MatchCriteria {
            events: vec!["waiting_input".into()],
            ..Default::default()
        },
        changes: vec!["attention".into()],
        commands: vec![vec![
            "/bin/sh".into(),
            "-c".into(),
            format!(
                "cat > {dir}/envelope.json && printf '%s|%s|%s' \"$SESSIONTAP_SOURCE\" \"$SESSIONTAP_EVENT\" \"$SESSIONTAP_ATTENTION_SUMMARY\" > {dir}/env.txt && date +%s%N >> {dir}/runs",
                dir = marker_dir.display()
            ),
        ]],
    };
    let mut route_rx = updates.subscribe();
    tokio::spawn(async move {
        while let Ok(publication) = route_rx.recv().await {
            if let HubPublication::Update(update) = publication {
                routing::dispatch(vec![subscription.clone()], *update);
            }
        }
    });

    let mut reader = BufReader::new(client_sock);
    reader
        .write_all(&serde_json::to_vec(&HubRequest::Listen).unwrap())
        .await
        .unwrap();
    reader.write_all(b"\n").await.unwrap();

    // empty baseline first
    match read_envelope(&mut reader).await {
        HubStreamEnvelope::Snapshot {
            hub_revision,
            sources,
            invocations,
        } => {
            assert_eq!(hub_revision, 0);
            assert!(sources.is_empty());
            assert!(invocations.is_empty());
        }
        other => panic!("expected empty snapshot, got {other:?}"),
    }

    // host and sandbox establish baselines with running agents
    let host_agent = fixture("claude");
    let sandbox_agent = fixture("codex");
    let (status, body) = post(
        &client,
        &base,
        &snapshot_envelope("host", 1, vec![host_agent.clone()]),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(body["status"], "applied");
    let (status, _) = post(
        &client,
        &base,
        &snapshot_envelope("sandbox", 1, vec![sandbox_agent.clone()]),
    )
    .await;
    assert_eq!(status, 200);

    // merged status: both sources and agents present
    let (_, sources, invocations) = store.merged().unwrap();
    assert_eq!(sources.len(), 2);
    assert_eq!(invocations.len(), 2);
    assert!(invocations.iter().all(|i| i.attention.is_none()));

    // sandbox agent becomes blocked with attention; the route must fire
    let mut blocked = sandbox_agent.clone();
    blocked.activity = Activity::WaitingInput;
    blocked.status = PublicStatus::Blocked;
    blocked.revision = 2;
    let (status, body) = post(
        &client,
        &base,
        &waiting_update("sandbox", "wait-1", 2, blocked.clone(), "Choose an option"),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(body["status"], "applied");
    wait_for_file_contents(
        &marker_dir.join("env.txt"),
        "sandbox|waiting_input|Choose an option",
    )
    .await;
    let envelope: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(marker_dir.join("envelope.json")).unwrap())
            .unwrap();
    assert_eq!(envelope["type"], "update");
    assert_eq!(envelope["source_id"], "sandbox");
    assert_eq!(
        envelope["attention"]["context"]["summary"],
        "Choose an option"
    );

    // retry idempotency: redelivery acks without re-running the script
    let (status, body) = post(
        &client,
        &base,
        &waiting_update("sandbox", "wait-1", 2, blocked.clone(), "Choose an option"),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(body["status"], "duplicate");
    tokio::time::sleep(Duration::from_millis(150)).await;
    let runs = std::fs::read_to_string(marker_dir.join("runs")).unwrap();
    assert_eq!(runs.lines().count(), 1);

    // attention changes while status remains blocked: route fires again
    let (status, _) = post(
        &client,
        &base,
        &waiting_update("sandbox", "wait-2", 3, blocked.clone(), "Second question"),
    )
    .await;
    assert_eq!(status, 200);
    wait_for_file_contents(
        &marker_dir.join("env.txt"),
        "sandbox|waiting_input|Second question",
    )
    .await;

    // snapshot repair: an unknown source must baseline before updates apply
    let stray = fixture("qwen");
    let (status, body) = post(
        &client,
        &base,
        &waiting_update("sandbox-new", "s-1", 1, stray.clone(), "Hi"),
    )
    .await;
    assert_eq!(status, 409);
    assert_eq!(body["error"], "snapshot_required");
    let (status, _) = post(
        &client,
        &base,
        &snapshot_envelope("sandbox-new", 1, vec![stray.clone()]),
    )
    .await;
    assert_eq!(status, 200);
    let mut stray_blocked = stray.clone();
    stray_blocked.activity = Activity::WaitingInput;
    stray_blocked.status = PublicStatus::Blocked;
    stray_blocked.revision = 2;
    let (status, body) = post(
        &client,
        &base,
        &waiting_update("sandbox-new", "s-1", 2, stray_blocked, "Hi"),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(body["status"], "applied");

    // live consumer saw the merged baseline deltas and every accepted update
    // gap-free: collect until three waiting updates are observed
    let mut saw_updates = 0;
    let mut previous = 2_u64; // after two snapshot applications
    while saw_updates < 3 {
        match read_envelope(&mut reader).await {
            HubStreamEnvelope::Snapshot { hub_revision, .. } => {
                previous = hub_revision.max(previous);
            }
            HubStreamEnvelope::Update {
                hub_revision,
                source_id,
                ..
            } => {
                assert_eq!(hub_revision, previous + 1, "live stream gap detected");
                previous = hub_revision;
                if source_id == "sandbox" || source_id == "sandbox-new" {
                    saw_updates += 1;
                }
            }
        }
    }

    // merged attention state is explicit and clearable
    let (_, _, invocations) = store.merged().unwrap();
    let sandbox_view = invocations
        .iter()
        .find(|i| i.source_id == "sandbox")
        .unwrap();
    assert_eq!(
        sandbox_view.attention.as_ref().unwrap().context.summary,
        "Second question"
    );
    let host_view = invocations.iter().find(|i| i.source_id == "host").unwrap();
    assert!(host_view.attention.is_none());

    drop(reader);
    listener_task.abort();
}
