use crate::domain::{
    ActiveAttention, AttentionContext, EventKind, FailureContext, InvocationId, InvocationSnapshot,
    LiveEventMetadata, NormalizedEvent,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const HUB_SCHEMA_VERSION: u32 = 1;

/// Capability metadata advertised by a source. Reserved for a future
/// separately specified bidirectional transport; the initial hub performs no
/// control operations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SourceCapabilities {
    #[serde(default)]
    pub capture: bool,
    #[serde(default)]
    pub send_input: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceIdentity {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default)]
    pub capabilities: SourceCapabilities,
}

/// Event metadata carried by a canonical hub update.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HubEventMetadata {
    pub kind: EventKind,
    pub observed_at: DateTime<Utc>,
    pub received_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<FailureContext>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
}

/// Canonical versioned envelope delivered by SessionTap hub sinks.
///
/// `Update.attention` is always serialized: `None` is an explicit null that
/// tells receivers to clear previously retained attention.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HubEnvelope {
    Snapshot {
        schema_version: u32,
        source: SourceIdentity,
        revision: u64,
        invocations: Vec<InvocationSnapshot>,
        #[serde(default)]
        active_attention: BTreeMap<InvocationId, ActiveAttention>,
    },
    Update {
        schema_version: u32,
        source_id: String,
        event_id: String,
        revision: u64,
        event: HubEventMetadata,
        snapshot: Box<InvocationSnapshot>,
        attention: Option<ActiveAttention>,
    },
}

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
        attention: Option<AttentionContext>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        failure: Option<FailureContext>,
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
        invocations: Vec<InvocationSnapshot>,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEnvelope {
    Snapshot {
        schema_version: u32,
        revision: u64,
        invocations: Vec<InvocationSnapshot>,
        #[serde(default)]
        active_attention: BTreeMap<InvocationId, ActiveAttention>,
    },
    Update {
        schema_version: u32,
        revision: u64,
        snapshot: Box<InvocationSnapshot>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        event: Option<LiveEventMetadata>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SinkEvent {
    pub schema_version: u32,
    pub event_id: String,
    pub revision: u64,
    pub snapshot: InvocationSnapshot,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{
        Activity, AttentionSource, Capabilities, Lifecycle, ProcessMetadata, PublicStatus,
    };

    fn fixture_snapshot() -> InvocationSnapshot {
        let now = Utc::now();
        InvocationSnapshot {
            schema_version: 1,
            revision: 7,
            invocation_id: InvocationId::new(),
            provider: "claude".into(),
            executable: "claude".into(),
            args: vec![],
            cwd: "/tmp".into(),
            process: ProcessMetadata::default(),
            created_at: now,
            updated_at: now,
            lifecycle: Lifecycle::Alive,
            activity: Activity::WaitingInput,
            status: PublicStatus::Blocked,
            provider_session: None,
            provider_metadata: None,
            usage: None,
            repository: None,
            multiplexer: None,
            capabilities: Capabilities::default(),
            turn_generation: 0,
            completed_generation: None,
        }
    }

    fn fixture_update(attention: Option<ActiveAttention>) -> HubEnvelope {
        let now = Utc::now();
        HubEnvelope::Update {
            schema_version: HUB_SCHEMA_VERSION,
            source_id: "host".into(),
            event_id: "event-1".into(),
            revision: 7,
            event: HubEventMetadata {
                kind: EventKind::WaitingInput,
                observed_at: now,
                received_at: now,
                failure: None,
                turn_id: None,
            },
            snapshot: Box::new(fixture_snapshot()),
            attention,
        }
    }

    #[test]
    fn hub_update_is_tagged_versioned_and_source_identified() {
        let update = fixture_update(None);
        let value = serde_json::to_value(&update).unwrap();
        assert_eq!(value["type"], "update");
        assert_eq!(value["schema_version"], HUB_SCHEMA_VERSION);
        assert_eq!(value["source_id"], "host");
        assert_eq!(value["event"]["kind"], "waiting_input");
        let decoded: HubEnvelope = serde_json::from_value(value).unwrap();
        assert_eq!(decoded, update);
    }

    #[test]
    fn hub_update_serializes_cleared_attention_as_explicit_null() {
        let value = serde_json::to_value(fixture_update(None)).unwrap();
        assert!(value.as_object().unwrap().contains_key("attention"));
        assert!(value["attention"].is_null());
    }

    #[test]
    fn hub_update_carries_attention_object_and_failure_category() {
        let attention = ActiveAttention {
            kind: EventKind::WaitingInput,
            context: AttentionContext {
                summary: "Choose an option".into(),
                source: AttentionSource::Question,
            },
        };
        let mut update = fixture_update(Some(attention.clone()));
        if let HubEnvelope::Update { event, .. } = &mut update {
            event.failure = Some(FailureContext::RateLimited);
        }
        let value = serde_json::to_value(&update).unwrap();
        assert_eq!(value["attention"]["kind"], "waiting_input");
        assert_eq!(value["attention"]["context"]["source"], "question");
        assert_eq!(value["event"]["failure"], "rate_limited");
        let decoded: HubEnvelope = serde_json::from_value(value).unwrap();
        assert_eq!(decoded, update);
    }

    #[test]
    fn hub_snapshot_carries_source_identity_and_attention_map() {
        let snapshot = fixture_snapshot();
        let attention = ActiveAttention {
            kind: EventKind::WaitingApproval,
            context: AttentionContext {
                summary: "Run tests".into(),
                source: AttentionSource::ToolSummary,
            },
        };
        let envelope = HubEnvelope::Snapshot {
            schema_version: HUB_SCHEMA_VERSION,
            source: SourceIdentity {
                id: "sandbox".into(),
                display_name: Some("NixOS sandbox".into()),
                capabilities: SourceCapabilities::default(),
            },
            revision: 12,
            invocations: vec![snapshot.clone()],
            active_attention: BTreeMap::from([(snapshot.invocation_id.clone(), attention)]),
        };
        let value = serde_json::to_value(&envelope).unwrap();
        assert_eq!(value["type"], "snapshot");
        assert_eq!(value["source"]["id"], "sandbox");
        assert_eq!(value["source"]["display_name"], "NixOS sandbox");
        assert_eq!(value["invocations"][0]["revision"], 7);
        let decoded: HubEnvelope = serde_json::from_value(value).unwrap();
        assert_eq!(decoded, envelope);
    }

    #[test]
    fn hub_envelope_rejects_unknown_or_future_shapes() {
        assert!(
            serde_json::from_value::<HubEnvelope>(serde_json::json!({"type":"update"})).is_err()
        );
        assert!(
            serde_json::from_value::<HubEnvelope>(serde_json::json!({"type":"command"})).is_err()
        );
        let mut value = serde_json::to_value(fixture_update(None)).unwrap();
        value["schema_version"] = serde_json::json!(99);
        let decoded: HubEnvelope = serde_json::from_value(value).unwrap();
        match decoded {
            HubEnvelope::Update { schema_version, .. } => assert_eq!(schema_version, 99),
            _ => panic!("expected update"),
        }
    }

    #[test]
    fn request_envelope_is_tagged_and_backward_tolerant() {
        let value = serde_json::to_value(Request::Health).unwrap();
        assert_eq!(value["type"], "health");
        let decoded: Request =
            serde_json::from_value(serde_json::json!({"type":"health"})).unwrap();
        assert!(matches!(decoded, Request::Health));
    }
    #[test]
    fn error_schema_is_stable() {
        let response = Response::Error(ErrorEnvelope {
            code: "bad_request".into(),
            message: "invalid".into(),
        });
        assert_eq!(serde_json::to_value(response).unwrap()["type"], "error");
    }
    #[test]
    fn old_envelopes_remain_readable() {
        let now = chrono::Utc::now();
        let snapshot = crate::domain::InvocationSnapshot {
            schema_version: 1,
            revision: 1,
            invocation_id: InvocationId::new(),
            provider: "test".into(),
            executable: "test".into(),
            args: vec![],
            cwd: "/tmp".into(),
            process: Default::default(),
            created_at: now,
            updated_at: now,
            lifecycle: crate::domain::Lifecycle::Alive,
            activity: crate::domain::Activity::Idle,
            status: crate::domain::PublicStatus::Idle,
            provider_session: None,
            provider_metadata: None,
            usage: None,
            repository: None,
            multiplexer: None,
            capabilities: Default::default(),
            turn_generation: 0,
            completed_generation: None,
        };
        let old = serde_json::json!({"type":"update","schema_version":1,"revision":1,"snapshot":snapshot});
        assert!(matches!(
            serde_json::from_value::<StreamEnvelope>(old).unwrap(),
            StreamEnvelope::Update { event: None, .. }
        ));
        let old =
            serde_json::json!({"type":"snapshot","schema_version":1,"revision":1,"invocations":[]});
        assert!(
            matches!(serde_json::from_value::<StreamEnvelope>(old).unwrap(), StreamEnvelope::Snapshot { active_attention, .. } if active_attention.is_empty())
        );
    }
}
