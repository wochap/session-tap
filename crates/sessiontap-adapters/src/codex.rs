use crate::{
    CollectSessionDataRequest, NormalizeContext, OpaqueCursor, SessionEnrichment, SetupAction,
    SetupReport, ToolDetailPolicy,
    artifact::{
        ArtifactCursor, CollectError, Collected, SessionCollector, check_cancelled,
        open_bounded_nofollow, optional_u64, validate_under_root,
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
    fs::{self, File},
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
};

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
pub type CodexAdapter = HookAdapter<CodexDialect, CodexCollector>;

#[allow(non_upper_case_globals)]
pub const CodexAdapter: CodexAdapter = HookAdapter::new(CodexDialect, CodexCollector, setup);

fn setup(home: &Path, executable: &Path, action: SetupAction) -> Result<SetupReport> {
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

const TOOL_CORRELATION_FIELDS: &[&str] = &["tool_use_id"];
const TOOL_DETAIL: ToolDetailPolicy = ToolDetailPolicy {
    described_tools: &["Bash", "bash"],
    path_tools: &[
        "Read",
        "Write",
        "Edit",
        "read_file",
        "write_file",
        "edit_file",
    ],
    path_fields: &["file_path", "path"],
};

pub struct CodexDialect;

impl HookDialect for CodexDialect {
    fn id(&self) -> ProviderId {
        ProviderId::Codex
    }
    fn classify(&self, raw: &Value) -> Option<EventKind> {
        if is_subagent_payload(raw) {
            return None;
        }
        classify(raw)
    }
    fn start_reason(&self, raw: &Value) -> Option<String> {
        bounded_field(raw, &["source", "reason", "start_reason"], 32)
            .filter(|v| matches!(v.as_str(), "startup" | "clear" | "resume" | "compact"))
    }
    fn metadata(&self, raw: &Value) -> Option<ProviderMetadata> {
        provider_metadata(raw, None)
    }
    fn turn_id(&self, raw: &Value) -> Option<String> {
        raw.get("turn_id")
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
            adapter_identity: ProviderId::Codex,
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
const SESSION_NAME_MAX_CHARS: usize = 160;

/// Codex enrichment depends on two files, so its cursor covers both: the
/// rollout and, when readable, the session index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CodexCursor {
    rollout: ArtifactCursor,
    index: Option<ArtifactCursor>,
}

#[derive(Clone, Copy)]
pub struct CodexCollector;

impl SessionCollector for CodexCollector {
    fn collect(&self, request: &CollectSessionDataRequest) -> Result<Collected, CollectError> {
        Ok(scan(request).context("Codex collection")?)
    }
}

fn scan(request: &CollectSessionDataRequest) -> Result<Collected> {
    check_cancelled(request)?;
    let canonical = validate_under_root(
        &request.home.join(".codex/sessions"),
        &request.locator,
        None,
        "jsonl",
    )?;
    // Rollout files are named `rollout-<timestamp>-<session id>.jsonl`.
    if !canonical
        .file_stem()
        .and_then(|stem| stem.to_str())
        .and_then(|stem| stem.strip_suffix(request.key.provider_session_id.as_str()))
        .is_some_and(|prefix| prefix.ends_with('-'))
    {
        bail!("Codex artifact identity mismatch");
    }
    check_cancelled(request)?;
    let (file, metadata) = open_bounded_nofollow(&canonical, MAX_SCAN_BYTES)?;
    let index = match open_index(request) {
        Ok((file, metadata)) => Some((file, ArtifactCursor::from(&metadata))),
        Err(_) => {
            check_cancelled(request)?;
            None
        }
    };
    let cursor = CodexCursor {
        rollout: ArtifactCursor::from(&metadata),
        index: index.as_ref().map(|(_, cursor)| *cursor),
    };
    if request
        .prior_cursor
        .as_ref()
        .and_then(OpaqueCursor::downcast_ref::<CodexCursor>)
        == Some(&cursor)
    {
        return Ok(Collected::Unchanged {
            cursor: OpaqueCursor::new(cursor),
        });
    }
    let mut session_bound = false;
    let mut latest = None;
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
                .and_then(|value| sanitize_bounded(value, SESSION_NAME_MAX_CHARS));
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
    check_cancelled(request)?;
    if !session_bound {
        bail!("Codex artifact did not bind requested session");
    }
    cursor.rollout.ensure_stable(reader.get_ref())?;
    let index_name = match index {
        Some((file, index_cursor)) => match scan_index(request, file, index_cursor) {
            Ok(name) => name,
            Err(_) => {
                check_cancelled(request)?;
                None
            }
        },
        None => None,
    };
    Ok(Collected::Complete {
        enrichment: SessionEnrichment {
            session_name: index_name.or(session_name),
            usage: latest,
        },
        cursor: OpaqueCursor::new(cursor),
    })
}

fn open_index(request: &CollectSessionDataRequest) -> Result<(File, fs::Metadata)> {
    check_cancelled(request)?;
    let path = request.home.join(".codex/session_index.jsonl");
    if fs::symlink_metadata(&path)?.file_type().is_symlink() {
        bail!("Codex session index must not be a symlink");
    }
    open_bounded_nofollow(&path, MAX_SCAN_BYTES)
}

fn scan_index(
    request: &CollectSessionDataRequest,
    file: File,
    cursor: ArtifactCursor,
) -> Result<Option<String>> {
    let mut latest = None;
    let mut total = 0_u64;
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    loop {
        check_cancelled(request)?;
        line.clear();
        let count = reader.read_until(b'\n', &mut line)?;
        if count == 0 {
            break;
        }
        total = total
            .checked_add(u64::try_from(count)?)
            .context("Codex session index size overflow")?;
        if total > MAX_SCAN_BYTES {
            bail!("Codex session index exceeds limit");
        }
        if line.len() > MAX_LINE_BYTES {
            bail!("Codex session index record exceeds limit");
        }
        if line.last() != Some(&b'\n') {
            break;
        }
        let Ok(value) = serde_json::from_slice::<Value>(&line) else {
            continue;
        };
        if value.get("id").and_then(Value::as_str) != Some(request.key.provider_session_id.as_str())
        {
            continue;
        }
        if let Some(name) = value
            .get("thread_name")
            .and_then(Value::as_str)
            .and_then(|value| sanitize_bounded(value, SESSION_NAME_MAX_CHARS))
        {
            latest = Some(name);
        }
    }
    check_cancelled(request)?;
    cursor.ensure_stable(reader.get_ref())?;
    if reader.get_ref().metadata()?.len() > MAX_SCAN_BYTES {
        bail!("Codex session index changed identity during collection");
    }
    Ok(latest)
}

/// Codex does not track child agents, so a payload carrying a child-agent
/// identity is ignored before any root interpretation.
fn is_subagent_payload(raw: &Value) -> bool {
    raw.get("agent_id")
        .and_then(Value::as_str)
        .is_some_and(|id| !id.trim().is_empty())
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

fn percent(value: u64, window: u64) -> Result<u8> {
    let rounded = u128::from(value)
        .checked_mul(100)
        .and_then(|value| value.checked_add(u128::from(window) / 2))
        .context("Codex context percentage overflow")?
        / u128::from(window);
    Ok(u8::try_from(rounded.min(100))?)
}

#[cfg(test)]
mod collection_tests {
    use super::*;
    use crate::{AgentAdapter, CollectionCancellation, CollectionOutcome, ProviderSessionKey};
    use std::{fs, os::unix::fs::symlink};

    fn collect(
        request: CollectSessionDataRequest,
    ) -> Result<(SessionEnrichment, OpaqueCursor), CollectError> {
        match CodexCollector.collect(&request)? {
            Collected::Complete { enrichment, cursor } => Ok((enrichment, cursor)),
            Collected::Unchanged { .. } => panic!("unexpected unchanged outcome"),
        }
    }

    const USAGE_ROW: &str = r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":40,"output_tokens":8},"last_token_usage":{"total_tokens":26},"model_context_window":100}}}"#;

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
                adapter_identity: ProviderId::Codex,
                provider_session_id: session.into(),
            },
            locator,
            prior_cursor: None,
            cancellation: CollectionCancellation::default(),
        }
    }

    fn write_index(temp: &tempfile::TempDir, contents: impl AsRef<[u8]>) -> PathBuf {
        let path = temp.path().join(".codex/session_index.jsonl");
        fs::write(&path, contents).unwrap();
        path
    }

    fn assert_rollout_usage(enrichment: &SessionEnrichment) {
        assert_eq!(
            enrichment.usage,
            Some(Usage {
                input_tokens: Some(40),
                output_tokens: Some(8),
                context_tokens: Some(26),
                context_window_percent: Some(26),
            })
        );
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
    fn collector_uses_last_matching_sanitized_index_name_in_file_order() {
        let meta =
            r#"{"type":"session_meta","payload":{"session_id":"s1","title":"Rollout fallback"}}"#;
        let (temp, path) = fixture("s1", &[meta, USAGE_ROW]);
        let rows = [
            serde_json::json!({"id":"s1","thread_name":"Provisional","updated_at":"2099-01-01"}),
            serde_json::json!({"id":"s10","thread_name":"Prefix mismatch","updated_at":"2100-01-01"}),
            serde_json::json!({"id":"other","thread_name":"Interleaved","updated_at":"2100-01-01"}),
            serde_json::json!({"id":"s1","thread_name":"  Latest\nName \u{1b}[31mRed  ","updated_at":"1999-01-01"}),
        ];
        write_index(
            &temp,
            format!(
                "{}\n",
                rows.iter()
                    .map(Value::to_string)
                    .collect::<Vec<_>>()
                    .join("\n")
            ),
        );

        let (enrichment, _) = collect(request(&temp, "s1", path)).unwrap();
        assert_eq!(enrichment.session_name.as_deref(), Some("Latest Name Red"));
        assert_rollout_usage(&enrichment);
    }

    #[test]
    fn collector_falls_back_to_rollout_name_for_unavailable_or_invalid_index() {
        let assert_fallback = |temp: &tempfile::TempDir, path: PathBuf| {
            let (enrichment, _) = collect(request(temp, "s1", path)).unwrap();
            assert_eq!(enrichment.session_name.as_deref(), Some("Rollout fallback"));
            assert_rollout_usage(&enrichment);
        };
        let rollout = [
            r#"{"type":"session_meta","payload":{"session_id":"s1","title":"Rollout fallback"}}"#,
            USAGE_ROW,
        ];

        let (missing_home, missing_rollout) = fixture("s1", &rollout);
        assert_fallback(&missing_home, missing_rollout);

        let (malformed_home, malformed_rollout) = fixture("s1", &rollout);
        write_index(&malformed_home, b"not-json\n");
        assert_fallback(&malformed_home, malformed_rollout);

        let (unsafe_home, unsafe_rollout) = fixture("s1", &rollout);
        let outside = unsafe_home.path().join("outside.jsonl");
        fs::write(&outside, b"{}\n").unwrap();
        symlink(
            &outside,
            unsafe_home.path().join(".codex/session_index.jsonl"),
        )
        .unwrap();
        assert_fallback(&unsafe_home, unsafe_rollout);

        let (oversized_home, oversized_rollout) = fixture("s1", &rollout);
        let oversized = write_index(&oversized_home, b"{}");
        fs::OpenOptions::new()
            .write(true)
            .open(oversized)
            .unwrap()
            .set_len(MAX_SCAN_BYTES + 1)
            .unwrap();
        assert_fallback(&oversized_home, oversized_rollout);

        let (long_line_home, long_line_rollout) = fixture("s1", &rollout);
        write_index(&long_line_home, vec![b'x'; MAX_LINE_BYTES + 1]);
        assert_fallback(&long_line_home, long_line_rollout);
    }

    #[test]
    fn collector_ignores_incomplete_index_tail() {
        let meta =
            r#"{"type":"session_meta","payload":{"session_id":"s1","title":"Rollout fallback"}}"#;
        let (temp, path) = fixture("s1", &[meta, USAGE_ROW]);
        write_index(
            &temp,
            b"{\"id\":\"s1\",\"thread_name\":\"Complete\"}\n{\"id\":\"s1\",\"thread_name\":\"Partial",
        );

        let (enrichment, _) = collect(request(&temp, "s1", path)).unwrap();
        assert_eq!(enrichment.session_name.as_deref(), Some("Complete"));
        assert_rollout_usage(&enrichment);
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
        assert!(matches!(collect(cancelled), Err(CollectError::Cancelled)));
    }

    #[test]
    fn collector_rejects_rollout_named_for_another_session() {
        let meta = r#"{"type":"session_meta","payload":{"session_id":"s1"}}"#;
        let (temp, path) = fixture("s1", &[meta, USAGE_ROW]);
        assert!(collect(request(&temp, "s1", path.clone())).is_ok());
        let other = path.with_file_name("rollout-2026-08-31T00-00-00-other.jsonl");
        fs::rename(&path, &other).unwrap();
        let error = collect(request(&temp, "s1", other.clone())).err().unwrap();
        assert!(error.to_string().contains("identity mismatch"), "{error}");
        let prefixed = path.with_file_name("rollout-xs1.jsonl");
        fs::rename(&other, &prefixed).unwrap();
        assert!(collect(request(&temp, "s1", prefixed.clone())).is_err());
        let timestamped = path.with_file_name("rollout-2026-08-31T00-00-00-s1.jsonl");
        fs::rename(&prefixed, &timestamped).unwrap();
        assert!(collect(request(&temp, "s1", timestamped)).is_ok());
    }

    #[tokio::test]
    async fn unchanged_cursor_covers_rollout_and_session_index() {
        let meta = r#"{"type":"session_meta","payload":{"session_id":"s1","title":"Rollout"}}"#;
        let (temp, path) = fixture("s1", &[meta, USAGE_ROW]);
        let collect_with = |prior: Option<OpaqueCursor>| {
            let mut request = request(&temp, "s1", path.clone());
            request.prior_cursor = prior;
            CodexAdapter.collect_session_data(request)
        };
        let CollectionOutcome::Complete { cursor, .. } = collect_with(None).await else {
            panic!("expected complete outcome");
        };
        assert!(matches!(
            collect_with(Some(cursor.clone())).await,
            CollectionOutcome::Unchanged { .. }
        ));
        write_index(&temp, "{\"id\":\"s1\",\"thread_name\":\"Renamed\"}\n");
        let CollectionOutcome::Complete { enrichment, cursor } = collect_with(Some(cursor)).await
        else {
            panic!("a new session index must force a rescan");
        };
        assert_eq!(enrichment.session_name.as_deref(), Some("Renamed"));
        assert!(matches!(
            collect_with(Some(cursor.clone())).await,
            CollectionOutcome::Unchanged { .. }
        ));
        fs::write(&path, format!("{meta}\n{USAGE_ROW}\n{USAGE_ROW}\n")).unwrap();
        assert!(matches!(
            collect_with(Some(cursor)).await,
            CollectionOutcome::Complete { .. }
        ));
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
        let id = InvocationId::new();
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("src.rs"), "").unwrap();
        let normalize = |raw: serde_json::Value| {
            AgentAdapter::normalize_with_evidence(
                &CodexAdapter,
                &id,
                &raw,
                EventEvidence::managed_hook(1),
                &NormalizeContext {
                    workspace: Some(temp.path()),
                },
            )
            .unwrap()
            .into_event()
            .unwrap()
            .event
            .tool_activity
            .unwrap()
        };
        let shell = normalize(json!({
            "hook_event_name":"PreToolUse",
            "tool_name":"Bash",
            "tool_use_id":"c1",
            "tool_input":{"command":"PRIVATE","description":"Run unit tests"}
        }));
        assert_eq!(shell.detail.as_deref(), Some("Run unit tests"));
        assert_eq!(shell.correlation_id.as_deref(), Some("c1"));
        let read = normalize(json!({
            "hook_event_name":"PreToolUse",
            "tool_name":"read_file",
            "tool_use_id":"c2",
            "tool_input":{"path":"src.rs"}
        }));
        assert_eq!(read.detail.as_deref(), Some("src.rs"));
        let escaped = normalize(json!({
            "hook_event_name":"PreToolUse",
            "tool_name":"read_file",
            "tool_use_id":"c3",
            "tool_input":{"path":"../src.rs"}
        }));
        assert!(escaped.detail.is_none());
        let unlisted = normalize(json!({
            "hook_event_name":"PreToolUse",
            "tool_name":"Grep",
            "tool_use_id":"c4",
            "tool_input":{"description":"Not allowlisted","file_path":"src.rs"}
        }));
        assert!(unlisted.detail.is_none());
    }

    #[test]
    fn supported_tool_phase_is_exact_and_result_free() {
        let id = InvocationId::new();
        let (payload, phase) = (
            json!({"hook_event_name":"PostToolUse","tool_name":"Bash","tool_use_id":"b","tool_response":"PRIVATE"}),
            ToolActivityPhase::Finish,
        );
        let event = CodexAdapter.normalize(&id, &payload).unwrap().event;
        assert_eq!(event.tool_activity.as_ref().unwrap().phase, phase);
        assert!(!serde_json::to_string(&event).unwrap().contains("PRIVATE"));
    }
}
