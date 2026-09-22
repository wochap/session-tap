use crate::{
    CollectSessionDataRequest, NormalizeContext, OpaqueCursor, SessionEnrichment, SetupAction,
    SetupReport, ToolDetailPolicy,
    artifact::{
        ArtifactCursor, CollectError, Collected, SessionCollector, check_cancelled,
        cursor_unchanged, open_bounded_nofollow, optional_u64, validate_under_root,
    },
    bounded_field,
    dialect::HookDialect,
    driver::HookAdapter,
    merge_hook_config, provider_metadata, sanitize_bounded, tool_activity,
};
use anyhow::{Context, Result, bail};
use serde_json::Value;
use sessiontap_core::ProviderId;
use sessiontap_core::domain::{
    ArtifactCollectionContext, EventKind, ProviderMetadata, TOOL_CORRELATION_ID_MAX_CHARS,
    ToolActivityPhase, ToolActivityUpdate, Usage,
};
use std::{
    collections::BTreeSet,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
};

pub const HOOK_EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "PostToolUseFailure",
    "PermissionRequest",
    "Elicitation",
    "Notification",
    "PreCompact",
    "PostCompact",
    "Stop",
    "StopFailure",
    "SessionEnd",
];

pub type ClaudeAdapter = HookAdapter<ClaudeDialect, ClaudeCollector>;

#[allow(non_upper_case_globals)]
pub const ClaudeAdapter: ClaudeAdapter = HookAdapter::new(ClaudeDialect, ClaudeCollector, setup);

fn setup(home: &Path, executable: &Path, action: SetupAction) -> Result<SetupReport> {
    merge_hook_config(
        &home.join(".claude/settings.json"),
        "claude",
        HOOK_EVENTS,
        executable,
        action,
    )
}

const TOOL_CORRELATION_FIELDS: &[&str] = &["tool_use_id"];
const TOOL_DETAIL: ToolDetailPolicy = ToolDetailPolicy {
    described_tools: &["Bash", "bash"],
    path_tools: &["Read", "Write", "Edit"],
    path_fields: &["file_path"],
};

pub struct ClaudeDialect;

impl HookDialect for ClaudeDialect {
    fn id(&self) -> ProviderId {
        ProviderId::Claude
    }
    fn classify(&self, raw: &Value) -> Option<EventKind> {
        classify(raw)
    }
    fn start_reason(&self, raw: &Value) -> Option<String> {
        bounded_field(raw, &["source", "reason", "start_reason"], 32)
            .filter(|v| matches!(v.as_str(), "startup" | "clear" | "resume" | "compact"))
    }
    fn metadata(&self, raw: &Value) -> Option<ProviderMetadata> {
        provider_metadata(raw, Some("prompt_id"))
    }
    fn turn_id(&self, raw: &Value) -> Option<String> {
        raw.get("turn_id")
            .or_else(|| raw.get("prompt_id"))
            .and_then(Value::as_str)
            .and_then(|v| sanitize_bounded(v, 128))
    }
    fn tool_activity(
        &self,
        raw: &Value,
        context: &NormalizeContext<'_>,
    ) -> Option<ToolActivityUpdate> {
        let phase = match raw.get("hook_event_name")?.as_str()? {
            "PreToolUse" => ToolActivityPhase::Start,
            "ToolProgress" => ToolActivityPhase::Progress,
            "PostToolUse" => ToolActivityPhase::Finish,
            "PostToolUseFailure" => ToolActivityPhase::Failure,
            "PermissionRequest" => ToolActivityPhase::Attention,
            _ => return None,
        };
        tool_activity(
            phase,
            raw.get("tool_name")?.as_str()?,
            bounded_field(raw, TOOL_CORRELATION_FIELDS, TOOL_CORRELATION_ID_MAX_CHARS),
            raw.get("tool_input"),
            &TOOL_DETAIL,
            context.workspace,
        )
    }
    fn collection_context(&self, raw: &Value) -> Option<ArtifactCollectionContext> {
        Some(ArtifactCollectionContext {
            adapter_identity: ProviderId::Claude,
            provider_session_id: raw.get("session_id")?.as_str()?.trim().to_owned(),
            locator: PathBuf::from(raw.get("transcript_path")?.as_str()?),
        })
        .filter(|context| {
            !context.provider_session_id.is_empty() && !context.locator.as_os_str().is_empty()
        })
    }
}

const MAX_SCAN_BYTES: u64 = 64 * 1024 * 1024;
const MAX_LINE_BYTES: usize = 1024 * 1024;

#[derive(Clone, Copy)]
pub struct ClaudeCollector;

impl SessionCollector for ClaudeCollector {
    fn collect(&self, request: &CollectSessionDataRequest) -> Result<Collected, CollectError> {
        Ok(scan(request).context("Claude collection")?)
    }
}

fn scan(request: &CollectSessionDataRequest) -> Result<Collected> {
    check_cancelled(request)?;
    let canonical = validate_under_root(
        &request.home.join(".claude/projects"),
        &request.locator,
        Some(&request.key.provider_session_id),
        "jsonl",
    )?;
    check_cancelled(request)?;
    let (file, metadata) = open_bounded_nofollow(&canonical, MAX_SCAN_BYTES)?;
    let cursor = ArtifactCursor::from(&metadata);
    if cursor_unchanged(request.prior_cursor.as_ref(), &cursor) {
        return Ok(Collected::Unchanged {
            cursor: OpaqueCursor::new(cursor),
        });
    }
    let mut input = 0_u64;
    let mut output = 0_u64;
    let mut context = None;
    let mut response_ids = BTreeSet::new();
    let mut usage_observed = false;
    let mut session_name = None;
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    loop {
        check_cancelled(request)?;
        line.clear();
        let count = reader.read_until(b'\n', &mut line)?;
        if count == 0 {
            break;
        }
        if line.len() > MAX_LINE_BYTES {
            bail!("Claude artifact record exceeds limit");
        }
        if line.last() != Some(&b'\n') {
            break;
        }
        let value: Value = serde_json::from_slice(&line).context("malformed Claude record")?;
        if let Some(session) = value
            .get("sessionId")
            .or_else(|| value.get("session_id"))
            .and_then(Value::as_str)
            && session != request.key.provider_session_id
        {
            bail!("Claude artifact session mismatch");
        }
        if let Some(title) = ["customTitle", "aiTitle"]
            .into_iter()
            .find_map(|field| value.get(field).and_then(Value::as_str))
            .and_then(|title| sanitize_bounded(title, 160))
        {
            session_name = Some(title);
        }
        let Some(usage) = value
            .get("message")
            .and_then(|message| message.get("usage"))
            .or_else(|| value.get("usage"))
        else {
            continue;
        };
        let Some(response_id) = value
            .get("message")
            .and_then(|message| message.get("id"))
            .and_then(Value::as_str)
            .or_else(|| value.get("requestId").and_then(Value::as_str))
            .filter(|id| !id.trim().is_empty())
        else {
            continue;
        };
        if !response_ids.insert(response_id.to_owned()) {
            continue;
        }
        usage_observed = true;
        let fresh = optional_u64(usage, "input_tokens")?.unwrap_or(0);
        let cache_read = optional_u64(usage, "cache_read_input_tokens")?.unwrap_or(0);
        let cache_create = optional_u64(usage, "cache_creation_input_tokens")?.unwrap_or(0);
        let current = fresh
            .checked_add(cache_read)
            .and_then(|value| value.checked_add(cache_create))
            .context("Claude input token overflow")?;
        input = input
            .checked_add(current)
            .context("Claude cumulative input overflow")?;
        output = output
            .checked_add(optional_u64(usage, "output_tokens")?.unwrap_or(0))
            .context("Claude cumulative output overflow")?;
        context = Some(current);
    }
    check_cancelled(request)?;
    cursor.ensure_stable(reader.get_ref())?;
    Ok(Collected::Complete {
        enrichment: SessionEnrichment {
            session_name,
            usage: usage_observed.then_some(Usage {
                input_tokens: Some(input),
                output_tokens: Some(output),
                context_tokens: context,
                context_window_percent: None,
            }),
        },
        cursor: OpaqueCursor::new(cursor),
    })
}

fn classify(raw: &Value) -> Option<EventKind> {
    let name = raw.get("hook_event_name")?.as_str()?;
    let ask_user = raw
        .get("tool_name")
        .and_then(Value::as_str)
        .is_some_and(|tool| matches!(tool, "AskUserQuestion" | "ask_user_question"));
    match name {
        "SessionStart" => Some(EventKind::ProviderSessionStarted),
        "SessionEnd" => Some(EventKind::ProviderSessionEnded),
        "UserPromptSubmit" => Some(EventKind::NewTurn),
        "PreToolUse" if ask_user => Some(EventKind::WaitingInput),
        "PreToolUse" | "PostToolUse" | "PostToolUseFailure" => Some(EventKind::Working),
        "PermissionRequest" if ask_user => Some(EventKind::WaitingInput),
        "PermissionRequest" => Some(EventKind::WaitingApproval),
        "Elicitation" => Some(EventKind::WaitingInput),
        "Notification" => match raw.get("notification_type").and_then(Value::as_str) {
            Some("permission_prompt") => Some(EventKind::WaitingApproval),
            Some("elicitation_dialog" | "agent_needs_input") => Some(EventKind::WaitingInput),
            Some("idle_prompt") => Some(EventKind::Idle),
            _ => None,
        },
        "PreCompact" => Some(EventKind::Working),
        "PostCompact" => Some(EventKind::Enrichment),
        "Stop" if raw.get("is_interrupt").and_then(Value::as_bool) == Some(true) => {
            Some(EventKind::Interrupted)
        }
        "Stop" => Some(EventKind::Completed),
        "StopFailure" => Some(EventKind::Failed),
        _ => None,
    }
}

#[cfg(test)]
mod collection_tests {
    use super::*;
    use crate::{AgentAdapter, CollectionCancellation, CollectionOutcome, ProviderSessionKey};
    use serde_json::json;
    use std::{fs, os::unix::fs::symlink};

    fn collect(
        request: CollectSessionDataRequest,
    ) -> Result<(SessionEnrichment, OpaqueCursor), CollectError> {
        match ClaudeCollector.collect(&request)? {
            Collected::Complete { enrichment, cursor } => Ok((enrichment, cursor)),
            Collected::Unchanged { .. } => panic!("unexpected unchanged outcome"),
        }
    }

    fn request(
        temp: &tempfile::TempDir,
        session: &str,
        locator: PathBuf,
    ) -> CollectSessionDataRequest {
        CollectSessionDataRequest {
            home: temp.path().to_path_buf(),
            key: ProviderSessionKey {
                configured_provider: "claude".into(),
                adapter_identity: ProviderId::Claude,
                provider_session_id: session.into(),
            },
            locator,
            prior_cursor: None,
            cancellation: CollectionCancellation::default(),
        }
    }

    fn fixture(session: &str, rows: &[&str]) -> (tempfile::TempDir, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join(".claude/projects/project");
        fs::create_dir_all(&root).unwrap();
        let path = root.join(format!("{session}.jsonl"));
        fs::write(&path, rows.join("\n") + "\n").unwrap();
        (temp, path)
    }

    #[test]
    fn collector_deduplicates_cache_usage_and_extracts_latest_title() {
        let row = r#"{"sessionId":"s1","type":"assistant","message":{"id":"m1","usage":{"input_tokens":10,"cache_read_input_tokens":20,"cache_creation_input_tokens":3,"output_tokens":4}}}"#;
        let (temp, path) = fixture(
            "s1",
            &[
                r#"{"aiTitle":"First"}"#,
                row,
                row,
                r#"{"customTitle":"Latest"}"#,
            ],
        );
        let (enrichment, _) = collect(request(&temp, "s1", path)).unwrap();
        assert_eq!(enrichment.session_name.as_deref(), Some("Latest"));
        assert_eq!(
            enrichment.usage.unwrap(),
            Usage {
                input_tokens: Some(33),
                output_tokens: Some(4),
                context_tokens: Some(33),
                context_window_percent: None,
            }
        );
    }

    #[test]
    fn collector_rejects_escape_symlink_mismatch_malformed_overflow_and_cancellation() {
        let (temp, path) = fixture("s2", &["not-json"]);
        assert!(collect(request(&temp, "s2", path)).is_err());

        let (oversize_home, oversize) = fixture("large", &["{}"]);
        fs::OpenOptions::new()
            .write(true)
            .open(&oversize)
            .unwrap()
            .set_len(MAX_SCAN_BYTES + 1)
            .unwrap();
        assert!(collect(request(&oversize_home, "large", oversize)).is_err());

        let outside = temp.path().join("outside.jsonl");
        fs::write(&outside, "{}\n").unwrap();
        assert!(collect(request(&temp, "s2", outside.clone())).is_err());
        let link = temp.path().join(".claude/projects/project/s2.jsonl");
        fs::remove_file(&link).unwrap();
        symlink(&outside, &link).unwrap();
        assert!(collect(request(&temp, "s2", link)).is_err());

        let mismatch = temp.path().join(".claude/projects/project/other.jsonl");
        fs::write(&mismatch, "{}\n").unwrap();
        assert!(collect(request(&temp, "s2", mismatch)).is_err());

        let max = u64::MAX;
        let a =
            json!({"sessionId":"s3","message":{"id":"a","usage":{"input_tokens":max}}}).to_string();
        let b = r#"{"sessionId":"s3","message":{"id":"b","usage":{"input_tokens":1}}}"#;
        let (overflow_home, overflow) = fixture("s3", &[&a, b]);
        assert!(collect(request(&overflow_home, "s3", overflow)).is_err());

        let (cancel_home, cancel_path) = fixture("s4", &["{}"]);
        let cancelled = request(&cancel_home, "s4", cancel_path);
        cancelled.cancellation.cancel();
        assert!(matches!(collect(cancelled), Err(CollectError::Cancelled)));
    }

    #[tokio::test]
    async fn unchanged_cursor_skips_scan_and_changes_force_rescan() {
        let row = r#"{"sessionId":"s1","message":{"id":"m1","usage":{"input_tokens":1,"output_tokens":1}}}"#;
        let (temp, path) = fixture("s1", &[row]);
        let collect_with = |prior: Option<OpaqueCursor>| {
            let mut request = request(&temp, "s1", path.clone());
            request.prior_cursor = prior;
            ClaudeAdapter.collect_session_data(request)
        };
        let CollectionOutcome::Complete { cursor, .. } = collect_with(None).await else {
            panic!("expected complete outcome");
        };
        let CollectionOutcome::Unchanged { cursor: same } =
            collect_with(Some(cursor.clone())).await
        else {
            panic!("expected unchanged outcome");
        };
        assert_eq!(
            same.downcast_ref::<ArtifactCursor>(),
            cursor.downcast_ref::<ArtifactCursor>()
        );

        let appended = r#"{"sessionId":"s1","message":{"id":"m2","usage":{"input_tokens":2,"output_tokens":2}}}"#;
        fs::write(&path, format!("{row}\n{appended}\n")).unwrap();
        let CollectionOutcome::Complete {
            enrichment,
            cursor: grown,
        } = collect_with(Some(cursor.clone())).await
        else {
            panic!("appended artifact must be rescanned");
        };
        assert_eq!(enrichment.usage.unwrap().input_tokens, Some(3));

        let replacement = path.with_extension("tmp");
        fs::write(&replacement, format!("{row}\n")).unwrap();
        fs::rename(&replacement, &path).unwrap();
        let CollectionOutcome::Complete { enrichment, .. } = collect_with(Some(grown)).await else {
            panic!("replaced artifact must be rescanned from the start");
        };
        assert_eq!(enrichment.usage.unwrap().input_tokens, Some(1));
    }

    #[tokio::test]
    async fn setup_doctor_and_remove_preserve_statusline_configuration() {
        let temp = tempfile::tempdir().unwrap();
        let settings = temp.path().join(".claude/settings.json");
        fs::create_dir_all(settings.parent().unwrap()).unwrap();
        let statusline =
            json!({"type":"command","command":"user-status","padding":7,"private":"unchanged"});
        fs::write(
            &settings,
            serde_json::to_vec_pretty(&json!({"statusLine":statusline})).unwrap(),
        )
        .unwrap();
        for action in [
            SetupAction::Ensure,
            SetupAction::Doctor,
            SetupAction::Remove,
        ] {
            ClaudeAdapter
                .setup(temp.path(), Path::new("/opt/sessiontap"), action)
                .await
                .unwrap();
            let current: Value = serde_json::from_slice(&fs::read(&settings).unwrap()).unwrap();
            assert_eq!(current["statusLine"], statusline);
        }
        assert!(
            !temp
                .path()
                .join(".claude/sessiontap-statusline-backup.json")
                .exists()
        );
    }
}

#[cfg(test)]
mod tool_activity_tests {
    use super::*;
    use crate::AgentAdapter;
    use serde_json::json;
    use sessiontap_core::domain::{EventEvidence, InvocationId};
    use std::fs;

    #[test]
    fn tool_activity_selects_only_allowlisted_safe_detail() {
        use sessiontap_core::domain::ToolActivityPhase;
        let id = InvocationId::new();
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("src.rs");
        fs::write(&file, "fn main() {}\n").unwrap();
        let cwd = temp.path().to_string_lossy();

        let shell = ClaudeAdapter
            .normalize(
                &id,
                &json!({
                    "hook_event_name":"PreToolUse",
                    "tool_name":"Bash",
                    "tool_use_id":"tool-1",
                    "cwd":cwd,
                    "tool_input":{"command":"PRIVATE_COMMAND", "description":"Run unit tests"}
                }),
            )
            .unwrap();
        let tool = shell.event.tool_activity.as_ref().unwrap();
        assert_eq!(tool.phase, ToolActivityPhase::Start);
        assert_eq!(tool.label, "shell");
        assert_eq!(tool.correlation_id.as_deref(), Some("tool-1"));
        assert_eq!(tool.detail.as_deref(), Some("Run unit tests"));
        assert!(
            !serde_json::to_string(&shell.event)
                .unwrap()
                .contains("PRIVATE_COMMAND")
        );

        let bound = |raw: &Value| {
            AgentAdapter::normalize_with_evidence(
                &ClaudeAdapter,
                &id,
                raw,
                EventEvidence::managed_hook(1),
                &NormalizeContext {
                    workspace: Some(temp.path()),
                },
            )
            .unwrap()
            .into_event()
            .unwrap()
        };
        let read = bound(&json!({
            "hook_event_name":"PreToolUse",
            "tool_name":"Read",
            "tool_use_id":"tool-2",
            "cwd":"/",
            "tool_input":{"file_path":file}
        }));
        assert_eq!(
            read.event.tool_activity.unwrap().detail.as_deref(),
            Some("src.rs")
        );

        let outside = tempfile::NamedTempFile::new().unwrap();
        let outside_payload = json!({
            "hook_event_name":"PreToolUse",
            "tool_name":"Read",
            "tool_use_id":"tool-outside",
            "tool_input":{"file_path":outside.path()}
        });
        assert!(
            bound(&outside_payload)
                .event
                .tool_activity
                .unwrap()
                .detail
                .is_none()
        );

        // Without a trusted workspace no file target is ever bound.
        assert!(
            ClaudeAdapter
                .normalize(
                    &id,
                    &json!({
                        "hook_event_name":"PreToolUse",
                        "tool_name":"Read",
                        "tool_use_id":"tool-4",
                        "cwd":cwd,
                        "tool_input":{"file_path":file}
                    }),
                )
                .unwrap()
                .event
                .tool_activity
                .unwrap()
                .detail
                .is_none()
        );

        let unsafe_detail = ClaudeAdapter
            .normalize(
                &id,
                &json!({
                    "hook_event_name":"PreToolUse",
                    "tool_name":"Bash",
                    "tool_use_id":"tool-3",
                    "cwd":cwd,
                    "tool_input":{"description":"Open https://example.invalid/?token=secret"}
                }),
            )
            .unwrap();
        assert!(unsafe_detail.event.tool_activity.unwrap().detail.is_none());
    }

    #[test]
    fn payload_supplied_workspace_is_ignored() {
        let id = InvocationId::new();
        let claimed = tempfile::tempdir().unwrap();
        let trusted = tempfile::tempdir().unwrap();
        let file = claimed.path().join("secret.rs");
        fs::write(&file, "").unwrap();
        let claimed_path = claimed.path().to_string_lossy().into_owned();
        let raw = json!({
            "hook_event_name":"PreToolUse",
            "tool_name":"Read",
            "tool_use_id":"tool-1",
            "cwd":claimed_path,
            "workspace":claimed_path,
            "__sessiontap_invocation_workspace":claimed_path,
            "tool_input":{"file_path":file}
        });
        for context in [
            NormalizeContext::default(),
            NormalizeContext {
                workspace: Some(trusted.path()),
            },
        ] {
            let normalized = AgentAdapter::normalize_with_evidence(
                &ClaudeAdapter,
                &id,
                &raw,
                EventEvidence::managed_hook(1),
                &context,
            )
            .unwrap()
            .into_event()
            .unwrap();
            assert!(normalized.event.tool_activity.unwrap().detail.is_none());
        }
        let normalized = AgentAdapter::normalize_with_evidence(
            &ClaudeAdapter,
            &id,
            &raw,
            EventEvidence::managed_hook(1),
            &NormalizeContext {
                workspace: Some(claimed.path()),
            },
        )
        .unwrap()
        .into_event()
        .unwrap();
        assert_eq!(
            normalized.event.tool_activity.unwrap().detail.as_deref(),
            Some("secret.rs")
        );
    }

    #[test]
    fn supported_tool_phase_is_exact_and_result_free() {
        let id = InvocationId::new();
        let (payload, phase) = (
            json!({"hook_event_name":"PostToolUseFailure","tool_name":"Bash","tool_use_id":"a","error":"PRIVATE"}),
            ToolActivityPhase::Failure,
        );
        let event = ClaudeAdapter.normalize(&id, &payload).unwrap().event;
        assert_eq!(event.tool_activity.as_ref().unwrap().phase, phase);
        assert!(!serde_json::to_string(&event).unwrap().contains("PRIVATE"));
    }
}
