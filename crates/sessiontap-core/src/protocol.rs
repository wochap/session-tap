use crate::domain::{
    ArtifactCollectionContext, InvocationId, InvocationSnapshot, NormalizedEvent, PublicAgentView,
    PublicField, StatusReasonContext,
};
pub use crate::terminal::TerminalFrame;
use crate::terminal::TerminalInput;
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
    Status,
    Listen,
    Capture {
        invocation_id: InvocationId,
    },
    /// Streaming request answered with [`TerminalFrame`] lines, or one
    /// error response when the terminal is unavailable.
    TerminalWatch {
        invocation_id: InvocationId,
    },
    TerminalInput {
        invocation_id: InvocationId,
        input: TerminalInput,
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

/// Version of the hub control channel protocol sent in `hello`.
pub const RELAY_PROTOCOL_VERSION: u32 = 1;

/// Hub-to-source messages on the control channel. `req` correlates an
/// answer; `stream` is the hub-assigned terminal stream ID.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RelayRequest {
    Open {
        req: u64,
        stream: u64,
        invocation_id: String,
    },
    Input {
        req: u64,
        stream: u64,
        input: TerminalInput,
    },
    Close {
        stream: u64,
    },
    /// The hub dropped frames for the stream; send a fresh snapshot.
    Resync {
        stream: u64,
    },
}

/// Source-to-hub messages on the control channel. `hello` is first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RelayMessage {
    Hello {
        source_id: String,
        protocol: u32,
    },
    Opened {
        req: u64,
        stream: u64,
    },
    Error {
        req: u64,
        code: String,
        #[serde(default)]
        message: String,
    },
    /// Input answer: success without `code`, refusal with it.
    InputResult {
        req: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        code: Option<String>,
    },
    Frame {
        stream: u64,
        frame: TerminalFrame,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{
        ChildActivity, ChildAgentState, InvocationSnapshot, ProviderMetadata,
        PublicChildAgentReason, PublicProviderSession, PublicReasonKind, PublicStatus,
        PublicStatusReason, Repository, Usage, project_public,
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
            children: None,
            terminal: None,
        }
    }

    #[test]
    fn relay_envelopes_round_trip() {
        use crate::terminal::{EndReason, Key, NamedKey};
        use serde_json::json;
        fn check<T>(value: &T, expected: serde_json::Value)
        where
            T: Serialize + for<'de> Deserialize<'de> + PartialEq + std::fmt::Debug,
        {
            assert_eq!(serde_json::to_value(value).unwrap(), expected);
            assert_eq!(&serde_json::from_value::<T>(expected).unwrap(), value);
        }
        check(
            &RelayMessage::Hello {
                source_id: "host".into(),
                protocol: RELAY_PROTOCOL_VERSION,
            },
            json!({"type": "hello", "source_id": "host", "protocol": 1}),
        );
        check(
            &RelayRequest::Open {
                req: 1,
                stream: 7,
                invocation_id: "inv".into(),
            },
            json!({"type": "open", "req": 1, "stream": 7, "invocation_id": "inv"}),
        );
        check(
            &RelayMessage::Opened { req: 1, stream: 7 },
            json!({"type": "opened", "req": 1, "stream": 7}),
        );
        check(
            &RelayMessage::Error {
                req: 1,
                code: "source_disallows_control".into(),
                message: "off".into(),
            },
            json!({"type": "error", "req": 1, "code": "source_disallows_control", "message": "off"}),
        );
        check(
            &RelayRequest::Input {
                req: 2,
                stream: 7,
                input: TerminalInput::Keys(vec![Key::named(NamedKey::Enter), Key::char('1')]),
            },
            json!({"type": "input", "req": 2, "stream": 7, "input": {"keys": ["enter", "1"]}}),
        );
        check(
            &RelayMessage::InputResult { req: 2, code: None },
            json!({"type": "input_result", "req": 2}),
        );
        check(
            &RelayMessage::InputResult {
                req: 3,
                code: Some("not_foreground".into()),
            },
            json!({"type": "input_result", "req": 3, "code": "not_foreground"}),
        );
        check(
            &RelayRequest::Close { stream: 7 },
            json!({"type": "close", "stream": 7}),
        );
        check(
            &RelayRequest::Resync { stream: 7 },
            json!({"type": "resync", "stream": 7}),
        );
        let bytes: Vec<u8> = (0..=255).collect();
        let frame = RelayMessage::Frame {
            stream: 7,
            frame: TerminalFrame::Output {
                seq: 3,
                data: bytes.clone(),
            },
        };
        let text = serde_json::to_string(&frame).unwrap();
        match serde_json::from_str::<RelayMessage>(&text).unwrap() {
            RelayMessage::Frame {
                frame: TerminalFrame::Output { data, .. },
                ..
            } => assert_eq!(data, bytes),
            other => panic!("{other:?}"),
        }
        check(
            &RelayMessage::Frame {
                stream: 7,
                frame: TerminalFrame::Ended {
                    reason: EndReason::SourceDisallowsControl,
                },
            },
            json!({"type": "frame", "stream": 7, "frame": {"type": "ended", "reason": "source_disallows_control"}}),
        );
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

    #[test]
    fn child_agent_update_matches_golden_json() {
        let at = |minute, second| {
            Utc.with_ymd_and_hms(2026, 8, 28, 12, minute, second)
                .unwrap()
        };
        let child = |id: &str, kind: &str, activity, reason, started, updated| ChildAgentState {
            agent_id: id.into(),
            agent_type: kind.into(),
            activity,
            reason: Some(reason),
            started_at: started,
            updated_at: updated,
        };
        let mut snapshot: InvocationSnapshot =
            serde_json::from_str(include_str!("../tests/golden/pre-enum-tmux-snapshot.json"))
                .unwrap();
        // Stored out of order; the projection sorts by start time then ID.
        snapshot.children = vec![
            child(
                "agent-c",
                "reviewer",
                ChildActivity::Stopped,
                PublicChildAgentReason {
                    kind: Some(PublicReasonKind::Completed),
                    summary: None,
                },
                at(0, 20),
                at(1, 50),
            ),
            child(
                "agent-b",
                "general-purpose",
                ChildActivity::Running,
                PublicChildAgentReason {
                    kind: None,
                    summary: Some("read_file".into()),
                },
                at(0, 10),
                at(1, 30),
            ),
            child(
                "agent-a",
                "Explore",
                ChildActivity::Blocked,
                PublicChildAgentReason {
                    kind: Some(PublicReasonKind::Approval),
                    summary: Some("shell".into()),
                },
                at(0, 10),
                at(2, 0),
            ),
        ];
        let mut rich = view();
        rich.status = PublicStatus::Stopped;
        rich.reason = Some(PublicStatusReason {
            kind: PublicReasonKind::Completed,
            summary: "All tests pass".into(),
        });
        rich.updated_at = at(2, 0);
        rich.children = project_public(&snapshot, None).children;
        let local = StreamEnvelope::Update {
            schema_version: 1,
            revision: 10,
            delivery_id: "delivery-10".into(),
            changed: BTreeSet::from([PublicField::Children]),
            view: Box::new(rich),
        };
        let golden: serde_json::Value = serde_json::from_str(include_str!(
            "../tests/golden/public-local-update-children.json"
        ))
        .unwrap();
        assert_eq!(serde_json::to_value(&local).unwrap(), golden);
        let json = serde_json::to_string(&local).unwrap();
        for private in ["correlation_id", "detail", "transcript", "activity"] {
            assert!(!json.contains(private), "{private}");
        }
        assert_eq!(
            serde_json::from_value::<StreamEnvelope>(golden).unwrap(),
            local
        );
    }
}
