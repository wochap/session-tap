use chrono::Utc;
use sessiontap_core::{
    domain::{InvocationId, PublicAgentView, PublicField, PublicStatus},
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
