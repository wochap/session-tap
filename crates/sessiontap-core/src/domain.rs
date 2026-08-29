use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct InvocationId(pub Uuid);

impl InvocationId {
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for InvocationId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for InvocationId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::str::FromStr for InvocationId {
    type Err = uuid::Error;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Uuid::parse_str(value).map(Self)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Lifecycle {
    Starting,
    Alive,
    Exited,
    Lost,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Activity {
    Unknown,
    Idle,
    Working,
    WaitingInput,
    WaitingApproval,
    Stopped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PublicStatus {
    Running,
    Idle,
    Blocked,
    Stopped,
}

/// Observer-facing reason category. Internal event kinds and attention source
/// details deliberately do not cross the public projection boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PublicReasonKind {
    Input,
    Approval,
    Completed,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicStatusReason {
    pub kind: PublicReasonKind,
    pub summary: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicProviderSession {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_reason: Option<String>,
}

/// The complete, deliberately selected observer-facing state for one agent.
/// This is constructed field-by-field and is never a serialized internal
/// invocation snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicAgentView {
    pub invocation_id: InvocationId,
    pub provider: String,
    pub status: PublicStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<PublicStatusReason>,
    pub cwd: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<PublicProviderSession>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<ProviderMetadata>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<Repository>,
}

/// Typed public field paths, ordered by declaration for deterministic JSON.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PublicField {
    InvocationId,
    Provider,
    Status,
    Reason,
    Cwd,
    CreatedAt,
    UpdatedAt,
    Session,
    Metadata,
    Usage,
    Repository,
}

#[must_use]
pub const fn derive_status(lifecycle: Lifecycle, activity: Activity) -> PublicStatus {
    match lifecycle {
        Lifecycle::Exited | Lifecycle::Lost => PublicStatus::Stopped,
        Lifecycle::Starting => PublicStatus::Idle,
        Lifecycle::Alive => match activity {
            Activity::WaitingInput | Activity::WaitingApproval => PublicStatus::Blocked,
            Activity::Stopped => PublicStatus::Stopped,
            Activity::Working => PublicStatus::Running,
            Activity::Unknown | Activity::Idle => PublicStatus::Idle,
        },
    }
}

#[must_use]
pub fn project_public(
    snapshot: &InvocationSnapshot,
    current_reason: Option<&CurrentStatusReason>,
) -> PublicAgentView {
    let status = derive_status(snapshot.lifecycle, snapshot.activity);
    let reason = current_reason.and_then(|reason| {
        let kind = match (status, &reason.kind) {
            (PublicStatus::Blocked, EventKind::WaitingInput) => PublicReasonKind::Input,
            (PublicStatus::Blocked, EventKind::WaitingApproval) => PublicReasonKind::Approval,
            (PublicStatus::Stopped, EventKind::Completed) => PublicReasonKind::Completed,
            (PublicStatus::Stopped, EventKind::Failed) => PublicReasonKind::Failed,
            _ => return None,
        };
        Some(PublicStatusReason {
            kind,
            summary: reason.context.summary.clone(),
        })
    });
    PublicAgentView {
        invocation_id: snapshot.invocation_id.clone(),
        provider: snapshot.provider.clone(),
        status,
        reason,
        cwd: snapshot.cwd.clone(),
        created_at: snapshot.created_at,
        updated_at: snapshot.updated_at,
        session: snapshot
            .provider_session
            .as_ref()
            .map(|session| PublicProviderSession {
                id: session.id.clone(),
                name: session.name.clone(),
                start_reason: session.start_reason.clone(),
            }),
        metadata: snapshot.provider_metadata.clone(),
        usage: snapshot.usage.clone(),
        repository: snapshot.repository.clone(),
    }
}

#[must_use]
pub fn changed_public_fields(
    previous: Option<&PublicAgentView>,
    current: &PublicAgentView,
) -> BTreeSet<PublicField> {
    let Some(previous) = previous else {
        return BTreeSet::from([
            PublicField::InvocationId,
            PublicField::Provider,
            PublicField::Status,
            PublicField::Reason,
            PublicField::Cwd,
            PublicField::CreatedAt,
            PublicField::UpdatedAt,
            PublicField::Session,
            PublicField::Metadata,
            PublicField::Usage,
            PublicField::Repository,
        ]);
    };
    let mut changed = BTreeSet::new();
    macro_rules! field {
        ($name:ident, $variant:ident) => {
            if previous.$name != current.$name {
                changed.insert(PublicField::$variant);
            }
        };
    }
    field!(invocation_id, InvocationId);
    field!(provider, Provider);
    field!(status, Status);
    field!(reason, Reason);
    field!(cwd, Cwd);
    field!(created_at, CreatedAt);
    field!(updated_at, UpdatedAt);
    field!(session, Session);
    field!(metadata, Metadata);
    field!(usage, Usage);
    field!(repository, Repository);
    changed
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ProviderSession {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub generation: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ProviderMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_turn_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: Option<u64>,
    #[serde(default)]
    pub output_tokens: Option<u64>,
    #[serde(default)]
    pub context_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window_percent: Option<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Repository {
    pub root: String,
    pub branch: Option<String>,
    pub head: Option<String>,
    pub dirty: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ProcessMetadata {
    pub wrapper_pid: u32,
    pub child_pid: Option<u32>,
    pub start_identity: Option<String>,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct MultiplexerMetadata {
    pub backend: String,
    pub socket: String,
    pub server_pid: Option<u32>,
    pub session_id: Option<String>,
    pub session_name: Option<String>,
    pub window_id: Option<String>,
    pub window_index: Option<u32>,
    pub pane_id: String,
    pub pane_tty: Option<String>,
    pub pane_pid: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Capabilities {
    pub capture: bool,
    pub send_input: bool,
    pub usage: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InvocationSnapshot {
    pub schema_version: u32,
    pub revision: u64,
    pub invocation_id: InvocationId,
    pub provider: String,
    pub executable: String,
    pub args: Vec<String>,
    pub cwd: String,
    pub process: ProcessMetadata,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub lifecycle: Lifecycle,
    pub activity: Activity,
    pub status: PublicStatus,
    pub provider_session: Option<ProviderSession>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_metadata: Option<ProviderMetadata>,
    pub usage: Option<Usage>,
    pub repository: Option<Repository>,
    pub multiplexer: Option<MultiplexerMetadata>,
    pub capabilities: Capabilities,
    #[serde(skip)]
    pub turn_generation: u64,
    #[serde(skip)]
    pub completed_generation: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    NewTurn,
    Working,
    Idle,
    WaitingInput,
    WaitingApproval,
    Completed,
    Failed,
    ProviderSessionStarted,
    ProviderSessionEnded,
    SessionEnded,
    Enrichment,
}

pub const STATUS_EXCERPT_MAX_CHARS: usize = 100;
pub const STATUS_REASON_MAX_CHARS: usize = 128;
pub const STATUS_REASON_MAX_BYTES: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatusReasonSource {
    Description,
    ToolSummary,
    Command,
    Question,
    ToolName,
    GenericInput,
    AssistantMessage,
    FailureCategory,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusReasonContext {
    pub summary: String,
    pub source: StatusReasonSource,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CurrentStatusReason {
    pub kind: EventKind,
    pub context: StatusReasonContext,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveEventMetadata {
    pub kind: EventKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_reason: Option<StatusReasonContext>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NormalizedAdapterEvent {
    pub event: NormalizedEvent,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_reason: Option<StatusReasonContext>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum AdapterOutcome {
    Event(Box<NormalizedAdapterEvent>),
    Ignored,
}

impl AdapterOutcome {
    #[must_use]
    pub fn into_event(self) -> Option<NormalizedAdapterEvent> {
        match self {
            Self::Event(event) => Some(*event),
            Self::Ignored => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NormalizedEvent {
    pub schema_version: u32,
    pub event_id: String,
    pub invocation_id: InvocationId,
    pub provider_event_id: Option<String>,
    pub provider: String,
    pub observed_at: DateTime<Utc>,
    pub received_at: DateTime<Utc>,
    pub source: String,
    pub kind: EventKind,
    pub provider_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_session_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_session_start_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_metadata: Option<ProviderMetadata>,
    pub usage: Option<Usage>,
    pub turn_id: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_precedence_is_exhaustive() {
        for activity in [
            Activity::Unknown,
            Activity::Idle,
            Activity::Working,
            Activity::WaitingInput,
            Activity::WaitingApproval,
            Activity::Stopped,
        ] {
            assert_eq!(
                derive_status(Lifecycle::Exited, activity),
                PublicStatus::Stopped
            );
            assert_eq!(
                derive_status(Lifecycle::Lost, activity),
                PublicStatus::Stopped
            );
        }
        assert_eq!(
            derive_status(Lifecycle::Alive, Activity::WaitingInput),
            PublicStatus::Blocked
        );
        assert_eq!(
            derive_status(Lifecycle::Alive, Activity::WaitingApproval),
            PublicStatus::Blocked
        );
        assert_eq!(
            derive_status(Lifecycle::Alive, Activity::Working),
            PublicStatus::Running
        );
        assert_eq!(
            derive_status(Lifecycle::Alive, Activity::Idle),
            PublicStatus::Idle
        );
        assert_eq!(
            derive_status(Lifecycle::Alive, Activity::Unknown),
            PublicStatus::Idle
        );
        assert_eq!(
            derive_status(Lifecycle::Alive, Activity::Stopped),
            PublicStatus::Stopped
        );
    }

    #[test]
    fn public_reason_projection_requires_a_compatible_status_and_kind() {
        let now = Utc::now();
        let mut snapshot = InvocationSnapshot {
            schema_version: 1,
            revision: 1,
            invocation_id: InvocationId::new(),
            provider: "fixture".into(),
            executable: "fixture".into(),
            args: vec![],
            cwd: "/fixture".into(),
            process: ProcessMetadata::default(),
            created_at: now,
            updated_at: now,
            lifecycle: Lifecycle::Alive,
            activity: Activity::Stopped,
            status: PublicStatus::Stopped,
            provider_session: None,
            provider_metadata: None,
            usage: None,
            repository: None,
            multiplexer: None,
            capabilities: Capabilities::default(),
            turn_generation: 1,
            completed_generation: Some(1),
        };
        let completed = CurrentStatusReason {
            kind: EventKind::Completed,
            context: StatusReasonContext {
                summary: "Done".into(),
                source: StatusReasonSource::AssistantMessage,
            },
        };
        assert_eq!(
            project_public(&snapshot, Some(&completed))
                .reason
                .unwrap()
                .kind,
            PublicReasonKind::Completed
        );
        let blocked = CurrentStatusReason {
            kind: EventKind::WaitingInput,
            context: StatusReasonContext {
                summary: "Choose".into(),
                source: StatusReasonSource::Question,
            },
        };
        assert!(project_public(&snapshot, Some(&blocked)).reason.is_none());
        snapshot.activity = Activity::WaitingInput;
        assert_eq!(
            project_public(&snapshot, Some(&blocked))
                .reason
                .unwrap()
                .kind,
            PublicReasonKind::Input
        );
        assert!(project_public(&snapshot, Some(&completed)).reason.is_none());

        let approval = CurrentStatusReason {
            kind: EventKind::WaitingApproval,
            context: StatusReasonContext {
                summary: "Approve".into(),
                source: StatusReasonSource::Description,
            },
        };
        snapshot.activity = Activity::WaitingApproval;
        assert_eq!(
            project_public(&snapshot, Some(&approval))
                .reason
                .unwrap()
                .kind,
            PublicReasonKind::Approval
        );
        let failed = CurrentStatusReason {
            kind: EventKind::Failed,
            context: StatusReasonContext {
                summary: "Timed out".into(),
                source: StatusReasonSource::FailureCategory,
            },
        };
        snapshot.activity = Activity::Stopped;
        assert_eq!(
            project_public(&snapshot, Some(&failed))
                .reason
                .unwrap()
                .kind,
            PublicReasonKind::Failed
        );
    }

    #[test]
    fn changed_fields_cover_blocked_stopped_failed_idle_and_lifecycle_stop() {
        let now = Utc::now();
        let base = PublicAgentView {
            invocation_id: InvocationId::new(),
            provider: "fixture".into(),
            status: PublicStatus::Idle,
            reason: None,
            cwd: "/fixture".into(),
            created_at: now,
            updated_at: now,
            session: None,
            metadata: None,
            usage: None,
            repository: None,
        };
        let mut blocked = base.clone();
        blocked.status = PublicStatus::Blocked;
        blocked.reason = Some(PublicStatusReason {
            kind: PublicReasonKind::Input,
            summary: "Choose".into(),
        });
        assert_eq!(
            changed_public_fields(Some(&base), &blocked),
            BTreeSet::from([PublicField::Status, PublicField::Reason])
        );

        let mut completed = blocked.clone();
        completed.status = PublicStatus::Stopped;
        completed.reason = Some(PublicStatusReason {
            kind: PublicReasonKind::Completed,
            summary: "Done".into(),
        });
        assert_eq!(
            changed_public_fields(Some(&blocked), &completed),
            BTreeSet::from([PublicField::Status, PublicField::Reason])
        );
        let mut failed = completed.clone();
        failed.reason = Some(PublicStatusReason {
            kind: PublicReasonKind::Failed,
            summary: "Timed out".into(),
        });
        assert_eq!(
            changed_public_fields(Some(&completed), &failed),
            BTreeSet::from([PublicField::Reason])
        );
        assert_eq!(
            changed_public_fields(Some(&failed), &base),
            BTreeSet::from([PublicField::Status, PublicField::Reason])
        );
        let mut lifecycle_only = base.clone();
        lifecycle_only.status = PublicStatus::Stopped;
        assert_eq!(
            changed_public_fields(Some(&base), &lifecycle_only),
            BTreeSet::from([PublicField::Status])
        );
    }

    #[test]
    fn public_projection_is_field_selected_and_private_values_are_absent() {
        let now = Utc::now();
        let snapshot = InvocationSnapshot {
            schema_version: 1,
            revision: 1,
            invocation_id: InvocationId::new(),
            provider: "fixture".into(),
            executable: "PRIVATE_EXECUTABLE".into(),
            args: vec!["PRIVATE_ARGUMENT".into()],
            cwd: "/fixture".into(),
            process: ProcessMetadata::default(),
            created_at: now,
            updated_at: now,
            lifecycle: Lifecycle::Alive,
            activity: Activity::Idle,
            status: PublicStatus::Idle,
            provider_session: None,
            provider_metadata: None,
            usage: None,
            repository: None,
            multiplexer: None,
            capabilities: Capabilities::default(),
            turn_generation: 0,
            completed_generation: None,
        };
        let public = project_public(&snapshot, None);
        let value = serde_json::to_string(&public).unwrap();
        for private in [
            "PRIVATE_EXECUTABLE",
            "PRIVATE_ARGUMENT",
            "process",
            "multiplexer",
            "capabilities",
            "lifecycle",
            "activity",
            "turn_generation",
        ] {
            assert!(!value.contains(private));
        }
        assert_eq!(public.provider, "fixture");
        assert_eq!(public.status, PublicStatus::Idle);
    }
    #[test]
    fn live_metadata_uses_stable_snake_case() {
        let value = serde_json::to_value(LiveEventMetadata {
            kind: EventKind::WaitingApproval,
            status_reason: Some(StatusReasonContext {
                summary: "Run tests".into(),
                source: StatusReasonSource::ToolSummary,
            }),
            turn_id: None,
        })
        .unwrap();
        assert_eq!(value["kind"], "waiting_approval");
        assert_eq!(value["status_reason"]["source"], "tool_summary");
    }
}
