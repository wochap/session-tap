use chrono::Utc;
use sessiontap_core::{
    domain::{
        ActiveAttention, Activity, AttentionContext, AttentionSource, Capabilities, EventKind,
        InvocationId, InvocationSnapshot, Lifecycle, ProcessMetadata, PublicStatus,
    },
    protocol::{
        HUB_SCHEMA_VERSION, HubEnvelope, HubEventMetadata, SourceCapabilities, SourceIdentity,
    },
};
use sessiontap_hub::ingest::{AcceptedUpdate, HubPublication};
use sessiontap_hub::listen::{self, HubRequest, HubStreamEnvelope};
use sessiontap_hub::store::HubStore;
use std::sync::Arc;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    sync::broadcast,
};

const BROADCAST_CAPACITY: usize = 1024;

fn fixture_snapshot() -> InvocationSnapshot {
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
        usage: None,
        repository: None,
        multiplexer: None,
        capabilities: Capabilities::default(),
        turn_generation: 0,
        completed_generation: None,
    }
}

fn source_snapshot(invocations: Vec<InvocationSnapshot>) -> HubEnvelope {
    HubEnvelope::Snapshot {
        schema_version: HUB_SCHEMA_VERSION,
        source: SourceIdentity {
            id: "host".into(),
            display_name: Some("Host machine".into()),
            capabilities: SourceCapabilities::default(),
        },
        revision: 1,
        invocations,
        active_attention: Default::default(),
    }
}

fn accepted_update(hub_revision: u64, snapshot: InvocationSnapshot) -> AcceptedUpdate {
    let now = Utc::now();
    AcceptedUpdate {
        hub_revision,
        source_id: "host".into(),
        event_id: format!("event-{hub_revision}"),
        event: HubEventMetadata {
            kind: EventKind::WaitingInput,
            observed_at: now,
            received_at: now,
            failure: None,
        },
        snapshot,
        attention: Some(ActiveAttention {
            kind: EventKind::WaitingInput,
            context: AttentionContext {
                summary: "Waiting".into(),
                source: AttentionSource::GenericInput,
            },
        }),
        changed: vec!["attention".into()],
        first_seen: false,
    }
}

async fn connect_listener(
    store: Arc<HubStore>,
    updates: &broadcast::Sender<HubPublication>,
) -> (UnixStream, tokio::task::JoinHandle<anyhow::Result<()>>) {
    let (client, server) = UnixStream::pair().unwrap();
    let task = tokio::spawn(listen::serve_listener(server, store, updates.subscribe()));
    (client, task)
}

async fn send_listen(stream: &mut UnixStream) {
    stream
        .write_all(&serde_json::to_vec(&HubRequest::Listen).unwrap())
        .await
        .unwrap();
    stream.write_all(b"\n").await.unwrap();
}

async fn read_line(reader: &mut BufReader<UnixStream>) -> HubStreamEnvelope {
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    serde_json::from_str(&line).unwrap()
}

#[tokio::test]
async fn listener_receives_baseline_then_live_updates() {
    let store = Arc::new(HubStore::memory().unwrap());
    let snapshot = fixture_snapshot();
    store
        .ingest_snapshot(&source_snapshot(vec![snapshot.clone()]))
        .unwrap();
    let (updates, _sender) = broadcast::channel(BROADCAST_CAPACITY);
    let (mut stream, task) = connect_listener(Arc::clone(&store), &updates).await;
    send_listen(&mut stream).await;
    let mut reader = BufReader::new(stream);
    match read_line(&mut reader).await {
        HubStreamEnvelope::Snapshot {
            hub_revision,
            sources,
            invocations,
        } => {
            assert_eq!(hub_revision, 1);
            assert_eq!(sources[0].source_id, "host");
            assert_eq!(sources[0].display_name.as_deref(), Some("Host machine"));
            assert_eq!(invocations.len(), 1);
        }
        other => panic!("expected snapshot, got {other:?}"),
    }
    let mut changed = snapshot.clone();
    changed.activity = Activity::WaitingInput;
    changed.status = PublicStatus::Blocked;
    changed.revision = 2;
    updates
        .send(HubPublication::Update(Box::new(accepted_update(
            2, changed,
        ))))
        .unwrap();
    match read_line(&mut reader).await {
        HubStreamEnvelope::Update {
            hub_revision,
            source_id,
            event,
            attention,
            changed: fields,
            ..
        } => {
            assert_eq!(hub_revision, 2);
            assert_eq!(source_id, "host");
            assert_eq!(event.kind, EventKind::WaitingInput);
            assert!(attention.is_some());
            assert_eq!(fields, vec!["attention".to_owned()]);
        }
        other => panic!("expected update, got {other:?}"),
    }
    drop(reader);
    task.abort();
}

#[tokio::test]
async fn updates_at_or_below_baseline_are_never_reemitted() {
    let store = Arc::new(HubStore::memory().unwrap());
    let snapshot = fixture_snapshot();
    store
        .ingest_snapshot(&source_snapshot(vec![snapshot.clone()]))
        .unwrap();
    let (updates, _sender) = broadcast::channel(BROADCAST_CAPACITY);
    let (mut stream, task) = connect_listener(Arc::clone(&store), &updates).await;
    send_listen(&mut stream).await;
    let mut reader = BufReader::new(stream);
    assert!(matches!(
        read_line(&mut reader).await,
        HubStreamEnvelope::Snapshot { .. }
    ));
    updates
        .send(HubPublication::Update(Box::new(accepted_update(
            1,
            snapshot.clone(),
        ))))
        .unwrap();
    let mut later = snapshot.clone();
    later.revision = 2;
    updates
        .send(HubPublication::Update(Box::new(accepted_update(2, later))))
        .unwrap();
    match read_line(&mut reader).await {
        HubStreamEnvelope::Update { hub_revision, .. } => assert_eq!(hub_revision, 2),
        other => panic!("expected only the later update, got {other:?}"),
    }
    drop(reader);
    task.abort();
}

#[tokio::test]
async fn reconnecting_consumer_receives_a_new_complete_baseline() {
    let store = Arc::new(HubStore::memory().unwrap());
    let snapshot = fixture_snapshot();
    store
        .ingest_snapshot(&source_snapshot(vec![snapshot.clone()]))
        .unwrap();
    let (updates, _sender) = broadcast::channel(BROADCAST_CAPACITY);
    for _ in 0..2 {
        let (mut stream, task) = connect_listener(Arc::clone(&store), &updates).await;
        send_listen(&mut stream).await;
        let mut reader = BufReader::new(stream);
        match read_line(&mut reader).await {
            HubStreamEnvelope::Snapshot { invocations, .. } => {
                assert_eq!(invocations.len(), 1);
            }
            other => panic!("expected snapshot, got {other:?}"),
        }
        drop(reader);
        task.abort();
    }
}

#[tokio::test]
async fn concurrent_consumers_each_receive_the_full_stream() {
    let store = Arc::new(HubStore::memory().unwrap());
    let snapshot = fixture_snapshot();
    store
        .ingest_snapshot(&source_snapshot(vec![snapshot.clone()]))
        .unwrap();
    let (updates, _sender) = broadcast::channel(BROADCAST_CAPACITY);
    let mut streams = Vec::new();
    for _ in 0..4 {
        let (mut stream, task) = connect_listener(Arc::clone(&store), &updates).await;
        send_listen(&mut stream).await;
        streams.push((BufReader::new(stream), task));
    }
    let mut changed = snapshot.clone();
    changed.activity = Activity::Working;
    changed.status = PublicStatus::Running;
    changed.revision = 2;
    updates
        .send(HubPublication::Update(Box::new(accepted_update(
            2, changed,
        ))))
        .unwrap();
    for (reader, task) in streams.iter_mut() {
        assert!(matches!(
            read_line(reader).await,
            HubStreamEnvelope::Snapshot { .. }
        ));
        match read_line(reader).await {
            HubStreamEnvelope::Update { hub_revision, .. } => assert_eq!(hub_revision, 2),
            other => panic!("expected update, got {other:?}"),
        }
        task.abort();
    }
}

#[tokio::test]
async fn continuous_ingestion_produces_a_gap_free_update_sequence() {
    let store = Arc::new(HubStore::memory().unwrap());
    let snapshot = fixture_snapshot();
    store
        .ingest_snapshot(&source_snapshot(vec![snapshot.clone()]))
        .unwrap();
    let (updates, _sender) = broadcast::channel(BROADCAST_CAPACITY);
    let (mut stream, task) = connect_listener(Arc::clone(&store), &updates).await;
    send_listen(&mut stream).await;
    let mut reader = BufReader::new(stream);
    match read_line(&mut reader).await {
        HubStreamEnvelope::Snapshot { hub_revision, .. } => assert_eq!(hub_revision, 1),
        other => panic!("expected snapshot, got {other:?}"),
    }
    // publish a stream of accepted updates while the consumer drains them
    const COUNT: u64 = 64;
    for hub_revision in 2..=COUNT + 1 {
        let mut next = snapshot.clone();
        next.revision = hub_revision;
        updates
            .send(HubPublication::Update(Box::new(accepted_update(
                hub_revision,
                next,
            ))))
            .unwrap();
    }
    let mut previous = 1_u64;
    for _ in 0..COUNT {
        match read_line(&mut reader).await {
            HubStreamEnvelope::Update { hub_revision, .. } => {
                assert_eq!(hub_revision, previous + 1, "update gap detected");
                previous = hub_revision;
            }
            other => panic!("expected update, got {other:?}"),
        }
    }
    assert_eq!(previous, COUNT + 1);
    drop(reader);
    task.abort();
}

#[tokio::test]
async fn listener_connection_accepts_only_one_request() {
    let store = Arc::new(HubStore::memory().unwrap());
    let (updates, _sender) = broadcast::channel(BROADCAST_CAPACITY);
    let (mut stream, task) = connect_listener(store, &updates).await;
    send_listen(&mut stream).await;
    send_listen(&mut stream).await;
    assert!(task.await.unwrap().is_err());
}
