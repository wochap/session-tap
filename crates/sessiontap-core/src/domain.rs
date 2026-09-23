use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::PathBuf;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceChannel {
    ManagedHook,
    SideChannel,
    ProcessObservation,
    ProviderArtifact,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceTrust {
    AuthenticatedInvocation,
    LocalObservation,
}

pub const COLLECTOR_INSTANCE_ID_MAX_CHARS: usize = 128;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventEvidence {
    pub channel: EvidenceChannel,
    pub trust: EvidenceTrust,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub collector_revision: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub collector_instance_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_sequence: Option<u64>,
}

pub const SOURCE_ORDER_CURSOR_MAX: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceOrderCursor {
    pub channel: EvidenceChannel,
    pub collector_revision: Option<u64>,
    pub collector_instance_id: Option<String>,
    pub sequence: u64,
}

impl EventEvidence {
    #[must_use]
    pub const fn managed_hook(collector_revision: u64) -> Self {
        Self {
            channel: EvidenceChannel::ManagedHook,
            trust: EvidenceTrust::AuthenticatedInvocation,
            collector_revision: Some(collector_revision),
            collector_instance_id: None,
            source_sequence: None,
        }
    }

    #[must_use]
    pub const fn local(channel: EvidenceChannel) -> Self {
        Self {
            channel,
            trust: EvidenceTrust::LocalObservation,
            collector_revision: None,
            collector_instance_id: None,
            source_sequence: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityConfirmation {
    Live,
    RestoredUnconfirmed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolActivityPhase {
    Start,
    Progress,
    Finish,
    Failure,
    Attention,
}

pub const TOOL_CORRELATION_ID_MAX_CHARS: usize = 128;
pub const TOOL_LABEL_MAX_CHARS: usize = 64;
pub const TOOL_DETAIL_MAX_CHARS: usize = 160;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolActivityUpdate {
    pub phase: ToolActivityPhase,
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CurrentToolActivity {
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    pub started_at: DateTime<Utc>,
    pub last_observed_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PublicStatus {
    Running,
    Idle,
    Blocked,
    Stopped,
}

impl PublicStatus {
    /// Canonical serialized name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Idle => "idle",
            Self::Blocked => "blocked",
            Self::Stopped => "stopped",
        }
    }
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

impl PublicReasonKind {
    /// Canonical serialized name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Input => "input",
            Self::Approval => "approval",
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicStatusReason {
    pub kind: PublicReasonKind,
    pub summary: String,
}

/// Observer-facing reason for one child agent. A running child may carry a
/// summary without a kind; a stopped child may carry a kind without a summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicChildAgentReason {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<PublicReasonKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

/// Bounded observer-facing view of one child agent of the root invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicChildAgentView {
    pub agent_id: String,
    pub agent_type: String,
    pub status: PublicStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<PublicChildAgentReason>,
    pub started_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub children: Option<Vec<PublicChildAgentView>>,
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
    Children,
}

impl PublicField {
    /// Canonical serialized field path.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvocationId => "invocation_id",
            Self::Provider => "provider",
            Self::Status => "status",
            Self::Reason => "reason",
            Self::Cwd => "cwd",
            Self::CreatedAt => "created_at",
            Self::UpdatedAt => "updated_at",
            Self::Session => "session",
            Self::Metadata => "metadata",
            Self::Usage => "usage",
            Self::Repository => "repository",
            Self::Children => "children",
        }
    }
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
        children: project_children(&snapshot.children),
    }
}

/// Children sorted by start time then agent ID; `None` when none are retained.
fn project_children(children: &[ChildAgentState]) -> Option<Vec<PublicChildAgentView>> {
    if children.is_empty() {
        return None;
    }
    let mut views = children
        .iter()
        .map(|child| PublicChildAgentView {
            agent_id: child.agent_id.clone(),
            agent_type: child.agent_type.clone(),
            status: child.activity.public_status(),
            reason: child.reason.clone(),
            started_at: child.started_at,
            updated_at: child.updated_at,
        })
        .collect::<Vec<_>>();
    views.sort_by(|a, b| {
        a.started_at
            .cmp(&b.started_at)
            .then_with(|| a.agent_id.cmp(&b.agent_id))
    });
    Some(views)
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
            PublicField::Children,
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
    field!(children, Children);
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

/// Private provider-owned artifact locator carried only over the authenticated
/// local control protocol. It is deliberately absent from snapshots.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactCollectionContext {
    pub adapter_identity: crate::ProviderId,
    pub provider_session_id: String,
    pub locator: PathBuf,
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

/// Terminal multiplexer that hosts an invocation. Serialized as the same
/// lowercase string persisted before the enum existed.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum MultiplexerBackend {
    #[default]
    Tmux,
}

impl MultiplexerBackend {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Tmux => "tmux",
        }
    }
}

impl std::fmt::Display for MultiplexerBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct MultiplexerMetadata {
    pub backend: MultiplexerBackend,
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

pub const CHILD_AGENTS_MAX: usize = 32;
pub const CHILD_AGENT_ID_MAX_CHARS: usize = TOOL_CORRELATION_ID_MAX_CHARS;
pub const CHILD_AGENT_TYPE_MAX_CHARS: usize = TOOL_LABEL_MAX_CHARS;

/// Provider-owned identity of the child agent a payload belongs to. Both
/// values are sanitized and bounded by the adapter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChildAgentRef {
    pub agent_id: String,
    pub agent_type: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChildActivity {
    Running,
    Blocked,
    Stopped,
}

impl ChildActivity {
    #[must_use]
    pub const fn public_status(self) -> PublicStatus {
        match self {
            Self::Running => PublicStatus::Running,
            Self::Blocked => PublicStatus::Blocked,
            Self::Stopped => PublicStatus::Stopped,
        }
    }
}

/// Retained state of one child agent within the current root turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChildAgentState {
    pub agent_id: String,
    pub agent_type: String,
    pub activity: ChildActivity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<PublicChildAgentReason>,
    pub started_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
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
    pub state_started_at: DateTime<Utc>,
    pub last_state_asserted_at: Option<DateTime<Utc>>,
    pub activity_confirmation: ActivityConfirmation,
    pub last_evidence: Option<EventEvidence>,
    pub source_ordering: Vec<SourceOrderCursor>,
    pub current_tool_activity: Option<CurrentToolActivity>,
    pub status: PublicStatus,
    pub provider_session: Option<ProviderSession>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_metadata: Option<ProviderMetadata>,
    pub usage: Option<Usage>,
    pub repository: Option<Repository>,
    pub multiplexer: Option<MultiplexerMetadata>,
    pub capabilities: Capabilities,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<ChildAgentState>,
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
    Interrupted,
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
    #[serde(skip)]
    pub collection_context: Option<ArtifactCollectionContext>,
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
    pub evidence: EventEvidence,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_activity: Option<ToolActivityUpdate>,
    /// Present when the event belongs to a child agent rather than the root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_agent: Option<ChildAgentRef>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn serde_name<T: Serialize>(value: T) -> String {
        serde_json::to_value(value)
            .unwrap()
            .as_str()
            .unwrap()
            .to_owned()
    }

    #[test]
    fn as_str_matches_serialized_names() {
        for status in [
            PublicStatus::Running,
            PublicStatus::Idle,
            PublicStatus::Blocked,
            PublicStatus::Stopped,
        ] {
            assert_eq!(status.as_str(), serde_name(status));
        }
        for kind in [
            PublicReasonKind::Input,
            PublicReasonKind::Approval,
            PublicReasonKind::Completed,
            PublicReasonKind::Failed,
        ] {
            assert_eq!(kind.as_str(), serde_name(kind));
        }
        for field in [
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
            PublicField::Children,
        ] {
            assert_eq!(field.as_str(), serde_name(field));
        }
        assert_eq!(
            MultiplexerBackend::Tmux.as_str(),
            serde_name(MultiplexerBackend::Tmux)
        );
    }

    /// Written by the code before `MultiplexerBackend` existed, when the
    /// backend was a free-form string.
    #[test]
    fn pre_enum_snapshot_round_trips_identically() {
        let raw = include_str!("../tests/golden/pre-enum-tmux-snapshot.json");
        let snapshot: InvocationSnapshot = serde_json::from_str(raw).unwrap();
        assert_eq!(
            snapshot.multiplexer.as_ref().map(|m| m.backend),
            Some(MultiplexerBackend::Tmux)
        );
        assert_eq!(
            serde_json::to_string_pretty(&snapshot).unwrap(),
            raw.trim_end()
        );
        let unknown = raw.replace("\"backend\": \"tmux\"", "\"backend\": \"kitty\"");
        assert!(serde_json::from_str::<InvocationSnapshot>(&unknown).is_err());
    }

    #[test]
    fn snapshot_without_children_keeps_its_serialized_keys() {
        let raw = include_str!("../tests/golden/pre-enum-tmux-snapshot.json");
        let snapshot: InvocationSnapshot = serde_json::from_str(raw).unwrap();
        assert!(snapshot.children.is_empty());
        let value = serde_json::to_value(&snapshot).unwrap();
        let keys: Vec<String> =
            serde_json::from_str(include_str!("../tests/golden/snapshot-keys.json")).unwrap();
        for key in keys {
            assert!(value.get(&key).is_some(), "{key}");
        }
        assert!(value.get("children").is_none());
        let view = serde_json::to_value(project_public(&snapshot, None)).unwrap();
        assert!(view.get("children").is_none());
    }

    #[test]
    fn child_agent_types_round_trip() {
        let now = Utc::now();
        let reference = ChildAgentRef {
            agent_id: "agent-1".into(),
            agent_type: "Explore".into(),
        };
        let state = ChildAgentState {
            agent_id: "agent-1".into(),
            agent_type: "Explore".into(),
            activity: ChildActivity::Blocked,
            reason: Some(PublicChildAgentReason {
                kind: Some(PublicReasonKind::Approval),
                summary: Some("bash".into()),
            }),
            started_at: now,
            updated_at: now,
        };
        let view = PublicChildAgentView {
            agent_id: "agent-1".into(),
            agent_type: "Explore".into(),
            status: PublicStatus::Running,
            reason: None,
            started_at: now,
            updated_at: now,
        };
        fn round_trip<T: Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug>(
            value: &T,
        ) {
            let json = serde_json::to_string(value).unwrap();
            assert_eq!(&serde_json::from_str::<T>(&json).unwrap(), value);
        }
        round_trip(&reference);
        round_trip(&state);
        round_trip(&view);
        assert!(!serde_json::to_string(&view).unwrap().contains("reason"));
        for activity in [
            ChildActivity::Running,
            ChildActivity::Blocked,
            ChildActivity::Stopped,
        ] {
            assert_eq!(serde_name(activity), activity.public_status().as_str());
        }
        assert_eq!(CHILD_AGENT_ID_MAX_CHARS, TOOL_CORRELATION_ID_MAX_CHARS);
        assert_eq!(CHILD_AGENT_TYPE_MAX_CHARS, TOOL_LABEL_MAX_CHARS);
    }

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
            state_started_at: now,
            last_state_asserted_at: Some(now),
            activity_confirmation: ActivityConfirmation::Live,
            last_evidence: None,
            source_ordering: vec![],
            current_tool_activity: None,
            status: PublicStatus::Stopped,
            provider_session: None,
            provider_metadata: None,
            usage: None,
            repository: None,
            multiplexer: None,
            capabilities: Capabilities::default(),
            turn_generation: 1,
            completed_generation: Some(1),
            children: Vec::new(),
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
            children: None,
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
            state_started_at: now,
            last_state_asserted_at: Some(now),
            activity_confirmation: ActivityConfirmation::Live,
            last_evidence: Some(EventEvidence {
                channel: EvidenceChannel::ProviderArtifact,
                trust: EvidenceTrust::LocalObservation,
                collector_revision: Some(7),
                collector_instance_id: Some("PRIVATE_COLLECTOR".into()),
                source_sequence: Some(9),
            }),
            source_ordering: vec![SourceOrderCursor {
                channel: EvidenceChannel::SideChannel,
                collector_revision: Some(7),
                collector_instance_id: Some("PRIVATE_ORDERING".into()),
                sequence: 9,
            }],
            current_tool_activity: Some(CurrentToolActivity {
                label: "PRIVATE_TOOL".into(),
                correlation_id: Some("PRIVATE_CORRELATION".into()),
                detail: Some("PRIVATE_DETAIL".into()),
                started_at: now,
                last_observed_at: now,
            }),
            status: PublicStatus::Idle,
            provider_session: None,
            provider_metadata: None,
            usage: None,
            repository: None,
            multiplexer: None,
            capabilities: Capabilities::default(),
            turn_generation: 0,
            completed_generation: None,
            children: Vec::new(),
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
            "PRIVATE_COLLECTOR",
            "PRIVATE_ORDERING",
            "PRIVATE_TOOL",
            "PRIVATE_CORRELATION",
            "PRIVATE_DETAIL",
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
