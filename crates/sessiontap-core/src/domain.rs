use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PublicStatus {
    Running,
    Idle,
    Blocked,
    Stopped,
}

#[must_use]
pub const fn derive_status(lifecycle: Lifecycle, activity: Activity) -> PublicStatus {
    match lifecycle {
        Lifecycle::Exited | Lifecycle::Lost => PublicStatus::Stopped,
        Lifecycle::Starting => PublicStatus::Idle,
        Lifecycle::Alive => match activity {
            Activity::WaitingInput | Activity::WaitingApproval => PublicStatus::Blocked,
            Activity::Working => PublicStatus::Running,
            Activity::Unknown | Activity::Idle => PublicStatus::Idle,
        },
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ProviderSession {
    pub id: String,
    pub name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Usage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub context_tokens: Option<u64>,
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
    WaitingInput,
    WaitingApproval,
    Completed,
    Failed,
    SessionEnded,
    Enrichment,
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
    }

    #[test]
    fn public_snapshot_matches_golden_schema() {
        let now = Utc::now();
        let snapshot = InvocationSnapshot {
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
            activity: Activity::Idle,
            status: PublicStatus::Idle,
            provider_session: None,
            usage: None,
            repository: None,
            multiplexer: None,
            capabilities: Capabilities::default(),
            turn_generation: 0,
            completed_generation: None,
        };
        let value = serde_json::to_value(snapshot).unwrap();
        let mut keys = value
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        keys.sort();
        let expected: Vec<String> =
            serde_json::from_str(include_str!("../tests/golden/snapshot-keys.json")).unwrap();
        assert_eq!(keys, expected);
    }
}
