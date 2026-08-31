use crate::{
    AgentAdapter, BoundedDiagnostic, CollectSessionDataRequest, CollectionOutcome,
    LaunchPreparation, OpaqueCursor, SessionEnrichment, SetupAction, SetupReport, bounded_field,
    completed_reason_context, failed_reason_context, is_subagent_payload, merge_hook_config,
    provider_metadata, sanitize_bounded, status_reason_context, tool_activity_update,
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
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Seek, SeekFrom},
    os::unix::{fs::MetadataExt, fs::OpenOptionsExt},
    path::{Path, PathBuf},
    process::Command,
    sync::{Mutex, OnceLock},
};
use uuid::Uuid;

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

    pub fn poll(&mut self) -> Result<Vec<Value>> {
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

fn probe_qwen_dual_output(executable: &str) -> bool {
    static CACHE: OnceLock<Mutex<HashMap<String, bool>>> = OnceLock::new();
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
        .insert(executable.to_owned(), supported);
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
pub struct QwenAdapter;

#[cfg(test)]
impl QwenAdapter {
    /// Test helper for cases that expect a normalized root event.
    pub fn normalize(&self, id: &InvocationId, raw: &Value) -> Result<NormalizedAdapterEvent> {
        match <Self as AgentAdapter>::normalize(self, id, raw)? {
            AdapterOutcome::Event(event) => Ok(*event),
            AdapterOutcome::Ignored => anyhow::bail!("ignored subagent hook"),
        }
    }
}

#[async_trait]
impl AgentAdapter for QwenAdapter {
    fn dialect(&self) -> &'static str {
        "qwen"
    }
    fn prepare_launch(&self, args: &[String], private_dir: &Path) -> Result<LaunchPreparation> {
        if qwen_has_user_side_channel(args) || !probe_qwen_dual_output("qwen") {
            return Ok(LaunchPreparation::default());
        }
        let path = private_dir.join("qwen-events.jsonl");
        Ok(LaunchPreparation {
            extra_args: vec!["--json-file".into(), path.to_string_lossy().into_owned()],
            environment: vec![],
            side_channel: Some(path),
        })
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
        let received_at = Utc::now();
        let observed_at = raw
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
            .map(|value| value.with_timezone(&Utc))
            .unwrap_or(received_at);
        let status_reason = match kind {
            EventKind::WaitingApproval => status_reason_context(raw, false),
            EventKind::WaitingInput => status_reason_context(raw, true),
            EventKind::Completed => completed_reason_context(raw),
            EventKind::Failed => failed_reason_context(raw),
            _ => None,
        };
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
        let usage = (usage != Usage::default()).then_some(usage);
        let turn_id = raw
            .get("turn_id")
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
                provider: "qwen".into(),
                observed_at,
                received_at,
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
                usage,
                turn_id,
                tool_activity: tool_activity_update("qwen", raw),
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
            &home.join(".qwen/settings.json"),
            "qwen",
            HOOK_EVENTS,
            executable,
            action,
        )
    }
}

fn collection_context(raw: &Value) -> Option<ArtifactCollectionContext> {
    Some(ArtifactCollectionContext {
        adapter_identity: "qwen".into(),
        provider_session_id: raw.get("session_id")?.as_str()?.trim().to_owned(),
        locator: PathBuf::from(raw.get("transcript_path")?.as_str()?),
    })
    .filter(|context| {
        !context.provider_session_id.is_empty() && !context.locator.as_os_str().is_empty()
    })
}

const MAX_SCAN_BYTES: u64 = 64 * 1024 * 1024;
const MAX_LINE_BYTES: usize = 1024 * 1024;

struct QwenCursor {
    _device: u64,
    _inode: u64,
    _stable_len: u64,
}

fn collect(request: CollectSessionDataRequest) -> Result<(SessionEnrichment, QwenCursor)> {
    check_cancelled(&request)?;
    let root = fs::canonicalize(request.home.join(".qwen/projects"))
        .context("Qwen artifact root is unavailable")?;
    let canonical = validate_path(&root, &request.locator, &request.key.provider_session_id)?;
    check_cancelled(&request)?;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&canonical)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > MAX_SCAN_BYTES {
        bail!("Qwen artifact is not a bounded regular file");
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
        check_cancelled(&request)?;
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
    check_cancelled(&request)?;
    let after = reader.get_ref().metadata()?;
    if after.dev() != metadata.dev()
        || after.ino() != metadata.ino()
        || after.len() < metadata.len()
    {
        bail!("Qwen artifact changed identity during collection");
    }
    Ok((
        SessionEnrichment {
            session_name,
            usage: usage_observed.then_some(Usage {
                input_tokens: Some(input),
                output_tokens: Some(output),
                context_tokens: context,
                context_window_percent: percent,
            }),
        },
        QwenCursor {
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
        bail!("Qwen artifact must not be a symlink");
    }
    let canonical = fs::canonicalize(unresolved)?;
    if !canonical.starts_with(root) {
        bail!("Qwen artifact escapes allowed root");
    }
    if canonical.extension().and_then(|value| value.to_str()) != Some("jsonl")
        || canonical.file_stem().and_then(|value| value.to_str()) != Some(session)
    {
        bail!("Qwen artifact identity mismatch");
    }
    Ok(canonical)
}

fn optional_u64(value: &Value, field: &str) -> Result<Option<u64>> {
    value
        .get(field)
        .map(|value| value.as_u64().context("invalid Qwen usage value"))
        .transpose()
}

fn context_percent(value: u64, window: u64) -> Result<u8> {
    let rounded = u128::from(value)
        .checked_mul(100)
        .and_then(|value| value.checked_add(u128::from(window) / 2))
        .context("Qwen context percentage overflow")?
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
#[allow(clippy::items_after_test_module)]
mod collection_tests {
    use super::*;
    use crate::{CollectionCancellation, ProviderSessionKey};
    use std::{fs, os::unix::fs::symlink};

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
                adapter_identity: "qwen".into(),
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
        assert!(collect(cancelled).is_err());
    }
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
