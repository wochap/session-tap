use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use fs2::FileExt;
use serde_json::{Value, json};
use sessiontap_core::{
    config::Config,
    domain::{
        ATTENTION_MAX_BYTES, ATTENTION_MAX_CHARS, AttentionContext, AttentionSource,
        FailureContext, InvocationId, NormalizedAdapterEvent, ProviderMetadata,
    },
};
use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process::Command,
    sync::{Mutex, OnceLock},
};

pub mod claude;
pub mod codex;
pub mod qwen;

pub const ADAPTER_API_VERSION: u32 = 1;

#[derive(Debug, Clone, Default)]
pub struct LaunchPreparation {
    pub extra_args: Vec<String>,
    pub environment: Vec<(String, String)>,
    pub side_channel: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetupAction {
    Ensure,
    Doctor,
    Remove,
}

#[derive(Debug, Clone)]
pub struct SetupReport {
    pub changed: bool,
    pub healthy: bool,
    pub message: String,
}

#[async_trait]
pub trait AgentAdapter: Send + Sync {
    fn api_version(&self) -> u32 {
        ADAPTER_API_VERSION
    }
    fn dialect(&self) -> &'static str;
    fn matches(&self, provider: &str) -> bool {
        provider == self.dialect()
    }
    fn redact_args(&self, args: &[String]) -> Vec<String> {
        redact_args(args)
    }
    fn prepare_launch(&self, _args: &[String], _private_dir: &Path) -> Result<LaunchPreparation> {
        Ok(LaunchPreparation::default())
    }
    fn normalize(
        &self,
        invocation_id: &InvocationId,
        raw: &Value,
    ) -> Result<NormalizedAdapterEvent>;
    async fn setup(
        &self,
        home: &Path,
        executable: &Path,
        action: SetupAction,
    ) -> Result<SetupReport>;
}

pub struct AdapterRegistry {
    adapters: HashMap<String, Box<dyn AgentAdapter>>,
    aliases: HashMap<String, (String, String)>,
}
impl AdapterRegistry {
    #[must_use]
    pub fn new(config: &Config) -> Self {
        let mut adapters: HashMap<String, Box<dyn AgentAdapter>> = HashMap::new();
        adapters.insert("claude".into(), Box::new(claude::ClaudeAdapter));
        adapters.insert("codex".into(), Box::new(codex::CodexAdapter));
        adapters.insert("qwen".into(), Box::new(qwen::QwenAdapter));
        let aliases = config
            .adapters
            .iter()
            .map(|(name, c)| (name.clone(), (c.inherits.clone(), c.executable.clone())))
            .collect();
        Self { adapters, aliases }
    }
    pub fn resolve(&self, provider: &str) -> Option<(&dyn AgentAdapter, String)> {
        if let Some(adapter) = self.adapters.get(provider) {
            return Some((adapter.as_ref(), provider.into()));
        }
        let (dialect, executable) = self.aliases.get(provider)?;
        Some((self.adapters.get(dialect)?.as_ref(), executable.clone()))
    }
}

pub fn redact_args(args: &[String]) -> Vec<String> {
    const FLAGS: &[&str] = &[
        "--api-key",
        "--token",
        "--password",
        "--secret",
        "--authorization",
        "-p",
    ];
    let mut out = Vec::with_capacity(args.len());
    let mut redact_next = false;
    for arg in args {
        if redact_next {
            out.push("[REDACTED]".into());
            redact_next = false;
            continue;
        }
        let lower = arg.to_ascii_lowercase();
        if FLAGS.contains(&lower.as_str()) {
            out.push(arg.clone());
            redact_next = true;
            continue;
        }
        if FLAGS.iter().any(|f| lower.starts_with(&format!("{f}="))) {
            out.push(format!(
                "{}=[REDACTED]",
                arg.split('=').next().unwrap_or_default()
            ));
        } else if lower.contains("sk-") || lower.contains("bearer ") {
            out.push("[REDACTED]".into());
        } else {
            out.push(arg.clone());
        }
    }
    out
}

#[cfg(any())]
fn normalize_provider(
    provider: &str,
    id: &InvocationId,
    raw: &Value,
) -> Result<NormalizedAdapterEvent> {
    let name = raw
        .get("hook_event_name")
        .or_else(|| raw.get("event_name"))
        .or_else(|| raw.get("type"))
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let lower = name.to_ascii_lowercase();
    let notification = raw
        .get("notification_type")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    let ask_user_question = matches!(provider, "claude" | "qwen")
        && lower.contains("permission")
        && raw
            .get("tool_name")
            .and_then(Value::as_str)
            .is_some_and(|name| {
                name.eq_ignore_ascii_case("AskUserQuestion")
                    || name.eq_ignore_ascii_case("ask_user_question")
            });
    let empty_qwen_prompt = provider == "qwen"
        && (lower.contains("userprompt") || lower.contains("promptsubmit"))
        && raw
            .get("prompt")
            .and_then(Value::as_str)
            .is_some_and(|prompt| prompt.trim().is_empty());
    let kind = if lower.contains("sessionstart") {
        EventKind::ProviderSessionStarted
    } else if lower.contains("sessionend") {
        EventKind::ProviderSessionEnded
    } else if empty_qwen_prompt {
        EventKind::Enrichment
    } else if lower.contains("userprompt")
        || lower.contains("promptsubmit")
        || lower.contains("turnstart")
    {
        EventKind::NewTurn
    } else if ask_user_question {
        EventKind::WaitingInput
    } else if lower.contains("permission") || notification.contains("permission_prompt") {
        EventKind::WaitingApproval
    } else if lower.contains("question")
        || lower.contains("elicitation")
        || lower.contains("needs_input")
        || lower.contains("userinputrequest")
        || notification.contains("needs_input")
    {
        EventKind::WaitingInput
    } else if lower.contains("pretool")
        || lower.contains("posttool")
        || lower.contains("toolstart")
        || lower.contains("toolend")
    {
        EventKind::Working
    } else if provider != "codex" && lower.contains("failure") {
        EventKind::Failed
    } else if lower == "stop" || lower.contains("turnend") {
        EventKind::Completed
    } else {
        EventKind::Enrichment
    };
    let usage_source = (provider == "qwen").then_some(raw);
    let usage = usage_source
        .map(|u| Usage {
            input_tokens: u.get("input_tokens").and_then(Value::as_u64),
            output_tokens: u.get("output_tokens").and_then(Value::as_u64),
            context_tokens: u.get("context_tokens").and_then(Value::as_u64),
            context_window_percent: (provider == "qwen")
                .then(|| {
                    u.get("context_usage")?
                        .as_f64()
                        .filter(|v| v.is_finite() && *v >= 0.0 && *v <= 1.0)
                        .map(|v| (v * 100.0).round() as u8)
                })
                .flatten(),
        })
        .filter(|u| {
            u.input_tokens.is_some()
                || u.output_tokens.is_some()
                || u.context_tokens.is_some()
                || u.context_window_percent.is_some()
        });
    let attention = match kind {
        EventKind::WaitingApproval => attention_context(raw, false),
        EventKind::WaitingInput => attention_context(raw, true),
        _ => None,
    };
    let failure = (kind == EventKind::Failed).then(|| failure_context(raw));
    let provider_session_name = (provider == "claude")
        .then(|| claude_session_name(raw))
        .flatten();
    let received_at = Utc::now();
    let observed_at = (provider == "qwen")
        .then(|| {
            raw.get("timestamp")
                .and_then(Value::as_str)
                .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
                .map(|value| value.with_timezone(&Utc))
        })
        .flatten()
        .unwrap_or(received_at);
    Ok(NormalizedAdapterEvent {
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
            provider: provider.into(),
            observed_at,
            received_at,
            source: "hook".into(),
            kind: kind.clone(),
            provider_session_id: raw
                .get("session_id")
                .and_then(Value::as_str)
                .map(str::to_owned),
            provider_session_name,
            provider_session_start_reason: (kind == EventKind::ProviderSessionStarted)
                .then(|| bounded_field(raw, &["source", "reason", "start_reason"], 32))
                .flatten()
                .filter(|value| {
                    matches!(value.as_str(), "startup" | "clear" | "resume" | "compact")
                }),
            provider_metadata: provider_metadata(
                raw,
                (provider == "claude").then_some("prompt_id"),
            ),
            usage,
            turn_id: raw
                .get("turn_id")
                .or_else(|| {
                    (provider == "claude")
                        .then(|| raw.get("prompt_id"))
                        .flatten()
                })
                .and_then(Value::as_str)
                .and_then(|value| sanitize_bounded(value, 128)),
        },
        attention,
        failure,
    })
}

pub(crate) fn sanitize_bounded(value: &str, max_chars: usize) -> Option<String> {
    let mut escaped = false;
    let clean = value
        .chars()
        .filter_map(|c| {
            if c == '\u{1b}' {
                escaped = true;
                return None;
            }
            if escaped {
                if c.is_ascii_alphabetic() {
                    escaped = false;
                }
                return None;
            }
            Some(if c.is_control() { ' ' } else { c })
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    (!clean.is_empty() && clean.chars().count() <= max_chars).then_some(clean)
}

pub(crate) fn bounded_field(raw: &Value, names: &[&str], max_chars: usize) -> Option<String> {
    names
        .iter()
        .find_map(|name| raw.get(name).and_then(Value::as_str))
        .and_then(|value| sanitize_bounded(value, max_chars))
}

pub(crate) fn provider_metadata(
    raw: &Value,
    alternate_turn_field: Option<&str>,
) -> Option<ProviderMetadata> {
    let model = bounded_field(raw, &["model"], 160);
    let effort = raw
        .get("effort")
        .and_then(|v| v.get("level").or(Some(v)))
        .and_then(Value::as_str)
        .filter(|v| matches!(*v, "low" | "medium" | "high" | "max" | "xhigh"))
        .map(str::to_owned);
    let permission_mode = bounded_field(raw, &["permission_mode"], 32).filter(|v| {
        matches!(
            v.as_str(),
            "default" | "acceptEdits" | "auto" | "plan" | "yolo" | "bypassPermissions"
        )
    });
    let current_turn_id = raw
        .get("turn_id")
        .or_else(|| alternate_turn_field.and_then(|field| raw.get(field)))
        .and_then(Value::as_str)
        .and_then(|v| sanitize_bounded(v, 128));
    let metadata = ProviderMetadata {
        model,
        effort,
        permission_mode,
        current_turn_id,
    };
    (metadata != ProviderMetadata::default()).then_some(metadata)
}

pub(crate) fn claude_session_name(raw: &Value) -> Option<String> {
    let path = raw.get("transcript_path")?.as_str()?;
    latest_transcript_title(Path::new(path)).ok().flatten()
}

fn latest_transcript_title(path: &Path) -> Result<Option<String>> {
    const CHUNK_BYTES: usize = 8 * 1024;
    const MAX_LINE_BYTES: usize = 64 * 1024;

    let mut file = File::open(path)?;
    let mut cursor = file.metadata()?.len();
    let mut reversed_line = Vec::new();
    let mut line_too_long = false;

    while cursor > 0 {
        let amount = usize::try_from(cursor.min(CHUNK_BYTES as u64))?;
        cursor -= amount as u64;
        file.seek(SeekFrom::Start(cursor))?;
        let mut chunk = vec![0; amount];
        std::io::Read::read_exact(&mut file, &mut chunk)?;

        for byte in chunk.into_iter().rev() {
            if byte == b'\n' {
                if !line_too_long {
                    reversed_line.reverse();
                    if let Some(title) = title_from_transcript_line(&reversed_line) {
                        return Ok(Some(title));
                    }
                }
                reversed_line.clear();
                line_too_long = false;
            } else if reversed_line.len() < MAX_LINE_BYTES {
                reversed_line.push(byte);
            } else {
                line_too_long = true;
            }
        }
    }

    if !line_too_long {
        reversed_line.reverse();
        if let Some(title) = title_from_transcript_line(&reversed_line) {
            return Ok(Some(title));
        }
    }
    Ok(None)
}

fn title_from_transcript_line(line: &[u8]) -> Option<String> {
    if !line
        .windows(b"aiTitle".len())
        .any(|part| part == b"aiTitle")
        && !line
            .windows(b"customTitle".len())
            .any(|part| part == b"customTitle")
    {
        return None;
    }
    let value: Value = serde_json::from_slice(line).ok()?;
    ["customTitle", "aiTitle"]
        .into_iter()
        .find_map(|key| value.get(key).and_then(Value::as_str))
        .and_then(bounded_one_line)
}

fn bounded_one_line(value: &str) -> Option<String> {
    let mut out = String::new();
    let mut spaced = false;
    for ch in value.chars().filter(|ch| !ch.is_control()) {
        if ch.is_whitespace() {
            spaced = !out.is_empty();
            continue;
        }
        if spaced && !out.ends_with(' ') {
            out.push(' ');
        }
        spaced = false;
        if out.chars().count() >= ATTENTION_MAX_CHARS
            || out.len() + ch.len_utf8() > ATTENTION_MAX_BYTES
        {
            break;
        }
        out.push(ch);
    }
    let out = out.trim().to_owned();
    (!out.is_empty()).then_some(out)
}

fn field<'a>(raw: &'a Value, names: &[&str]) -> Option<&'a str> {
    names
        .iter()
        .find_map(|name| raw.get(*name).and_then(Value::as_str))
}

pub(crate) fn attention_context(raw: &Value, input: bool) -> Option<AttentionContext> {
    let args = raw
        .get("tool_input")
        .or_else(|| raw.get("arguments"))
        .or_else(|| raw.get("input"));
    let description = field(raw, &["description", "message"])
        .and_then(bounded_one_line)
        .or_else(|| {
            args.and_then(|value| field(value, &["description"]))
                .and_then(bounded_one_line)
        });
    if let Some(summary) = description {
        return Some(AttentionContext {
            summary,
            source: AttentionSource::Description,
        });
    }
    let tool = field(raw, &["tool_name", "tool", "name"]);
    if input {
        let question = field(raw, &["question", "prompt"])
            .or_else(|| args.and_then(|v| field(v, &["question", "prompt", "message"])))
            .or_else(|| {
                args.and_then(|v| {
                    v.get("questions")?
                        .as_array()?
                        .first()?
                        .get("question")?
                        .as_str()
                })
            });
        if let Some(summary) = question.and_then(bounded_one_line) {
            return Some(AttentionContext {
                summary,
                source: AttentionSource::Question,
            });
        }
    }
    if let Some(tool) = tool {
        let lower = tool.to_ascii_lowercase();
        if lower.contains("patch") {
            return Some(AttentionContext {
                summary: "Apply a patch".into(),
                source: AttentionSource::ToolSummary,
            });
        }
        if ["read", "write", "edit", "glob", "grep"]
            .iter()
            .any(|name| lower == *name || lower.ends_with(&format!("__{name}")))
        {
            if let Some(path) = args.and_then(|v| field(v, &["file_path", "path"])) {
                let base = Path::new(path)
                    .file_name()
                    .and_then(|v| v.to_str())
                    .unwrap_or("file");
                return bounded_one_line(&format!("{tool}: {base}")).map(|summary| {
                    AttentionContext {
                        summary,
                        source: AttentionSource::ToolSummary,
                    }
                });
            }
        }
        if ["bash", "shell", "command", "exec_command"]
            .iter()
            .any(|name| lower.contains(name))
        {
            if let Some(command) = args.and_then(|v| field(v, &["command", "cmd"])) {
                let suspicious = [
                    "token",
                    "secret",
                    "password",
                    "authorization",
                    "api_key",
                    "api-key",
                    "bearer",
                    "sk-",
                ]
                .iter()
                .any(|needle| command.to_ascii_lowercase().contains(needle));
                let summary = if suspicious {
                    Some("Run a shell command".into())
                } else {
                    bounded_one_line(command)
                };
                return summary.map(|summary| AttentionContext {
                    summary,
                    source: AttentionSource::Command,
                });
            }
        }
        return bounded_one_line(tool).map(|summary| AttentionContext {
            summary,
            source: AttentionSource::ToolName,
        });
    }
    input.then(|| AttentionContext {
        summary: "Input requested".into(),
        source: AttentionSource::GenericInput,
    })
}

pub(crate) fn failure_context(raw: &Value) -> FailureContext {
    let category = field(raw, &["error_category", "failure_category", "reason"])
        .unwrap_or_default()
        .to_ascii_lowercase();
    if category.contains("auth") {
        FailureContext::Authentication
    } else if category.contains("permission") || category.contains("denied") {
        FailureContext::PermissionDenied
    } else if category.contains("rate") {
        FailureContext::RateLimited
    } else if category.contains("timeout") {
        FailureContext::Timeout
    } else if category.contains("tool") {
        FailureContext::ToolError
    } else {
        FailureContext::Unknown
    }
}

pub fn qwen_has_user_side_channel(args: &[String]) -> bool {
    args.iter().any(|a| {
        a == "--json-file"
            || a == "--json-fd"
            || a.starts_with("--json-file=")
            || a.starts_with("--json-fd=")
    })
}
pub fn probe_qwen_dual_output(executable: &str) -> bool {
    static CACHE: OnceLock<Mutex<HashMap<String, bool>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(value) = cache.lock().expect("probe cache poisoned").get(executable) {
        return *value;
    }
    let supported = Command::new(executable)
        .arg("--help")
        .output()
        .ok()
        .is_some_and(|o| String::from_utf8_lossy(&o.stdout).contains("--json-file"));
    cache
        .lock()
        .expect("probe cache poisoned")
        .insert(executable.to_owned(), supported);
    supported
}

pub struct JsonlTail {
    path: PathBuf,
    offset: u64,
    pending: String,
    max_line: usize,
    identity: Option<(u64, u64)>,
}
impl JsonlTail {
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
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
            Err(e) => return Err(e.into()),
        };
        let metadata = file.metadata()?;
        let current_identity = {
            use std::os::unix::fs::MetadataExt;
            (metadata.dev(), metadata.ino())
        };
        let len = metadata.len();
        if len < self.offset || self.identity.is_some_and(|old| old != current_identity) {
            self.offset = 0;
            self.pending.clear();
        }
        self.identity = Some(current_identity);
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

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn owned_handler(provider: &str, executable: &Path) -> Result<Value> {
    let executable = executable
        .to_str()
        .context("SessionTap executable path is not valid UTF-8")?;
    Ok(json!({
        "type": "command",
        "command": format!(
            "{} hook emit {}",
            shell_quote(executable),
            shell_quote(provider)
        ),
        "timeout": 3,
        "statusMessage": "SessionTap observability"
    }))
}
/// Follows a symlinked configuration file to its target so managed merges
/// preserve the link instead of replacing it.
fn resolve_config_link(label: &str, path: &Path) -> Result<PathBuf> {
    if path
        .symlink_metadata()
        .is_ok_and(|metadata| metadata.file_type().is_symlink())
    {
        return fs::canonicalize(path).map_err(|error| {
            anyhow::anyhow!(
                "{label} configuration symlink {} points to a missing target: {error}",
                path.display()
            )
        });
    }
    Ok(path.to_path_buf())
}

pub fn merge_hook_config(
    path: &Path,
    provider: &str,
    events: &[&str],
    executable: &Path,
    action: SetupAction,
) -> Result<SetupReport> {
    let resolved = resolve_config_link("provider", path)?;
    let path = resolved.as_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let lock_path = path.with_extension("sessiontap.lock");
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lock_path)?;
    lock.lock_exclusive()?;
    let original = if path.exists() {
        fs::read_to_string(path)?
    } else {
        "{}".into()
    };
    let mut root: Value = serde_json::from_str(&original)
        .context("provider configuration is invalid; refusing to overwrite")?;
    let hooks = root
        .as_object_mut()
        .context("configuration root must be an object")?
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .context("hooks must be an object")?;
    for event in events {
        let groups = hooks
            .entry((*event).to_owned())
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .context("hook event must be an array")?;
        for group in groups.iter_mut() {
            if let Some(handlers) = group.get_mut("hooks").and_then(Value::as_array_mut) {
                handlers.retain(|h| {
                    !(h.get("statusMessage").and_then(Value::as_str)
                        == Some("SessionTap observability")
                        && h.get("command")
                            .and_then(Value::as_str)
                            .is_some_and(|command| command.contains(" hook emit ")))
                });
            }
        }
        groups.retain(|g| {
            g.get("hooks")
                .and_then(Value::as_array)
                .is_none_or(|h| !h.is_empty())
        });
        if action != SetupAction::Remove {
            groups.push(json!({"hooks":[owned_handler(provider,executable)?]}));
        }
    }
    let rendered = serde_json::to_vec_pretty(&root)?;
    let changed = rendered != original.as_bytes();
    if action == SetupAction::Doctor {
        return Ok(SetupReport {
            changed: false,
            healthy: !changed,
            message: if changed {
                "managed hooks need refresh".into()
            } else {
                "managed hooks healthy".into()
            },
        });
    }
    let parent = path.parent().unwrap_or(Path::new("."));
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    temp.write_all(&rendered)?;
    temp.as_file().sync_all()?;
    temp.persist(path).map_err(|e| e.error)?;
    Ok(SetupReport {
        changed,
        healthy: true,
        message: if action == SetupAction::Remove {
            "managed hooks removed".into()
        } else {
            "managed hooks installed".into()
        },
    })
}

pub fn merge_owned_toml(path: &Path, table: &str, value: Option<toml::Value>) -> Result<()> {
    let resolved = resolve_config_link("TOML", path)?;
    let path = resolved.as_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path.with_extension("sessiontap.lock"))?;
    lock.lock_exclusive()?;
    let raw = if path.exists() {
        fs::read_to_string(path)?
    } else {
        String::new()
    };
    let mut document: toml::Table =
        toml::from_str(&raw).context("invalid TOML; refusing to overwrite")?;
    if let Some(value) = value {
        document.insert(table.to_owned(), value);
    } else {
        document.remove(table);
    }
    let parent = path.parent().unwrap_or(Path::new("."));
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    temp.write_all(toml::to_string_pretty(&document)?.as_bytes())?;
    temp.as_file().sync_all()?;
    temp.persist(path).map_err(|error| error.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sessiontap_core::config::CustomAdapter;
    use sessiontap_core::domain::EventKind;

    #[test]
    fn managed_hook_command_quotes_shell_metacharacters() {
        let handler = owned_handler("claude; echo injected", Path::new("/tmp/session tap'bin"))
            .expect("handler");
        assert_eq!(
            handler["command"],
            "'/tmp/session tap'\"'\"'bin' hook emit 'claude; echo injected'"
        );
    }
    #[test]
    fn alias_resolves_concrete_dialect_and_distinct_executable() {
        let mut config = Config::default();
        config.adapters.insert(
            "company-claude".into(),
            CustomAdapter {
                executable: "company-claude".into(),
                inherits: "claude".into(),
            },
        );
        let registry = AdapterRegistry::new(&config);
        let (adapter, executable) = registry.resolve("company-claude").unwrap();
        assert_eq!(adapter.dialect(), "claude");
        assert_eq!(executable, "company-claude");
    }
    #[test]
    fn redaction_preserves_boundaries() {
        let args = vec![
            "--api-key".into(),
            "secret".into(),
            "a b".into(),
            "--token=x".into(),
        ];
        assert_eq!(
            redact_args(&args),
            vec!["--api-key", "[REDACTED]", "a b", "--token=[REDACTED]"]
        );
        assert_eq!(args[1], "secret");
    }
    #[test]
    fn qwen_user_side_channel_wins() {
        assert!(qwen_has_user_side_channel(&["--json-fd=4".into()]));
        assert!(!qwen_has_user_side_channel(&["--help".into()]));
    }
    #[test]
    fn merge_preserves_user_hooks_and_removes_owned() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("settings.json");
        fs::write(
            &p,
            r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"mine"}]}]}}"#,
        )
        .unwrap();
        merge_hook_config(
            &p,
            "claude",
            claude::HOOK_EVENTS,
            Path::new("/bin/sessiontap"),
            SetupAction::Ensure,
        )
        .unwrap();
        let v: Value = serde_json::from_slice(&fs::read(&p).unwrap()).unwrap();
        assert!(v["hooks"]["Stop"].to_string().contains("mine"));
        let once = fs::read(&p).unwrap();
        let report = merge_hook_config(
            &p,
            "claude",
            claude::HOOK_EVENTS,
            Path::new("/bin/sessiontap"),
            SetupAction::Ensure,
        )
        .unwrap();
        assert!(!report.changed);
        assert_eq!(fs::read(&p).unwrap(), once);
        merge_hook_config(
            &p,
            "claude",
            claude::HOOK_EVENTS,
            Path::new("/bin/sessiontap"),
            SetupAction::Remove,
        )
        .unwrap();
        let v: Value = serde_json::from_slice(&fs::read(&p).unwrap()).unwrap();
        assert!(v.to_string().contains("mine"));
        assert!(!v.to_string().contains("SessionTap observability"));
    }
    #[test]
    fn malformed_is_untouched() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("settings.json");
        fs::write(&p, "{").unwrap();
        assert!(
            merge_hook_config(
                &p,
                "qwen",
                qwen::HOOK_EVENTS,
                Path::new("sessiontap"),
                SetupAction::Ensure
            )
            .is_err()
        );
        assert_eq!(fs::read_to_string(p).unwrap(), "{");
    }
    #[test]
    fn concurrent_refresh_keeps_valid_configuration() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("settings.json");
        fs::write(&p, "{}").unwrap();
        let joins = (0..8)
            .map(|_| {
                let p = p.clone();
                std::thread::spawn(move || {
                    merge_hook_config(
                        &p,
                        "codex",
                        codex::HOOK_EVENTS,
                        Path::new("/bin/sessiontap"),
                        SetupAction::Ensure,
                    )
                    .unwrap();
                })
            })
            .collect::<Vec<_>>();
        for join in joins {
            join.join().unwrap();
        }
        let value: Value = serde_json::from_slice(&fs::read(p).unwrap()).unwrap();
        for groups in value["hooks"].as_object().unwrap().values() {
            assert_eq!(groups.as_array().unwrap().len(), 1);
        }
    }
    #[test]
    fn torn_jsonl_waits_for_newline() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("events");
        fs::write(&p, "{\"session_id\":\"x\"}").unwrap();
        let mut tail = JsonlTail::new(p.clone(), 1024);
        assert!(tail.poll().unwrap().is_empty());
        fs::write(&p, "{\"session_id\":\"x\"}\n").unwrap();
        let got = tail.poll().unwrap();
        assert_eq!(got[0]["session_id"], "x");
    }

    #[test]
    fn jsonl_rotation_restarts_at_zero() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("events");
        fs::write(&p, "{\"n\":1}\n").unwrap();
        let mut tail = JsonlTail::new(p.clone(), 1024);
        assert_eq!(tail.poll().unwrap()[0]["n"], 1);
        let replacement = t.path().join("replacement");
        fs::write(&replacement, "{\"n\":2}\n").unwrap();
        fs::rename(replacement, &p).unwrap();
        assert_eq!(tail.poll().unwrap()[0]["n"], 2);
    }

    #[test]
    fn toml_merge_preserves_unrelated_tables() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("config.toml");
        fs::write(&p, "[user]\nvalue=1\n").unwrap();
        let mut owned = toml::Table::new();
        owned.insert("enabled".into(), toml::Value::Boolean(true));
        merge_owned_toml(&p, "sessiontap", Some(toml::Value::Table(owned))).unwrap();
        let raw = fs::read_to_string(&p).unwrap();
        assert!(raw.contains("[user]"));
        assert!(raw.contains("[sessiontap]"));
        merge_owned_toml(&p, "sessiontap", None).unwrap();
        assert!(!fs::read_to_string(p).unwrap().contains("[sessiontap]"));
    }

    #[test]
    fn sanitized_provider_fixtures_normalize_without_raw_content() {
        let id = InvocationId::new();
        let claude: Value =
            serde_json::from_str(include_str!("../tests/fixtures/claude-permission.json")).unwrap();
        let codex: Value =
            serde_json::from_str(include_str!("../tests/fixtures/codex-question.json")).unwrap();
        let qwen: Value =
            serde_json::from_str(include_str!("../tests/fixtures/qwen-stop.json")).unwrap();
        assert_eq!(
            claude::ClaudeAdapter
                .normalize(&id, &claude)
                .unwrap()
                .event
                .kind,
            EventKind::WaitingApproval
        );
        assert_eq!(
            codex::CodexAdapter
                .normalize(&id, &codex)
                .unwrap()
                .event
                .kind,
            EventKind::WaitingInput
        );
        assert_eq!(
            qwen::QwenAdapter.normalize(&id, &qwen).unwrap().event.kind,
            EventKind::Completed
        );
        assert!(
            !serde_json::to_string(&qwen::QwenAdapter.normalize(&id, &qwen).unwrap())
                .unwrap()
                .contains("context_usage")
        );
    }

    #[test]
    fn enriched_fixtures_are_bounded_correlated_and_evidence_backed() {
        let id = InvocationId::new();
        let codex: Value =
            serde_json::from_str(include_str!("../tests/fixtures/codex-clear.json")).unwrap();
        let codex = codex::CodexAdapter.normalize(&id, &codex).unwrap();
        assert_eq!(codex.event.kind, EventKind::ProviderSessionStarted);
        assert_eq!(
            codex.event.provider_session_start_reason.as_deref(),
            Some("clear")
        );
        assert_eq!(codex.event.turn_id.as_deref(), Some("turn-b"));
        assert_eq!(
            codex
                .event
                .provider_metadata
                .as_ref()
                .and_then(|m| m.model.as_deref()),
            Some("gpt-5.6")
        );
        assert!(codex.event.usage.is_none());

        let claude: Value =
            serde_json::from_str(include_str!("../tests/fixtures/claude-question.json")).unwrap();
        let claude = claude::ClaudeAdapter.normalize(&id, &claude).unwrap();
        assert_eq!(claude.event.kind, EventKind::WaitingInput);
        assert_eq!(claude.event.turn_id.as_deref(), Some("prompt-2"));
        let metadata = claude.event.provider_metadata.unwrap();
        assert_eq!(metadata.effort.as_deref(), Some("high"));
        assert_eq!(metadata.permission_mode.as_deref(), Some("acceptEdits"));
        assert!(claude.attention.is_some());
        assert!(claude.event.usage.is_none());

        let qwen: Value =
            serde_json::from_str(include_str!("../tests/fixtures/qwen-stop.json")).unwrap();
        let usage = qwen::QwenAdapter
            .normalize(&id, &qwen)
            .unwrap()
            .event
            .usage
            .unwrap();
        assert_eq!(usage.input_tokens, Some(100));
        assert_eq!(usage.context_window_percent, Some(50));

        let qwen_question: Value =
            serde_json::from_str(include_str!("../tests/fixtures/qwen-question.json")).unwrap();
        let qwen_question = qwen::QwenAdapter.normalize(&id, &qwen_question).unwrap();
        assert_eq!(qwen_question.event.kind, EventKind::WaitingInput);
        assert_eq!(
            qwen_question
                .event
                .provider_metadata
                .as_ref()
                .and_then(|metadata| metadata.permission_mode.as_deref()),
            Some("yolo")
        );
        assert_eq!(
            qwen_question.event.observed_at.to_rfc3339(),
            "2026-08-28T15:08:40.299+00:00"
        );
        assert!(qwen_question.event.received_at > qwen_question.event.observed_at);
    }

    #[test]
    fn qwen_empty_prompt_notifications_do_not_start_turns() {
        let id = InvocationId::new();
        let empty = qwen::QwenAdapter
            .normalize(
                &id,
                &json!({"hook_event_name": "UserPromptSubmit", "prompt": ""}),
            )
            .unwrap();
        assert_eq!(empty.event.kind, EventKind::Enrichment);

        let real = qwen::QwenAdapter
            .normalize(
                &id,
                &json!({"hook_event_name": "UserPromptSubmit", "prompt": "do work"}),
            )
            .unwrap();
        assert_eq!(real.event.kind, EventKind::NewTurn);
    }

    #[test]
    fn attention_fallbacks_are_bounded_and_safe() {
        let id = InvocationId::new();
        let raw = json!({
            "hook_event_name": "PermissionRequest",
            "tool_name": "Bash",
            "description": "Top-level description",
            "tool_input": {
                "command": "echo command",
                "description": "Nested description"
            }
        });
        let attention = codex::CodexAdapter
            .normalize(&id, &raw)
            .unwrap()
            .attention
            .unwrap();
        assert_eq!(attention.summary, "Top-level description");
        assert_eq!(attention.source, AttentionSource::Description);

        let raw = json!({
            "hook_event_name": "PermissionRequest",
            "tool_name": "Bash",
            "tool_input": {
                "command": "echo command",
                "description": "May I run the verification?"
            }
        });
        let attention = codex::CodexAdapter
            .normalize(&id, &raw)
            .unwrap()
            .attention
            .unwrap();
        assert_eq!(attention.summary, "May I run the verification?");
        assert_eq!(attention.source, AttentionSource::Description);

        let raw = json!({"hook_event_name":"PermissionRequest","tool_name":"Bash","tool_input":{"command":"echo\nsecret token=abc"}});
        assert_eq!(
            claude::ClaudeAdapter
                .normalize(&id, &raw)
                .unwrap()
                .attention
                .unwrap()
                .summary,
            "Run a shell command"
        );
        let raw = json!({"hook_event_name":"PermissionRequest","tool_name":"apply_patch","tool_input":{"patch":"PRIVATE"}});
        assert_eq!(
            claude::ClaudeAdapter
                .normalize(&id, &raw)
                .unwrap()
                .attention
                .unwrap()
                .summary,
            "Apply a patch"
        );
        let raw = json!({"hook_event_name":"PermissionRequest","tool_name":"Unknown","tool_input":{"private":"PRIVATE"}});
        assert_eq!(
            claude::ClaudeAdapter
                .normalize(&id, &raw)
                .unwrap()
                .attention
                .unwrap()
                .summary,
            "Unknown"
        );
    }

    #[test]
    fn messages_and_raw_failures_are_excluded() {
        let id = InvocationId::new();
        let done = claude::ClaudeAdapter
            .normalize(
                &id,
                &json!({"hook_event_name":"Stop","last_assistant_message":"PRIVATE"}),
            )
            .unwrap();
        assert_eq!(done.event.kind, EventKind::Completed);
        assert!(done.attention.is_none());
        let failed = qwen::QwenAdapter
            .normalize(
                &id,
                &json!({"hook_event_name":"StopFailure","reason":"timeout","error":"PRIVATE"}),
            )
            .unwrap();
        assert_eq!(failed.failure, Some(FailureContext::Timeout));
        assert!(!serde_json::to_string(&failed).unwrap().contains("PRIVATE"));

        let private = codex::CodexAdapter
            .normalize(
                &id,
                &json!({
                    "hook_event_name": "PostToolUse",
                    "prompt": "PRIVATE_PROMPT",
                    "last_assistant_message": "PRIVATE_ASSISTANT",
                    "transcript_path": "/PRIVATE_TRANSCRIPT",
                    "tool_input": {"command": "PRIVATE_INPUT"},
                    "tool_response": {"output": "PRIVATE_RESPONSE"}
                }),
            )
            .unwrap();
        let serialized = serde_json::to_string(&private).unwrap();
        for marker in [
            "PRIVATE_PROMPT",
            "PRIVATE_ASSISTANT",
            "PRIVATE_TRANSCRIPT",
            "PRIVATE_INPUT",
            "PRIVATE_RESPONSE",
        ] {
            assert!(!serialized.contains(marker));
        }
    }

    #[test]
    fn claude_uses_latest_bounded_transcript_title() {
        let temp = tempfile::tempdir().unwrap();
        let transcript = temp.path().join("session.jsonl");
        fs::write(
            &transcript,
            concat!(
                "{\"aiTitle\":\"Initial title\"}\n",
                "{\"message\":\"PRIVATE transcript content\"}\n",
                "{\"customTitle\":\"  Renamed\\n session  \"}\n"
            ),
        )
        .unwrap();
        let id = InvocationId::new();
        let normalized = claude::ClaudeAdapter
            .normalize(
                &id,
                &json!({
                    "hook_event_name": "Stop",
                    "session_id": "provider-session",
                    "transcript_path": transcript,
                }),
            )
            .unwrap();

        assert_eq!(
            normalized.event.provider_session_name.as_deref(),
            Some("Renamed session")
        );
        let serialized = serde_json::to_string(&normalized).unwrap();
        assert!(!serialized.contains("PRIVATE"));
        assert!(!serialized.contains("session.jsonl"));
    }

    #[test]
    fn hook_and_toml_merges_follow_symlink_targets() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let target_json = temp.path().join("target.json");
        fs::write(&target_json, "{}").unwrap();
        let hook_link = temp.path().join("settings.json");
        symlink(&target_json, &hook_link).unwrap();
        merge_hook_config(
            &hook_link,
            "claude",
            claude::HOOK_EVENTS,
            Path::new("sessiontap"),
            SetupAction::Ensure,
        )
        .unwrap();
        assert!(
            hook_link
                .symlink_metadata()
                .unwrap()
                .file_type()
                .is_symlink()
        );
        let merged: Value =
            serde_json::from_str(&fs::read_to_string(&target_json).unwrap()).unwrap();
        assert!(merged["hooks"].is_object());

        let target_toml = temp.path().join("target.toml");
        fs::write(&target_toml, "[user]\nvalue=1\n").unwrap();
        let toml_link = temp.path().join("config.toml");
        symlink(&target_toml, &toml_link).unwrap();
        merge_owned_toml(
            &toml_link,
            "sessiontap",
            Some(toml::toml! { managed = true }.into()),
        )
        .unwrap();
        assert!(
            toml_link
                .symlink_metadata()
                .unwrap()
                .file_type()
                .is_symlink()
        );
        let merged = fs::read_to_string(target_toml).unwrap();
        assert!(merged.contains("[user]"));
        assert!(merged.contains("[sessiontap]"));
    }

    #[test]
    fn hook_and_toml_merges_reject_dangling_symlinks() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let hook_link = temp.path().join("settings.json");
        symlink(temp.path().join("missing.json"), &hook_link).unwrap();
        assert!(
            merge_hook_config(
                &hook_link,
                "claude",
                claude::HOOK_EVENTS,
                Path::new("sessiontap"),
                SetupAction::Ensure
            )
            .unwrap_err()
            .to_string()
            .contains("missing target")
        );

        let toml_link = temp.path().join("config.toml");
        symlink(temp.path().join("missing.toml"), &toml_link).unwrap();
        assert!(
            merge_owned_toml(&toml_link, "sessiontap", None)
                .unwrap_err()
                .to_string()
                .contains("missing target")
        );
    }

    #[test]
    fn merged_hook_configuration_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("settings.json");
        merge_hook_config(
            &path,
            "claude",
            claude::HOOK_EVENTS,
            Path::new("sessiontap"),
            SetupAction::Ensure,
        )
        .unwrap();
        assert_eq!(path.metadata().unwrap().permissions().mode() & 0o777, 0o600);
    }
}
