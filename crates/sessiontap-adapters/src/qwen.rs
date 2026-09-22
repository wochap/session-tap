use crate::{
    CollectSessionDataRequest, LaunchPreparation, NormalizeContext, OpaqueCursor,
    SessionEnrichment, SetupAction, SetupReport, SideChannelSource, ToolDetailPolicy,
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
use chrono::{DateTime, Utc};
use serde_json::Value;
use sessiontap_core::ProviderId;
use sessiontap_core::domain::{
    ArtifactCollectionContext, EventKind, ProviderMetadata, TOOL_CORRELATION_ID_MAX_CHARS,
    ToolActivityPhase, ToolActivityUpdate, Usage,
};
use std::{
    collections::HashMap,
    fs::File,
    io::{BufRead, BufReader, Seek, SeekFrom},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    process::Command,
    sync::{Mutex, OnceLock},
};

const SIDE_CHANNEL_MAX_LINE: usize = 64 * 1024;

pub struct QwenJsonlTail {
    path: PathBuf,
    offset: u64,
    pending: String,
    max_line: usize,
    identity: Option<(u64, u64)>,
}

impl QwenJsonlTail {
    #[must_use]
    pub fn new(path: PathBuf, max_line: usize) -> Self {
        Self {
            path,
            offset: 0,
            pending: String::new(),
            max_line,
            identity: None,
        }
    }
}

impl SideChannelSource for QwenJsonlTail {
    fn poll(&mut self) -> Result<Vec<Value>> {
        let file = match File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
            Err(error) => return Err(error.into()),
        };
        let metadata = file.metadata()?;
        let identity = (metadata.dev(), metadata.ino());
        if metadata.len() < self.offset || self.identity.is_some_and(|old| old != identity) {
            self.offset = 0;
            self.pending.clear();
        }
        self.identity = Some(identity);
        let mut reader = BufReader::new(file);
        reader.seek(SeekFrom::Start(self.offset))?;
        let mut values = vec![];
        let mut line = String::new();
        while reader.read_line(&mut line)? > 0 {
            self.offset = reader.stream_position()?;
            self.pending.push_str(&line);
            if self.pending.len() > self.max_line {
                bail!("side-channel line exceeds limit");
            }
            if self.pending.ends_with('\n') {
                values.push(serde_json::from_str(self.pending.trim_end())?);
                self.pending.clear();
            }
            line.clear();
        }
        Ok(values)
    }
}

pub fn qwen_has_user_side_channel(args: &[String]) -> bool {
    args.iter().any(|argument| {
        argument == "--json-file"
            || argument == "--json-fd"
            || argument.starts_with("--json-file=")
            || argument.starts_with("--json-fd=")
    })
}

fn probe_qwen_dual_output(executable: &Path) -> bool {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, bool>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(value) = cache.lock().expect("probe cache poisoned").get(executable) {
        return *value;
    }
    let supported = Command::new(executable)
        .arg("--help")
        .output()
        .ok()
        .is_some_and(|output| String::from_utf8_lossy(&output.stdout).contains("--json-file"));
    cache
        .lock()
        .expect("probe cache poisoned")
        .insert(executable.to_path_buf(), supported);
    supported
}
pub const HOOK_EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "PostToolUseFailure",
    "PermissionRequest",
    "Notification",
    "Stop",
    "StopFailure",
    "SessionEnd",
];
pub type QwenAdapter = HookAdapter<QwenDialect, QwenCollector>;

#[allow(non_upper_case_globals)]
pub const QwenAdapter: QwenAdapter =
    HookAdapter::new(QwenDialect, QwenCollector, setup).with_launch(prepare_launch);

fn setup(home: &Path, executable: &Path, action: SetupAction) -> Result<SetupReport> {
    merge_hook_config(
        &home.join(".qwen/settings.json"),
        "qwen",
        HOOK_EVENTS,
        executable,
        action,
    )
}

fn prepare_launch(
    args: &[String],
    private_dir: &Path,
    executable: &Path,
) -> Result<LaunchPreparation> {
    if qwen_has_user_side_channel(args) || !probe_qwen_dual_output(executable) {
        return Ok(LaunchPreparation::default());
    }
    let path = private_dir.join("qwen-events.jsonl");
    Ok(LaunchPreparation {
        extra_args: vec!["--json-file".into(), path.to_string_lossy().into_owned()],
        environment: vec![],
        side_channel: Some(Box::new(QwenJsonlTail::new(path, SIDE_CHANNEL_MAX_LINE))),
    })
}

const TOOL_CORRELATION_FIELDS: &[&str] = &["tool_use_id", "tool_call_id"];
const TOOL_DETAIL: ToolDetailPolicy = ToolDetailPolicy {
    described_tools: &["run_shell_command", "shell_command"],
    path_tools: &["read_file", "write_file", "replace"],
    path_fields: &["file_path", "path"],
};

pub struct QwenDialect;

impl HookDialect for QwenDialect {
    fn id(&self) -> ProviderId {
        ProviderId::Qwen
    }
    fn classify(&self, raw: &Value) -> Option<EventKind> {
        classify(raw)
    }
    fn observed_at(&self, raw: &Value) -> Option<DateTime<Utc>> {
        raw.get("timestamp")
            .and_then(Value::as_str)
            .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
            .map(|value| value.with_timezone(&Utc))
    }
    fn start_reason(&self, raw: &Value) -> Option<String> {
        bounded_field(raw, &["source", "reason", "start_reason"], 32)
            .filter(|v| matches!(v.as_str(), "startup" | "clear" | "resume" | "compact"))
    }
    fn metadata(&self, raw: &Value) -> Option<ProviderMetadata> {
        provider_metadata(raw, None)
    }
    fn inline_usage(&self, raw: &Value) -> Option<Usage> {
        let usage = Usage {
            input_tokens: raw.get("input_tokens").and_then(Value::as_u64),
            output_tokens: raw.get("output_tokens").and_then(Value::as_u64),
            context_tokens: raw.get("context_tokens").and_then(Value::as_u64),
            context_window_percent: raw
                .get("context_usage")
                .and_then(Value::as_f64)
                .filter(|v| v.is_finite() && *v >= 0.0 && *v <= 1.0)
                .map(|v| (v * 100.0).round() as u8),
        };
        (usage != Usage::default()).then_some(usage)
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
            adapter_identity: ProviderId::Qwen,
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
pub struct QwenCollector;

impl SessionCollector for QwenCollector {
    fn collect(&self, request: &CollectSessionDataRequest) -> Result<Collected, CollectError> {
        Ok(scan(request).context("Qwen collection")?)
    }
}

fn scan(request: &CollectSessionDataRequest) -> Result<Collected> {
    check_cancelled(request)?;
    let canonical = validate_under_root(
        &request.home.join(".qwen/projects"),
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
    let mut percent = None;
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
            bail!("Qwen artifact record exceeds limit");
        }
        if line.last() != Some(&b'\n') {
            break;
        }
        let value: Value = serde_json::from_slice(&line).context("malformed Qwen record")?;
        if let Some(session) = value.get("sessionId").and_then(Value::as_str)
            && session != request.key.provider_session_id
        {
            bail!("Qwen artifact session mismatch");
        }
        if let Some(name) = ["sessionName", "title"]
            .into_iter()
            .find_map(|field| value.get(field).and_then(Value::as_str))
            .and_then(|value| sanitize_bounded(value, 160))
        {
            session_name = Some(name);
        }
        if value.get("type").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let Some(usage) = value.get("usageMetadata") else {
            continue;
        };
        let (Some(row_input), Some(row_output)) = (
            optional_u64(usage, "promptTokenCount")?,
            optional_u64(usage, "candidatesTokenCount")?,
        ) else {
            continue;
        };
        usage_observed = true;
        input = input
            .checked_add(row_input)
            .context("Qwen cumulative input overflow")?;
        output = output
            .checked_add(row_output)
            .context("Qwen cumulative output overflow")?;
        context = Some(row_input);
        percent = match optional_u64(&value, "contextWindowSize")? {
            Some(window) if window > 0 => Some(context_percent(row_input, window)?),
            _ => None,
        };
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
                context_window_percent: percent,
            }),
        },
        cursor: OpaqueCursor::new(cursor),
    })
}

fn context_percent(value: u64, window: u64) -> Result<u8> {
    let rounded = u128::from(value)
        .checked_mul(100)
        .and_then(|value| value.checked_add(u128::from(window) / 2))
        .context("Qwen context percentage overflow")?
        / u128::from(window);
    Ok(u8::try_from(rounded.min(100))?)
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
        "UserPromptSubmit"
            if raw
                .get("prompt")
                .and_then(Value::as_str)
                .is_some_and(|prompt| prompt.trim().is_empty()) =>
        {
            Some(EventKind::Enrichment)
        }
        "UserPromptSubmit" => Some(EventKind::NewTurn),
        "PreToolUse" if ask_user => Some(EventKind::WaitingInput),
        "PreToolUse" | "PostToolUse" | "PostToolUseFailure" => Some(EventKind::Working),
        "PermissionRequest" if ask_user => Some(EventKind::WaitingInput),
        "PermissionRequest" => Some(EventKind::WaitingApproval),
        "Notification" => match raw.get("notification_type").and_then(Value::as_str) {
            Some("permission_prompt") => Some(EventKind::WaitingApproval),
            Some("elicitation_dialog" | "agent_needs_input") => Some(EventKind::WaitingInput),
            Some("idle_prompt") => Some(EventKind::Idle),
            _ => None,
        },
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
    use std::{fs, os::unix::fs::symlink};

    fn collect(
        request: CollectSessionDataRequest,
    ) -> Result<(SessionEnrichment, OpaqueCursor), CollectError> {
        match QwenCollector.collect(&request)? {
            Collected::Complete { enrichment, cursor } => Ok((enrichment, cursor)),
            Collected::Unchanged { .. } => panic!("unexpected unchanged outcome"),
        }
    }

    fn fixture(session: &str, rows: &[&str]) -> (tempfile::TempDir, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join(".qwen/projects/project/chats");
        fs::create_dir_all(&root).unwrap();
        let path = root.join(format!("{session}.jsonl"));
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
                configured_provider: "qwen".into(),
                adapter_identity: ProviderId::Qwen,
                provider_session_id: session.into(),
            },
            locator,
            prior_cursor: None,
            cancellation: CollectionCancellation::default(),
        }
    }

    #[test]
    fn collector_sums_assistant_usage_and_excludes_telemetry() {
        let first = r#"{"sessionId":"s1","type":"assistant","title":"Work","usageMetadata":{"promptTokenCount":80,"candidatesTokenCount":5},"contextWindowSize":100}"#;
        let telemetry = r#"{"sessionId":"s1","type":"system","subtype":"ui_telemetry","usageMetadata":{"promptTokenCount":800}}"#;
        let last = r#"{"sessionId":"s1","type":"assistant","usageMetadata":{"promptTokenCount":20,"candidatesTokenCount":7},"contextWindowSize":100}"#;
        let (temp, path) = fixture("s1", &[first, telemetry, last]);
        let (enrichment, _) = collect(request(&temp, "s1", path)).unwrap();
        assert_eq!(enrichment.session_name.as_deref(), Some("Work"));
        assert_eq!(
            enrichment.usage.unwrap(),
            Usage {
                input_tokens: Some(100),
                output_tokens: Some(12),
                context_tokens: Some(20),
                context_window_percent: Some(20)
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
        let link = temp.path().join(".qwen/projects/project/chats/s2.jsonl");
        fs::remove_file(&link).unwrap();
        symlink(&outside, &link).unwrap();
        assert!(collect(request(&temp, "s2", link)).is_err());

        let (wrong_home, wrong) = fixture(
            "s3",
            &[
                r#"{"sessionId":"other","type":"assistant","usageMetadata":{"promptTokenCount":1,"candidatesTokenCount":1}}"#,
            ],
        );
        assert!(collect(request(&wrong_home, "s3", wrong)).is_err());

        let max = u64::MAX;
        let first = serde_json::json!({"sessionId":"s4","type":"assistant","usageMetadata":{"promptTokenCount":max,"candidatesTokenCount":1}}).to_string();
        let second = r#"{"sessionId":"s4","type":"assistant","usageMetadata":{"promptTokenCount":1,"candidatesTokenCount":1}}"#;
        let (overflow_home, overflow) = fixture("s4", &[&first, second]);
        assert!(collect(request(&overflow_home, "s4", overflow)).is_err());

        let (cancel_home, cancel_path) = fixture("s5", &["{}"]);
        let cancelled = request(&cancel_home, "s5", cancel_path);
        cancelled.cancellation.cancel();
        assert!(matches!(collect(cancelled), Err(CollectError::Cancelled)));
    }

    #[tokio::test]
    async fn unchanged_cursor_skips_scan_and_growth_forces_rescan() {
        let row = r#"{"sessionId":"s1","type":"assistant","usageMetadata":{"promptTokenCount":4,"candidatesTokenCount":1}}"#;
        let (temp, path) = fixture("s1", &[row]);
        let collect_with = |prior: Option<OpaqueCursor>| {
            let mut request = request(&temp, "s1", path.clone());
            request.prior_cursor = prior;
            QwenAdapter.collect_session_data(request)
        };
        let CollectionOutcome::Complete { cursor, .. } = collect_with(None).await else {
            panic!("expected complete outcome");
        };
        assert!(matches!(
            collect_with(Some(cursor.clone())).await,
            CollectionOutcome::Unchanged { .. }
        ));
        fs::write(&path, format!("{row}\n{row}\n")).unwrap();
        let CollectionOutcome::Complete { enrichment, .. } = collect_with(Some(cursor)).await
        else {
            panic!("grown artifact must be rescanned");
        };
        assert_eq!(enrichment.usage.unwrap().input_tokens, Some(8));
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
                &QwenAdapter,
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
            "tool_name":"run_shell_command",
            "tool_use_id":"q1",
            "tool_input":{"command":"PRIVATE","description":"Run unit tests"}
        }));
        assert_eq!(shell.detail.as_deref(), Some("Run unit tests"));
        assert_eq!(shell.correlation_id.as_deref(), Some("q1"));
        let read = normalize(json!({
            "hook_event_name":"PreToolUse",
            "tool_name":"replace",
            "tool_use_id":"q2",
            "tool_input":{"path":"src.rs"}
        }));
        assert_eq!(read.detail.as_deref(), Some("src.rs"));
        let escaped = normalize(json!({
            "hook_event_name":"PreToolUse",
            "tool_name":"replace",
            "tool_use_id":"q3",
            "tool_input":{"path":"../src.rs"}
        }));
        assert!(escaped.detail.is_none());
        let unlisted = normalize(json!({
            "hook_event_name":"PreToolUse",
            "tool_name":"Bash",
            "tool_use_id":"q4",
            "tool_input":{"description":"Not allowlisted","file_path":"src.rs"}
        }));
        assert!(unlisted.detail.is_none());
    }

    #[test]
    fn supported_tool_phase_is_exact_and_result_free() {
        let id = InvocationId::new();
        let (payload, phase) = (
            json!({"hook_event_name":"PermissionRequest","tool_name":"run_shell_command","tool_call_id":"c","tool_input":{"command":"PRIVATE"}}),
            ToolActivityPhase::Attention,
        );
        let event = QwenAdapter.normalize(&id, &payload).unwrap().event;
        assert_eq!(event.tool_activity.as_ref().unwrap().phase, phase);
        assert!(!serde_json::to_string(&event).unwrap().contains("PRIVATE"));
    }
}

#[cfg(test)]
mod launch_tests {
    use super::*;
    use crate::AgentAdapter;
    use std::{fs, os::unix::fs::PermissionsExt};

    fn fake_executable(dir: &Path, name: &str, help: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(
            &path,
            format!("#!/bin/sh\ntouch \"$0.probed\"\necho '{help}'\n"),
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[test]
    fn alias_executable_is_probed_and_side_channel_is_tailed_through_the_trait() {
        let temp = tempfile::tempdir().unwrap();
        let executable = fake_executable(temp.path(), "company-qwen", "  --json-file <path>");
        let private = temp.path().join("private");
        fs::create_dir_all(&private).unwrap();
        let mut prep = QwenAdapter
            .prepare_launch(&["--yolo".into()], &private, &executable)
            .unwrap();
        assert!(temp.path().join("company-qwen.probed").exists());
        let events = private.join("qwen-events.jsonl");
        assert_eq!(
            prep.extra_args,
            vec![
                "--json-file".to_owned(),
                events.to_string_lossy().into_owned()
            ]
        );
        let source: &mut dyn SideChannelSource = prep.side_channel.as_deref_mut().unwrap();
        assert!(source.poll().unwrap().is_empty());
        fs::write(&events, "{\"hook_event_name\":\"Stop\"}\n").unwrap();
        assert_eq!(source.poll().unwrap()[0]["hook_event_name"], "Stop");
    }

    #[test]
    fn unsupported_or_user_configured_side_channel_is_left_alone() {
        let temp = tempfile::tempdir().unwrap();
        let plain = fake_executable(temp.path(), "old-qwen", "usage: qwen [options]");
        let prep = QwenAdapter
            .prepare_launch(&[], temp.path(), &plain)
            .unwrap();
        assert!(temp.path().join("old-qwen.probed").exists());
        assert!(prep.extra_args.is_empty() && prep.side_channel.is_none());

        let dual = fake_executable(temp.path(), "new-qwen", "--json-file");
        let prep = QwenAdapter
            .prepare_launch(&["--json-fd=4".into()], temp.path(), &dual)
            .unwrap();
        assert!(prep.extra_args.is_empty() && prep.side_channel.is_none());
    }
}
