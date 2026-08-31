use crate::{
    AgentAdapter, BoundedDiagnostic, CollectSessionDataRequest, CollectionOutcome, OpaqueCursor,
    SessionEnrichment, SetupAction, SetupReport, bounded_field, completed_reason_context,
    is_subagent_payload, merge_hook_config, provider_metadata, sanitize_bounded,
    status_reason_context, tool_activity_update,
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
    "PermissionRequest",
    "UserInputRequest",
    "PostToolUse",
    "PreCompact",
    "PostCompact",
    "Interrupt",
    "Stop",
    "SessionEnd",
];
pub struct CodexAdapter;

#[cfg(test)]
impl CodexAdapter {
    /// Test helper for cases that expect a normalized root event.
    pub fn normalize(&self, id: &InvocationId, raw: &Value) -> Result<NormalizedAdapterEvent> {
        match <Self as AgentAdapter>::normalize(self, id, raw)? {
            AdapterOutcome::Event(event) => Ok(*event),
            AdapterOutcome::Ignored => anyhow::bail!("ignored subagent hook"),
        }
    }
}

#[async_trait]
impl AgentAdapter for CodexAdapter {
    fn dialect(&self) -> &'static str {
        "codex"
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
        Ok(AdapterOutcome::Event(Box::new(build(
            id, raw, kind, evidence,
        ))))
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
        let mut report = merge_hook_config(
            &home.join(".codex/hooks.json"),
            "codex",
            HOOK_EVENTS,
            executable,
            action,
        )?;
        if action != SetupAction::Remove {
            report
                .message
                .push_str("; review or refresh trust with Codex /hooks");
        }
        Ok(report)
    }
}

fn classify(raw: &Value) -> Option<EventKind> {
    let name = raw.get("hook_event_name")?.as_str()?;
    let request_input = raw
        .get("tool_name")
        .and_then(Value::as_str)
        .is_some_and(|tool| matches!(tool, "request_user_input" | "functions.request_user_input"));
    match name {
        "SessionStart" => Some(EventKind::ProviderSessionStarted),
        "SessionEnd" => Some(EventKind::ProviderSessionEnded),
        "UserPromptSubmit" => Some(EventKind::NewTurn),
        "PreToolUse" if request_input => Some(EventKind::WaitingInput),
        "PreToolUse" | "PostToolUse" => Some(EventKind::Working),
        "PermissionRequest" if request_input => Some(EventKind::WaitingInput),
        "PermissionRequest" => Some(EventKind::WaitingApproval),
        "UserInputRequest" => Some(EventKind::WaitingInput),
        "PreCompact" => Some(EventKind::Working),
        "PostCompact" => Some(EventKind::Enrichment),
        "Interrupt" => Some(EventKind::Interrupted),
        "Stop" => Some(EventKind::Completed),
        _ => None,
    }
}

fn build(
    id: &InvocationId,
    raw: &Value,
    kind: EventKind,
    evidence: EventEvidence,
) -> NormalizedAdapterEvent {
    let now = Utc::now();
    let status_reason = match kind {
        EventKind::WaitingApproval => status_reason_context(raw, false),
        EventKind::WaitingInput => status_reason_context(raw, true),
        EventKind::Completed => completed_reason_context(raw),
        _ => None,
    };
    let turn_id = raw
        .get("turn_id")
        .and_then(Value::as_str)
        .and_then(|v| sanitize_bounded(v, 128));
    NormalizedAdapterEvent {
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
            provider: "codex".into(),
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
            provider_metadata: provider_metadata(raw, None),
            usage: None,
            turn_id,
            tool_activity: tool_activity_update("codex", raw),
        },
        status_reason,
        collection_context: collection_context(raw),
    }
}

fn collection_context(raw: &Value) -> Option<ArtifactCollectionContext> {
    Some(ArtifactCollectionContext {
        adapter_identity: "codex".into(),
        provider_session_id: raw.get("session_id")?.as_str()?.trim().to_owned(),
        locator: PathBuf::from(raw.get("transcript_path")?.as_str()?),
    })
    .filter(|context| {
        !context.provider_session_id.is_empty() && !context.locator.as_os_str().is_empty()
    })
}

const MAX_SCAN_BYTES: u64 = 64 * 1024 * 1024;
const MAX_LINE_BYTES: usize = 1024 * 1024;

struct CodexCursor {
    _device: u64,
    _inode: u64,
    _stable_len: u64,
}

fn collect(request: CollectSessionDataRequest) -> Result<(SessionEnrichment, CodexCursor)> {
    check_cancelled(&request)?;
    let root = fs::canonicalize(request.home.join(".codex/sessions"))
        .context("Codex artifact root is unavailable")?;
    let canonical = validate_path(&root, &request.locator)?;
    check_cancelled(&request)?;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&canonical)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > MAX_SCAN_BYTES {
        bail!("Codex artifact is not a bounded regular file");
    }
    let mut session_bound = false;
    let mut latest = None;
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
            bail!("Codex artifact record exceeds limit");
        }
        if line.last() != Some(&b'\n') {
            break;
        }
        let value: Value = serde_json::from_slice(&line).context("malformed Codex record")?;
        let Some(record_type) = value.get("type").and_then(Value::as_str) else {
            continue;
        };
        let Some(payload) = value.get("payload") else {
            continue;
        };
        if record_type == "session_meta" {
            let matches = ["session_id", "id"].into_iter().any(|field| {
                payload.get(field).and_then(Value::as_str)
                    == Some(request.key.provider_session_id.as_str())
            });
            if !matches {
                bail!("Codex artifact session mismatch");
            }
            session_bound = true;
            session_name = ["session_name", "title"]
                .into_iter()
                .find_map(|field| payload.get(field).and_then(Value::as_str))
                .and_then(|value| sanitize_bounded(value, 160));
            continue;
        }
        if record_type != "event_msg"
            || payload.get("type").and_then(Value::as_str) != Some("token_count")
        {
            continue;
        }
        let Some(info) = payload.get("info").filter(|value| !value.is_null()) else {
            continue;
        };
        let (Some(total), Some(last)) =
            (info.get("total_token_usage"), info.get("last_token_usage"))
        else {
            continue;
        };
        let (Some(input), Some(output), Some(context)) = (
            optional_u64(total, "input_tokens")?,
            optional_u64(total, "output_tokens")?,
            optional_u64(last, "total_tokens")?,
        ) else {
            continue;
        };
        let percent = match optional_u64(info, "model_context_window")? {
            Some(window) if window > 0 => Some(percent(context, window)?),
            _ => None,
        };
        latest = Some(Usage {
            input_tokens: Some(input),
            output_tokens: Some(output),
            context_tokens: Some(context),
            context_window_percent: percent,
        });
    }
    check_cancelled(&request)?;
    if !session_bound {
        bail!("Codex artifact did not bind requested session");
    }
    let after = reader.get_ref().metadata()?;
    if after.dev() != metadata.dev()
        || after.ino() != metadata.ino()
        || after.len() < metadata.len()
    {
        bail!("Codex artifact changed identity during collection");
    }
    Ok((
        SessionEnrichment {
            session_name,
            usage: latest,
        },
        CodexCursor {
            _device: metadata.dev(),
            _inode: metadata.ino(),
            _stable_len: metadata.len(),
        },
    ))
}

fn validate_path(root: &Path, locator: &Path) -> Result<PathBuf> {
    let unresolved = if locator.is_absolute() {
        locator.to_path_buf()
    } else {
        root.join(locator)
    };
    if fs::symlink_metadata(&unresolved)?.file_type().is_symlink() {
        bail!("Codex artifact must not be a symlink");
    }
    let canonical = fs::canonicalize(unresolved)?;
    if !canonical.starts_with(root) {
        bail!("Codex artifact escapes allowed root");
    }
    if canonical.extension().and_then(|value| value.to_str()) != Some("jsonl") {
        bail!("Codex artifact must be JSONL");
    }
    Ok(canonical)
}

fn optional_u64(value: &Value, field: &str) -> Result<Option<u64>> {
    value
        .get(field)
        .map(|value| value.as_u64().context("invalid Codex usage value"))
        .transpose()
}

fn percent(value: u64, window: u64) -> Result<u8> {
    let rounded = u128::from(value)
        .checked_mul(100)
        .and_then(|value| value.checked_add(u128::from(window) / 2))
        .context("Codex context percentage overflow")?
        / u128::from(window);
    Ok(u8::try_from(rounded.min(100))?)
}

fn check_cancelled(request: &CollectSessionDataRequest) -> Result<()> {
    if request.cancellation.is_cancelled() {
        bail!("collection cancelled");
    }
    Ok(())
}

#[cfg(test)]
mod collection_tests {
    use super::*;
    use crate::{CollectionCancellation, ProviderSessionKey};
    use std::{fs, os::unix::fs::symlink};

    fn fixture(session: &str, rows: &[&str]) -> (tempfile::TempDir, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join(".codex/sessions/2026/08/31");
        fs::create_dir_all(&root).unwrap();
        let path = root.join(format!("rollout-{session}.jsonl"));
        fs::write(&path, rows.join("\n") + "\n").unwrap();
        (temp, path)
    }

    fn request(
        temp: &tempfile::TempDir,
        session: &str,
        locator: PathBuf,
    ) -> CollectSessionDataRequest {
        CollectSessionDataRequest {
            home: temp.path().to_path_buf(),
            key: ProviderSessionKey {
                configured_provider: "codex".into(),
                adapter_identity: "codex".into(),
                provider_session_id: session.into(),
            },
            locator,
            prior_cursor: None,
            cancellation: CollectionCancellation::default(),
        }
    }

    #[test]
    fn collector_uses_latest_cumulative_snapshot_and_metadata() {
        let meta = r#"{"type":"session_meta","payload":{"session_id":"s1","title":"Work"}}"#;
        let first = r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":10,"output_tokens":2},"last_token_usage":{"total_tokens":5},"model_context_window":100}}}"#;
        let last = r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":40,"output_tokens":8},"last_token_usage":{"total_tokens":26},"model_context_window":100}}}"#;
        let (temp, path) = fixture("s1", &[meta, first, last]);
        let (enrichment, _) = collect(request(&temp, "s1", path)).unwrap();
        assert_eq!(enrichment.session_name.as_deref(), Some("Work"));
        assert_eq!(
            enrichment.usage.unwrap(),
            Usage {
                input_tokens: Some(40),
                output_tokens: Some(8),
                context_tokens: Some(26),
                context_window_percent: Some(26)
            }
        );
    }

    #[test]
    fn collector_rejects_escape_symlink_mismatch_malformed_overflow_and_cancellation() {
        let (temp, malformed) = fixture("s2", &["not-json"]);
        assert!(collect(request(&temp, "s2", malformed)).is_err());
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
        let link = temp
            .path()
            .join(".codex/sessions/2026/08/31/rollout-s2.jsonl");
        fs::remove_file(&link).unwrap();
        symlink(&outside, &link).unwrap();
        assert!(collect(request(&temp, "s2", link)).is_err());

        let (wrong_home, wrong) = fixture(
            "s3",
            &[r#"{"type":"session_meta","payload":{"session_id":"other"}}"#],
        );
        assert!(collect(request(&wrong_home, "s3", wrong)).is_err());

        let huge = u64::MAX;
        let overflow_row = serde_json::json!({"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1,"output_tokens":1},"last_token_usage":{"total_tokens":huge},"model_context_window":1}}}).to_string();
        let (overflow_home, overflow) = fixture(
            "s4",
            &[
                r#"{"type":"session_meta","payload":{"session_id":"s4"}}"#,
                &overflow_row,
            ],
        );
        assert!(collect(request(&overflow_home, "s4", overflow)).is_ok());

        let (cancel_home, cancel_path) = fixture(
            "s5",
            &[r#"{"type":"session_meta","payload":{"session_id":"s5"}}"#],
        );
        let cancelled = request(&cancel_home, "s5", cancel_path);
        cancelled.cancellation.cancel();
        assert!(collect(cancelled).is_err());
    }
}
