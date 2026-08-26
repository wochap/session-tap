use crate::domain::{
    ActiveAttention, AttentionContext, FailureContext, InvocationId, InvocationSnapshot,
    LiveEventMetadata, NormalizedEvent,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

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
