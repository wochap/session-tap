use chrono::Utc;
use sessiontap_core::{
    domain::{InvocationId, PublicAgentView, PublicField, PublicStatus},
    protocol::{SourceEnvelope, SourceIdentity},
};
use sessiontap_hub::{
    ingest::{AcceptedUpdate, HubPublication},
    listen::{HubRequest, HubStreamEnvelope, serve_listener},
    store::HubStore,
};
use std::{collections::BTreeSet, sync::Arc};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    sync::broadcast,
};

fn view(status: PublicStatus) -> PublicAgentView {
    PublicAgentView {
        invocation_id: InvocationId::new(),
        provider: "codex".into(),
        status,
        reason: None,
        cwd: "/tmp".into(),
        created_at: Utc::now(),
        updated_at: Utc::now(),
        session: None,
        metadata: None,
        usage: None,
        repository: None,
    }
}

#[tokio::test]
async fn listener_gets_baseline_then_complete_public_update() {
    let store = Arc::new(HubStore::memory().unwrap());
    let idle = view(PublicStatus::Idle);
    store
        .ingest_snapshot(&SourceEnvelope::Snapshot {
            schema_version: 1,
            source: SourceIdentity {
                id: "sandbox".into(),
                display_name: None,
            },
            revision: 1,
            views: vec![idle.clone()],
        })
        .unwrap();
    let (sender, receiver) = broadcast::channel(8);
    let (client, server) = UnixStream::pair().unwrap();
    let task = tokio::spawn(serve_listener(server, store, receiver));
    let (read, mut write) = client.into_split();
    write
        .write_all(&serde_json::to_vec(&HubRequest::Listen).unwrap())
        .await
        .unwrap();
    write.write_all(b"\n").await.unwrap();
    let mut lines = BufReader::new(read).lines();
    let baseline: HubStreamEnvelope =
        serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
    assert!(matches!(baseline, HubStreamEnvelope::Snapshot { agents, .. } if agents.len() == 1));

    let mut running = idle;
    running.status = PublicStatus::Running;
    sender
        .send(HubPublication::Update(Box::new(AcceptedUpdate {
            hub_revision: 2,
            source_id: "sandbox".into(),
            delivery_id: "d1".into(),
            source_revision: 2,
            view: running,
            changed: BTreeSet::from([PublicField::Status]),
            first_seen: false,
        })))
        .unwrap();
    let update: HubStreamEnvelope =
        serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
    assert!(
        matches!(update, HubStreamEnvelope::Update { view, changed, .. } if view.status == PublicStatus::Running && changed.contains(&PublicField::Status))
    );
    drop(write);
    task.await.unwrap().unwrap();
}
