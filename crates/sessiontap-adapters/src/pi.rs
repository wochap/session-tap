use crate::{
    NormalizeContext, SetupAction, SetupReport, artifact::NoCollector, bounded_field,
    dialect::HookDialect, driver::HookAdapter, effort_level, normalized_tool_label, status_excerpt,
};
use anyhow::{Context, Result};
use fs2::FileExt;
use serde_json::Value;
use sessiontap_core::ProviderId;
use sessiontap_core::domain::{
    EventKind, ProviderMetadata, StatusReasonContext, StatusReasonSource,
    TOOL_CORRELATION_ID_MAX_CHARS, ToolActivityPhase, ToolActivityUpdate, Usage,
};
use sessiontap_infra::fs::atomic_write;
use std::{
    fs::{self, OpenOptions},
    path::{Path, PathBuf},
};

/// Pi lifecycle events the managed extension subscribes to. `turn_end` is
/// subscribed for local excerpt and usage accounting only and is never
/// forwarded to the broker.
pub const SUBSCRIBED_EVENTS: &[&str] = &[
    "session_start",
    "session_shutdown",
    "session_info_changed",
    "model_select",
    "thinking_level_select",
    "before_agent_start",
    "turn_start",
    "turn_end",
    "tool_execution_start",
    "tool_execution_end",
    "agent_settled",
];

/// Wire events the managed extension forwards to `sessiontap hook emit pi`.
pub const FORWARDED_EVENTS: &[&str] = &[
    "session_start",
    "session_shutdown",
    "session_info_changed",
    "model_select",
    "thinking_level_select",
    "before_agent_start",
    "turn_start",
    "tool_execution_start",
    "tool_execution_end",
    "agent_settled",
];

pub const MANAGED_EXTENSION_FILE: &str = "sessiontap.ts";
pub const OWNERSHIP_MARKER: &str = "// sessiontap-managed-extension v1";

const SESSION_NAME_MAX_CHARS: usize = 160;
const EXECUTABLE_PLACEHOLDER: &str = "\"__SESSIONTAP_EXECUTABLE__\"";

/// Pi observes agent lifecycle through in-process TypeScript extensions
/// instead of configuration-file hooks. The managed extension forwards
/// bounded payloads on a private wire format keyed by `pi_event`; cumulative
/// usage is accumulated by the extension itself, so the adapter never reads
/// pi session artifacts and never emits a collection context.
pub type PiAdapter = HookAdapter<PiDialect, NoCollector>;

#[allow(non_upper_case_globals)]
pub const PiAdapter: PiAdapter = HookAdapter::new(PiDialect, NoCollector, manage_extension);

pub struct PiDialect;

impl HookDialect for PiDialect {
    fn id(&self) -> ProviderId {
        ProviderId::Pi
    }
    fn classify(&self, raw: &Value) -> Option<EventKind> {
        classify(raw)
    }
    /// The managed extension forwards root-session events only.
    fn is_subagent(&self, _raw: &Value) -> bool {
        false
    }
    fn provider_event_id(&self, _raw: &Value) -> Option<String> {
        None
    }
    fn session_name(&self, raw: &Value) -> Option<String> {
        bounded_field(raw, &["session_name"], SESSION_NAME_MAX_CHARS)
    }
    fn start_reason(&self, raw: &Value) -> Option<String> {
        bounded_field(raw, &["reason"], 32)
            .filter(|v| matches!(v.as_str(), "startup" | "new" | "resume" | "fork" | "reload"))
    }
    fn metadata(&self, raw: &Value) -> Option<ProviderMetadata> {
        let metadata = ProviderMetadata {
            model: bounded_field(raw, &["model"], 160),
            effort: bounded_field(raw, &["thinking_level"], 32)
                .and_then(|level| effort_level(&level)),
            permission_mode: None,
            current_turn_id: None,
        };
        (metadata != ProviderMetadata::default()).then_some(metadata)
    }
    fn inline_usage(&self, raw: &Value) -> Option<Usage> {
        let usage = Usage {
            input_tokens: raw.get("input_tokens").and_then(Value::as_u64),
            output_tokens: raw.get("output_tokens").and_then(Value::as_u64),
            context_tokens: raw.get("context_tokens").and_then(Value::as_u64),
            context_window_percent: raw
                .get("context_window_percent")
                .and_then(Value::as_u64)
                .filter(|value| *value <= 100)
                .map(|value| value as u8),
        };
        (usage != Usage::default()).then_some(usage)
    }
    fn completed_reason(&self, raw: &Value) -> Option<StatusReasonContext> {
        raw.get("excerpt")
            .and_then(Value::as_str)
            .and_then(status_excerpt)
            .map(|summary| StatusReasonContext {
                summary,
                source: StatusReasonSource::AssistantMessage,
            })
    }
    fn tool_activity(
        &self,
        raw: &Value,
        _context: &NormalizeContext<'_>,
    ) -> Option<ToolActivityUpdate> {
        let phase = match raw.get("pi_event")?.as_str()? {
            "tool_execution_start" => ToolActivityPhase::Start,
            "tool_execution_end" => ToolActivityPhase::Finish,
            _ => return None,
        };
        let label = normalized_tool_label(raw.get("tool_name")?.as_str()?)?;
        Some(ToolActivityUpdate {
            phase,
            label,
            correlation_id: bounded_field(raw, &["tool_call_id"], TOOL_CORRELATION_ID_MAX_CHARS),
            detail: None,
        })
    }
}

fn classify(raw: &Value) -> Option<EventKind> {
    let name = raw.get("pi_event")?.as_str()?;
    match name {
        "session_start" => Some(EventKind::ProviderSessionStarted),
        "session_shutdown" => Some(EventKind::ProviderSessionEnded),
        "before_agent_start" => Some(EventKind::NewTurn),
        "turn_start" => Some(EventKind::Working),
        "tool_execution_start" | "tool_execution_end" => Some(EventKind::Working),
        "session_info_changed" | "model_select" | "thinking_level_select" => {
            Some(EventKind::Enrichment)
        }
        "agent_settled" => match raw.get("settled_status").and_then(Value::as_str) {
            Some("error") => Some(EventKind::Failed),
            Some("aborted") => Some(EventKind::Interrupted),
            _ => Some(EventKind::Completed),
        },
        _ => None,
    }
}

fn extension_path(home: &Path) -> PathBuf {
    home.join(".pi/agent/extensions")
        .join(MANAGED_EXTENSION_FILE)
}

pub fn render_extension(executable: &str) -> Result<String> {
    let quoted = serde_json::to_string(executable)?;
    Ok(EXTENSION_TEMPLATE.replace(EXECUTABLE_PLACEHOLDER, &quoted))
}

fn managed_content(path: &Path) -> Option<String> {
    fs::read_to_string(path).ok()
}

fn is_owned(content: &str) -> bool {
    content.contains(OWNERSHIP_MARKER)
}

pub fn manage_extension(
    home: &Path,
    executable: &Path,
    action: SetupAction,
) -> Result<SetupReport> {
    let executable = executable
        .to_str()
        .context("SessionTap executable path is not valid UTF-8")?;
    let rendered = render_extension(executable)?;
    let path = extension_path(home);
    let dir = path.parent().expect("extension path has a parent");
    let existing = managed_content(&path);
    match action {
        SetupAction::Ensure => {
            if let Some(content) = &existing
                && !is_owned(content)
            {
                anyhow::bail!(
                    "{} is not SessionTap-owned; refusing to modify it",
                    path.display()
                );
            }
            if existing.as_deref() == Some(rendered.as_str()) {
                return Ok(SetupReport {
                    changed: false,
                    healthy: true,
                    message: "managed extension healthy".into(),
                });
            }
            fs::create_dir_all(dir)?;
            let lock = OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(dir.join(format!("{MANAGED_EXTENSION_FILE}.sessiontap.lock")))?;
            lock.lock_exclusive()?;
            atomic_write(&path, rendered.as_bytes(), 0o600)?;
            Ok(SetupReport {
                changed: true,
                healthy: true,
                message: "managed extension installed".into(),
            })
        }
        SetupAction::Doctor => {
            let healthy = existing.as_deref() == Some(rendered.as_str());
            Ok(SetupReport {
                changed: false,
                healthy,
                message: if healthy {
                    "managed extension healthy".into()
                } else {
                    "managed extension needs refresh".into()
                },
            })
        }
        SetupAction::Remove => {
            if let Some(content) = &existing
                && is_owned(content)
            {
                fs::remove_file(&path)?;
                return Ok(SetupReport {
                    changed: true,
                    healthy: true,
                    message: "managed extension removed".into(),
                });
            }
            Ok(SetupReport {
                changed: false,
                healthy: true,
                message: "managed extension absent".into(),
            })
        }
    }
}

const EXTENSION_TEMPLATE: &str = include_str!("../assets/pi-extension.ts");

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AgentAdapter, CollectSessionDataRequest, CollectionOutcome};
    use serde_json::json;
    use sessiontap_core::domain::{AdapterOutcome, InvocationId};
    use std::collections::BTreeSet;

    /// Length of the template before it moved to `assets/pi-extension.ts`.
    const TEMPLATE_BYTES: usize = 9737;

    #[test]
    fn fixture_covers_every_forwarded_event_and_matches_expected_kinds() {
        let id = InvocationId::new();
        let cases: Vec<Value> =
            serde_json::from_str(include_str!("../tests/fixtures/pi-events.json")).unwrap();
        let covered = cases
            .iter()
            .filter_map(|case| case["payload"]["pi_event"].as_str())
            .collect::<BTreeSet<_>>();
        assert_eq!(
            covered,
            FORWARDED_EVENTS.iter().copied().collect::<BTreeSet<_>>(),
            "fixture coverage differs from forwarded pi events"
        );
        for case in cases {
            let payload = &case["payload"];
            let outcome = AgentAdapter::normalize(&PiAdapter, &id, payload)
                .unwrap()
                .into_event()
                .expect("fixture payload must normalize");
            assert_eq!(
                serde_json::to_value(outcome.event.kind).unwrap(),
                case["expected"],
                "unexpected pi outcome for {payload}"
            );
        }
    }

    #[test]
    fn unknown_drifted_and_ignored_by_design_payloads_are_ignored() {
        let id = InvocationId::new();
        for payload in [
            json!({"pi_event":"agent_settled_later"}),
            json!({"pi_event":"session_starts"}),
            json!({"pi_event":"tool_execution_update","tool_name":"bash"}),
            json!({"pi_event":"message_end"}),
            json!({"pi_event":"agent_end"}),
            json!({"pi_event":"turn_end","session_id":"s"}),
            json!({"hook_event_name":"session_start"}),
            json!({"event_name":"session_start"}),
            json!({"session_id":"s"}),
            json!(null),
        ] {
            assert_eq!(
                AgentAdapter::normalize(&PiAdapter, &id, &payload).unwrap(),
                AdapterOutcome::Ignored,
                "accepted {payload}"
            );
        }
    }

    #[test]
    fn missing_optional_fields_degrade_to_absent_metadata() {
        let id = InvocationId::new();
        let start = PiAdapter
            .normalize(&id, &json!({"pi_event":"session_start"}))
            .unwrap();
        assert_eq!(start.event.kind, EventKind::ProviderSessionStarted);
        assert!(start.event.provider_session_id.is_none());
        assert!(start.event.provider_metadata.is_none());
        assert!(start.event.usage.is_none());

        let settled = PiAdapter
            .normalize(&id, &json!({"pi_event":"agent_settled"}))
            .unwrap();
        assert_eq!(settled.event.kind, EventKind::Completed);
        assert!(settled.status_reason.is_none());
        assert!(settled.event.usage.is_none());

        let unknown_status = PiAdapter
            .normalize(
                &id,
                &json!({"pi_event":"agent_settled","settled_status":"something_new"}),
            )
            .unwrap();
        assert_eq!(unknown_status.event.kind, EventKind::Completed);
    }

    #[test]
    fn settled_payloads_carry_excerpt_reason_failure_category_and_usage() {
        let id = InvocationId::new();
        let raw: Value =
            serde_json::from_str(include_str!("../tests/fixtures/pi-settled.json")).unwrap();
        let settled = PiAdapter.normalize(&id, &raw).unwrap();
        assert_eq!(settled.event.kind, EventKind::Completed);
        let reason = settled.status_reason.unwrap();
        assert_eq!(
            reason.summary,
            "Refactored the adapter registry and ran the workspace tests."
        );
        assert_eq!(reason.source, StatusReasonSource::AssistantMessage);
        let usage = settled.event.usage.unwrap();
        assert_eq!(usage.input_tokens, Some(1543));
        assert_eq!(usage.output_tokens, Some(402));
        assert_eq!(usage.context_tokens, Some(18204));
        assert_eq!(usage.context_window_percent, Some(9));
        assert_eq!(
            settled.event.provider_session_id.as_deref(),
            Some("pi-session-1")
        );
        assert_eq!(
            settled.event.provider_session_name.as_deref(),
            Some("adapter work")
        );

        let failed = PiAdapter
            .normalize(
                &id,
                &json!({"pi_event":"agent_settled","settled_status":"error","reason":"timeout","input_tokens":5}),
            )
            .unwrap();
        assert_eq!(failed.event.kind, EventKind::Failed);
        assert_eq!(failed.status_reason.unwrap().summary, "Timed out");

        let interrupted = PiAdapter
            .normalize(
                &id,
                &json!({"pi_event":"agent_settled","settled_status":"aborted"}),
            )
            .unwrap();
        assert_eq!(interrupted.event.kind, EventKind::Interrupted);
        assert!(interrupted.status_reason.is_none());
    }

    #[test]
    fn session_and_metadata_fields_are_normalized_and_bounded() {
        let id = InvocationId::new();
        let started = PiAdapter
            .normalize(
                &id,
                &json!({
                    "pi_event":"session_start",
                    "reason":"resume",
                    "session_id":"s-9",
                    "session_name":"My session",
                    "model":"anthropic/claude-sonnet-4-5",
                    "thinking_level":"minimal",
                    "mode":"interactive"
                }),
            )
            .unwrap();
        assert_eq!(
            started.event.provider_session_start_reason.as_deref(),
            Some("resume")
        );
        assert_eq!(
            started.event.provider_session_name.as_deref(),
            Some("My session")
        );
        let metadata = started.event.provider_metadata.unwrap();
        assert_eq!(
            metadata.model.as_deref(),
            Some("anthropic/claude-sonnet-4-5")
        );
        assert_eq!(metadata.effort.as_deref(), Some("minimal"));
        assert!(metadata.permission_mode.is_none());

        let drifted_reason = PiAdapter
            .normalize(
                &id,
                &json!({"pi_event":"session_start","reason":"something_new"}),
            )
            .unwrap();
        assert!(drifted_reason.event.provider_session_start_reason.is_none());

        let drifted_level = PiAdapter
            .normalize(
                &id,
                &json!({"pi_event":"thinking_level_select","thinking_level":"extreme"}),
            )
            .unwrap();
        assert!(
            drifted_level
                .event
                .provider_metadata
                .as_ref()
                .and_then(|metadata| metadata.effort.as_ref())
                .is_none()
        );

        let long_name = "界".repeat(SESSION_NAME_MAX_CHARS + 40);
        let oversized = PiAdapter
            .normalize(
                &id,
                &json!({"pi_event":"session_info_changed","session_name":long_name}),
            )
            .unwrap();
        assert!(
            oversized.event.provider_session_name.as_deref().is_none(),
            "oversized session name must be dropped, not truncated"
        );
    }

    #[test]
    fn tool_activity_is_bounded_correlated_and_result_free() {
        let id = InvocationId::new();
        let started = PiAdapter
            .normalize(
                &id,
                &json!({
                    "pi_event":"tool_execution_start",
                    "tool_name":"bash",
                    "tool_call_id":"call-7",
                    "args":{"command":"PRIVATE"}
                }),
            )
            .unwrap();
        let tool = started.event.tool_activity.as_ref().unwrap();
        assert_eq!(tool.phase, ToolActivityPhase::Start);
        assert_eq!(tool.label, "shell");
        assert_eq!(tool.correlation_id.as_deref(), Some("call-7"));
        assert!(tool.detail.is_none());
        assert!(
            !serde_json::to_string(&started.event)
                .unwrap()
                .contains("PRIVATE")
        );

        let finished = PiAdapter
            .normalize(
                &id,
                &json!({
                    "pi_event":"tool_execution_end",
                    "tool_name":"read",
                    "tool_call_id":"call-8",
                    "result":{"content":"PRIVATE"}
                }),
            )
            .unwrap();
        let tool = finished.event.tool_activity.as_ref().unwrap();
        assert_eq!(tool.phase, ToolActivityPhase::Finish);
        assert_eq!(tool.label, "read_file");
        assert!(
            !serde_json::to_string(&finished.event)
                .unwrap()
                .contains("PRIVATE")
        );

        let unmapped = PiAdapter
            .normalize(
                &id,
                &json!({"pi_event":"tool_execution_start","tool_name":"bad tool!"}),
            )
            .unwrap();
        assert!(unmapped.event.tool_activity.is_none());
    }

    #[test]
    fn pi_never_emits_collection_context_or_attention_signals() {
        let id = InvocationId::new();
        for payload in [
            json!({"pi_event":"session_start","session_id":"s","session_file":"/home/u/.pi/sessions/s.json"}),
            json!({"pi_event":"agent_settled","settled_status":"complete","session_id":"s"}),
            json!({"pi_event":"tool_execution_start","tool_name":"bash","tool_call_id":"c"}),
        ] {
            let normalized = AgentAdapter::normalize(&PiAdapter, &id, &payload)
                .unwrap()
                .into_event()
                .unwrap();
            assert!(normalized.collection_context.is_none());
            assert!(!matches!(
                normalized.event.kind,
                EventKind::WaitingApproval | EventKind::WaitingInput
            ));
        }
    }

    fn user_extension(dir: &Path) -> PathBuf {
        let path = dir.join("user-ext.ts");
        fs::write(
            &path,
            "// my own extension\nexport default function () {}\n",
        )
        .unwrap();
        path
    }

    #[test]
    fn setup_is_idempotent_and_leaves_user_extensions_byte_identical() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join(".pi/agent/extensions");
        fs::create_dir_all(&dir).unwrap();
        let user = user_extension(&dir);
        let before = fs::read(&user).unwrap();

        let report = manage_extension(
            temp.path(),
            Path::new("/opt/session tap"),
            SetupAction::Ensure,
        )
        .unwrap();
        assert!(report.changed);
        let installed = fs::read_to_string(dir.join(MANAGED_EXTENSION_FILE)).unwrap();
        assert!(installed.contains(OWNERSHIP_MARKER));
        assert!(installed.contains("\"/opt/session tap\""));
        assert_eq!(fs::read(&user).unwrap(), before);

        let again = manage_extension(
            temp.path(),
            Path::new("/opt/session tap"),
            SetupAction::Ensure,
        )
        .unwrap();
        assert!(!again.changed);
        assert_eq!(fs::read(&user).unwrap(), before);
    }

    #[test]
    fn doctor_detects_missing_edited_and_stale_managed_extension() {
        let temp = tempfile::tempdir().unwrap();
        let executable = Path::new("/opt/sessiontap");
        assert!(
            !manage_extension(temp.path(), executable, SetupAction::Doctor)
                .unwrap()
                .healthy
        );

        manage_extension(temp.path(), executable, SetupAction::Ensure).unwrap();
        let healthy = manage_extension(temp.path(), executable, SetupAction::Doctor).unwrap();
        assert!(healthy.healthy);
        assert!(!healthy.changed);

        let path = extension_path(temp.path());
        fs::write(&path, format!("{OWNERSHIP_MARKER}\n// edited\n")).unwrap();
        assert!(
            !manage_extension(temp.path(), executable, SetupAction::Doctor)
                .unwrap()
                .healthy
        );

        manage_extension(temp.path(), executable, SetupAction::Ensure).unwrap();
        assert!(
            !manage_extension(temp.path(), Path::new("/opt/other"), SetupAction::Doctor)
                .unwrap()
                .healthy,
            "stale executable path must read as drift"
        );
        assert!(
            manage_extension(temp.path(), executable, SetupAction::Doctor)
                .unwrap()
                .healthy
        );
    }

    #[test]
    fn remove_is_ownership_scoped_and_user_extensions_survive_every_action() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join(".pi/agent/extensions");
        fs::create_dir_all(&dir).unwrap();
        let user = user_extension(&dir);
        let before = fs::read(&user).unwrap();
        let managed = dir.join(MANAGED_EXTENSION_FILE);

        manage_extension(
            temp.path(),
            Path::new("/opt/sessiontap"),
            SetupAction::Ensure,
        )
        .unwrap();
        for action in [SetupAction::Doctor, SetupAction::Remove] {
            manage_extension(temp.path(), Path::new("/opt/sessiontap"), action).unwrap();
            assert_eq!(fs::read(&user).unwrap(), before);
        }
        assert!(!managed.exists());

        let report = manage_extension(
            temp.path(),
            Path::new("/opt/sessiontap"),
            SetupAction::Remove,
        )
        .unwrap();
        assert!(!report.changed);

        // A user file occupying the managed filename is never overwritten or
        // deleted.
        fs::write(&managed, "// user-authored, happens to share the name\n").unwrap();
        let user_content = fs::read(&managed).unwrap();
        assert!(
            manage_extension(
                temp.path(),
                Path::new("/opt/sessiontap"),
                SetupAction::Ensure
            )
            .is_err()
        );
        assert_eq!(fs::read(&managed).unwrap(), user_content);
        assert!(
            !manage_extension(
                temp.path(),
                Path::new("/opt/sessiontap"),
                SetupAction::Doctor
            )
            .unwrap()
            .healthy
        );
        let report = manage_extension(
            temp.path(),
            Path::new("/opt/sessiontap"),
            SetupAction::Remove,
        )
        .unwrap();
        assert!(!report.changed);
        assert_eq!(fs::read(&managed).unwrap(), user_content);
    }

    #[test]
    fn rendered_extension_embeds_escaped_executable_path() {
        let rendered = render_extension("/opt/weird \"path\" \\ here").unwrap();
        assert!(rendered.contains("\"/opt/weird \\\"path\\\" \\\\ here\""));
        assert!(rendered.contains(OWNERSHIP_MARKER));
        assert!(
            rendered.contains("hook\", \"emit\", \"pi\"")
                || rendered.contains("[\"hook\", \"emit\", \"pi\"]")
        );
    }

    #[test]
    fn registry_resolves_pi_dialect() {
        let config = sessiontap_core::config::Config::default();
        let registry = crate::AdapterRegistry::new(&config);
        let (adapter, executable) = registry.resolve("pi").unwrap();
        assert_eq!(adapter.dialect(), "pi");
        assert_eq!(executable, "pi");
    }

    #[tokio::test]
    async fn collection_is_unsupported_without_a_diagnostic() {
        let outcome = PiAdapter
            .collect_session_data(CollectSessionDataRequest {
                home: PathBuf::from("/nonexistent"),
                key: crate::ProviderSessionKey {
                    configured_provider: "pi".into(),
                    adapter_identity: ProviderId::Pi,
                    provider_session_id: "s".into(),
                },
                locator: PathBuf::from("/nonexistent"),
                prior_cursor: None,
                cancellation: crate::CollectionCancellation::default(),
            })
            .await;
        assert!(matches!(outcome, CollectionOutcome::Unsupported));
    }

    #[test]
    fn managed_extension_template_is_byte_identical() {
        let rendered = render_extension("/opt/sessiontap").unwrap();
        assert!(rendered.starts_with(OWNERSHIP_MARKER));
        assert!(rendered.contains("const SESSIONTAP_EXECUTABLE = \"/opt/sessiontap\";"));
        assert_eq!(EXTENSION_TEMPLATE.len(), TEMPLATE_BYTES);
        assert_eq!(
            EXTENSION_TEMPLATE.matches(EXECUTABLE_PLACEHOLDER).count(),
            1
        );
    }
}
