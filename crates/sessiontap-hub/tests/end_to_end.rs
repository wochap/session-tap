use chrono::Utc;
use sessiontap_core::{
    domain::{InvocationId, PublicAgentView, PublicStatus},
    protocol::{SourceEnvelope, SourceIdentity},
};
use sessiontap_hub::store::HubStore;

fn view(id: InvocationId, provider: &str) -> PublicAgentView {
    PublicAgentView {
        invocation_id: id,
        provider: provider.into(),
        status: PublicStatus::Idle,
        reason: None,
        cwd: "/work".into(),
        created_at: Utc::now(),
        updated_at: Utc::now(),
        session: None,
        metadata: None,
        usage: None,
        repository: None,
    }
}

#[test]
fn same_invocation_id_from_multiple_sources_remains_distinct_and_restores() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("hub.db");
    let id = InvocationId::new();
    {
        let store = HubStore::open(&path).unwrap();
        for (source, provider) in [("machine-a", "codex"), ("machine-b", "claude")] {
            store
                .ingest_snapshot(&SourceEnvelope::Snapshot {
                    schema_version: 1,
                    source: SourceIdentity {
                        id: source.into(),
                        display_name: None,
                    },
                    revision: 1,
                    views: vec![view(id.clone(), provider)],
                })
                .unwrap();
        }
    }
    let store = HubStore::open(&path).unwrap();
    let (_, _, agents) = store.merged().unwrap();
    assert_eq!(agents.len(), 2);
    assert_ne!(agents[0].source_id, agents[1].source_id);
    let serialized = serde_json::to_string(&agents).unwrap();
    for private in [
        "process",
        "multiplexer",
        "credential",
        "raw_hook",
        "activity",
        "lifecycle",
    ] {
        assert!(!serialized.contains(private));
    }
}
