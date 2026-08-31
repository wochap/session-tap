use crate::{
    AgentAdapter, BoundedDiagnostic, CollectSessionDataRequest, CollectionOutcome, OpaqueCursor,
    SessionEnrichment, SetupAction, SetupReport, bounded_field, completed_reason_context,
    failed_reason_context, is_subagent_payload, merge_hook_config, provider_metadata,
    sanitize_bounded, status_reason_context, tool_activity_update,
};
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use chrono::Utc;
use serde_json::Value;
use sessiontap_core::domain::{
    AdapterOutcome, ArtifactCollectionContext, EventEvidence, EventKind, InvocationId,
    NormalizedAdapterEvent, NormalizedEvent, Usage,
};
use std::{
    collections::BTreeSet,
    fs::{self, OpenOptions},
    io::{BufRead, BufReader},
    os::unix::{fs::MetadataExt, fs::OpenOptionsExt},
    path::{Path, PathBuf},
};
use uuid::Uuid;

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
pub struct ClaudeAdapter;

#[cfg(test)]
impl ClaudeAdapter {
    /// Test helper for cases that expect a normalized root event.
    pub fn normalize(&self, id: &InvocationId, raw: &Value) -> Result<NormalizedAdapterEvent> {
        match <Self as AgentAdapter>::normalize(self, id, raw)? {
            AdapterOutcome::Event(event) => Ok(*event),
            AdapterOutcome::Ignored => anyhow::bail!("ignored subagent hook"),
        }
    }
}

#[async_trait]
impl AgentAdapter for ClaudeAdapter {
    fn dialect(&self) -> &'static str {
        "claude"
    }
    fn normalize_with_evidence(
        &self,
        id: &InvocationId,
        raw: &Value,
        evidence: EventEvidence,
    ) -> Result<AdapterOutcome> {
        if is_subagent_payload(raw) {
            return Ok(AdapterOutcome::Ignored);
        }
        let Some(kind) = classify(raw) else {
            return Ok(AdapterOutcome::Ignored);
        };
        let now = Utc::now();
        let status_reason = match kind {
            EventKind::WaitingApproval => status_reason_context(raw, false),
            EventKind::WaitingInput => status_reason_context(raw, true),
            EventKind::Completed => completed_reason_context(raw),
            EventKind::Failed => failed_reason_context(raw),
            _ => None,
        };
        let turn_id = raw
            .get("turn_id")
            .or_else(|| raw.get("prompt_id"))
            .and_then(Value::as_str)
            .and_then(|v| sanitize_bounded(v, 128));
        Ok(AdapterOutcome::Event(Box::new(NormalizedAdapterEvent {
            event: NormalizedEvent {
                schema_version: sessiontap_core::SCHEMA_VERSION,
                event_id: raw
                    .get("event_id")
                    .and_then(Value::as_str)
                    .map_or_else(|| Uuid::new_v4().to_string(), str::to_owned),
                invocation_id: id.clone(),
                provider_event_id: raw
                    .get("event_id")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                provider: "claude".into(),
                observed_at: now,
                received_at: now,
                evidence,
                kind: kind.clone(),
                provider_session_id: raw
                    .get("session_id")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                provider_session_name: None,
                provider_session_start_reason: (kind == EventKind::ProviderSessionStarted)
                    .then(|| bounded_field(raw, &["source", "reason", "start_reason"], 32))
                    .flatten()
                    .filter(|v| matches!(v.as_str(), "startup" | "clear" | "resume" | "compact")),
                provider_metadata: provider_metadata(raw, Some("prompt_id")),
                usage: None,
                turn_id,
                tool_activity: tool_activity_update("claude", raw),
            },
            status_reason,
            collection_context: collection_context(raw),
        })))
    }
    async fn collect_session_data(&self, request: CollectSessionDataRequest) -> CollectionOutcome {
        match tokio::task::spawn_blocking(move || collect(request)).await {
            Ok(Ok((enrichment, cursor))) => CollectionOutcome::Complete {
                enrichment,
                cursor: OpaqueCursor::new(cursor),
            },
            Ok(Err(error)) if error.to_string() == "collection cancelled" => {
                CollectionOutcome::Cancelled
            }
            Ok(Err(error)) => CollectionOutcome::Failed(BoundedDiagnostic::new(error.to_string())),
            Err(error) => CollectionOutcome::Failed(BoundedDiagnostic::new(error.to_string())),
        }
    }
    async fn setup(
        &self,
        home: &Path,
        executable: &Path,
        action: SetupAction,
    ) -> Result<SetupReport> {
        merge_hook_config(
            &home.join(".claude/settings.json"),
            "claude",
            HOOK_EVENTS,
            executable,
            action,
        )
    }
}

fn collection_context(raw: &Value) -> Option<ArtifactCollectionContext> {
    Some(ArtifactCollectionContext {
        adapter_identity: "claude".into(),
        provider_session_id: raw.get("session_id")?.as_str()?.trim().to_owned(),
        locator: PathBuf::from(raw.get("transcript_path")?.as_str()?),
    })
    .filter(|context| {
        !context.provider_session_id.is_empty() && !context.locator.as_os_str().is_empty()
    })
}

const MAX_SCAN_BYTES: u64 = 64 * 1024 * 1024;
const MAX_LINE_BYTES: usize = 1024 * 1024;

#[derive(Clone)]
struct ClaudeCursor {
    _device: u64,
    _inode: u64,
    _stable_len: u64,
}

fn collect(request: CollectSessionDataRequest) -> Result<(SessionEnrichment, ClaudeCursor)> {
    check_cancelled(&request)?;
    let root = fs::canonicalize(request.home.join(".claude/projects"))
        .context("Claude artifact root is unavailable")?;
    let canonical = validate_path(&root, &request.locator, &request.key.provider_session_id)?;
    check_cancelled(&request)?;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&canonical)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > MAX_SCAN_BYTES {
        bail!("Claude artifact is not a bounded regular file");
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
        check_cancelled(&request)?;
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
    check_cancelled(&request)?;
    let after = reader.get_ref().metadata()?;
    if after.dev() != metadata.dev()
        || after.ino() != metadata.ino()
        || after.len() < metadata.len()
    {
        bail!("Claude artifact changed identity during collection");
    }
    Ok((
        SessionEnrichment {
            session_name,
            usage: usage_observed.then_some(Usage {
                input_tokens: Some(input),
                output_tokens: Some(output),
                context_tokens: context,
                context_window_percent: None,
            }),
        },
        ClaudeCursor {
            _device: metadata.dev(),
            _inode: metadata.ino(),
            _stable_len: metadata.len(),
        },
    ))
}

fn validate_path(root: &Path, locator: &Path, session: &str) -> Result<PathBuf> {
    let unresolved = if locator.is_absolute() {
        locator.to_path_buf()
    } else {
        root.join(locator)
    };
    if fs::symlink_metadata(&unresolved)?.file_type().is_symlink() {
        bail!("Claude artifact must not be a symlink");
    }
    let canonical = fs::canonicalize(unresolved)?;
    if !canonical.starts_with(root) {
        bail!("Claude artifact escapes allowed root");
    }
    if canonical.extension().and_then(|value| value.to_str()) != Some("jsonl")
        || canonical.file_stem().and_then(|value| value.to_str()) != Some(session)
    {
        bail!("Claude artifact identity mismatch");
    }
    Ok(canonical)
}

fn optional_u64(value: &Value, field: &str) -> Result<Option<u64>> {
    value
        .get(field)
        .map(|value| value.as_u64().context("invalid Claude usage value"))
        .transpose()
}

fn check_cancelled(request: &CollectSessionDataRequest) -> Result<()> {
    if request.cancellation.is_cancelled() {
        bail!("collection cancelled");
    }
    Ok(())
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
    use crate::{CollectionCancellation, ProviderSessionKey};
    use serde_json::json;
    use std::{fs, os::unix::fs::symlink};

    fn request(
        temp: &tempfile::TempDir,
        session: &str,
        locator: PathBuf,
    ) -> CollectSessionDataRequest {
        CollectSessionDataRequest {
            home: temp.path().to_path_buf(),
            key: ProviderSessionKey {
                configured_provider: "claude".into(),
                adapter_identity: "claude".into(),
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
        assert!(collect(cancelled).is_err());
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
