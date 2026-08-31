use crate::domain::{
    ArtifactCollectionContext, InvocationId, InvocationSnapshot, NormalizedEvent, PublicAgentView,
    PublicField, StatusReasonContext, StatuslineObservation,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

pub const HUB_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceIdentity {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
}

/// Canonical public source envelope delivered by every sink and accepted by
/// the hub. Unknown JSON fields are ignored while recognized fields remain
/// strongly typed and required where appropriate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SourceEnvelope {
    Snapshot {
        schema_version: u32,
        source: SourceIdentity,
        revision: u64,
        views: Vec<PublicAgentView>,
    },
    Update {
        schema_version: u32,
        source_id: String,
        delivery_id: String,
        revision: u64,
        changed: BTreeSet<PublicField>,
        view: Box<PublicAgentView>,
    },
}

/// Private daemon request protocol. Internal snapshots and events occur only
/// on this local authenticated control path.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    Health,
    Register {
        snapshot: Box<InvocationSnapshot>,
        credential: String,
    },
    BindChild {
        invocation_id: InvocationId,
        credential: String,
        child_pid: u32,
        start_identity: Option<String>,
    },
    LifecycleExit {
        invocation_id: InvocationId,
        credential: String,
        exit_code: Option<i32>,
        signal: Option<i32>,
    },
    HookIngest {
        provider: String,
        invocation_id: InvocationId,
        credential: String,
        event: Box<NormalizedEvent>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status_reason: Option<StatusReasonContext>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        collection_context: Option<ArtifactCollectionContext>,
    },
    StatuslineIngest {
        provider: String,
        invocation_id: InvocationId,
        credential: String,
        observation: StatuslineObservation,
    },
    Status,
    Listen,
    Capture {
        invocation_id: InvocationId,
    },
    SendInput {
        invocation_id: InvocationId,
        text: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    Ok,
    Health {
        version: u32,
    },
    Status {
        revision: u64,
        views: Vec<PublicAgentView>,
    },
    Captured {
        text: String,
    },
    Error(ErrorEnvelope),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorEnvelope {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEnvelope {
    Snapshot {
        schema_version: u32,
        revision: u64,
        views: Vec<PublicAgentView>,
    },
    Update {
        schema_version: u32,
        revision: u64,
        delivery_id: String,
        changed: BTreeSet<PublicField>,
        view: Box<PublicAgentView>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{
        ProviderMetadata, PublicProviderSession, PublicReasonKind, PublicStatus,
        PublicStatusReason, Repository, Usage,
    };
    use chrono::{TimeZone, Utc};

    fn view() -> PublicAgentView {
        let at = Utc.with_ymd_and_hms(2026, 8, 28, 12, 0, 0).unwrap();
        PublicAgentView {
            invocation_id: "00000000-0000-4000-8000-000000000001".parse().unwrap(),
            provider: "company-claude".into(),
            status: PublicStatus::Running,
            reason: None,
            cwd: "/work/project".into(),
            created_at: at,
            updated_at: at,
            session: None,
            metadata: None,
            usage: None,
            repository: Some(Repository {
                root: "/work/project".into(),
                branch: Some("main".into()),
                head: None,
                dirty: Some(false),
            }),
        }
    }

    #[test]
    fn canonical_update_has_only_public_fields() {
        let envelope = SourceEnvelope::Update {
            schema_version: HUB_SCHEMA_VERSION,
            source_id: "sandbox".into(),
            delivery_id: "delivery-1".into(),
            revision: 7,
            changed: BTreeSet::from([PublicField::Status, PublicField::Repository]),
            view: Box::new(view()),
        };
        let json = serde_json::to_string(&envelope).unwrap();
        for private in [
            "process",
            "multiplexer",
            "activity",
            "lifecycle",
            "event_kind",
            "credential",
            "args",
            "transcript_path",
            "collection_context",
            "byte_offset",
            "response_ids",
        ] {
            assert!(!json.contains(private));
        }
        assert_eq!(
            serde_json::from_str::<SourceEnvelope>(&json).unwrap(),
            envelope
        );
    }

    #[test]
    fn malformed_is_rejected_and_unknown_input_is_discarded() {
        assert!(
            serde_json::from_value::<SourceEnvelope>(serde_json::json!({"type":"update"})).is_err()
        );
        let mut value = serde_json::to_value(SourceEnvelope::Snapshot {
            schema_version: 1,
            source: SourceIdentity {
                id: "sandbox".into(),
                display_name: None,
            },
            revision: 1,
            views: vec![view()],
        })
        .unwrap();
        value["private_process"] = serde_json::json!({"pid": 42});
        let decoded: SourceEnvelope = serde_json::from_value(value).unwrap();
        assert!(
            !serde_json::to_string(&decoded)
                .unwrap()
                .contains("private_process")
        );
    }

    #[test]
    fn shared_local_and_source_updates_match_golden_json() {
        let base = view();
        let local_snapshot = StreamEnvelope::Snapshot {
            schema_version: 1,
            revision: 9,
            views: vec![base.clone()],
        };
        let source_snapshot = SourceEnvelope::Snapshot {
            schema_version: 1,
            source: SourceIdentity {
                id: "sandbox".into(),
                display_name: Some("Sandbox".into()),
            },
            revision: 9,
            views: vec![base],
        };
        let local_snapshot_golden: serde_json::Value =
            serde_json::from_str(include_str!("../tests/golden/public-local-snapshot.json"))
                .unwrap();
        let source_snapshot_golden: serde_json::Value =
            serde_json::from_str(include_str!("../tests/golden/public-source-snapshot.json"))
                .unwrap();
        assert_eq!(
            serde_json::to_value(local_snapshot).unwrap(),
            local_snapshot_golden
        );
        assert_eq!(
            serde_json::to_value(source_snapshot).unwrap(),
            source_snapshot_golden
        );

        let mut rich = view();
        rich.status = PublicStatus::Stopped;
        rich.reason = Some(PublicStatusReason {
            kind: PublicReasonKind::Completed,
            summary: "All tests pass".into(),
        });
        rich.updated_at = Utc.with_ymd_and_hms(2026, 8, 28, 12, 1, 0).unwrap();
        rich.session = Some(PublicProviderSession {
            id: "session-2".into(),
            name: Some("Refactor".into()),
            start_reason: Some("resume".into()),
        });
        rich.metadata = Some(ProviderMetadata {
            model: Some("claude-opus".into()),
            effort: Some("high".into()),
            permission_mode: None,
            current_turn_id: None,
        });
        rich.usage = Some(Usage {
            input_tokens: Some(100),
            output_tokens: Some(20),
            context_tokens: Some(120),
            context_window_percent: Some(40),
        });
        let changed = BTreeSet::from([
            PublicField::Status,
            PublicField::Reason,
            PublicField::Session,
            PublicField::Usage,
        ]);
        let local = StreamEnvelope::Update {
            schema_version: 1,
            revision: 9,
            delivery_id: "delivery-9".into(),
            changed: changed.clone(),
            view: Box::new(rich.clone()),
        };
        let source = SourceEnvelope::Update {
            schema_version: 1,
            source_id: "sandbox".into(),
            delivery_id: "delivery-9".into(),
            revision: 9,
            changed,
            view: Box::new(rich),
        };
        let local_golden: serde_json::Value =
            serde_json::from_str(include_str!("../tests/golden/public-local-update.json")).unwrap();
        let source_golden: serde_json::Value =
            serde_json::from_str(include_str!("../tests/golden/public-source-update.json"))
                .unwrap();
        assert_eq!(serde_json::to_value(local).unwrap(), local_golden);
        assert_eq!(serde_json::to_value(source).unwrap(), source_golden);
    }
}
