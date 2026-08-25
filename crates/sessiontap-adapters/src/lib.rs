use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use chrono::Utc;
use fs2::FileExt;
use serde_json::{Value, json};
use sessiontap_core::{
    config::Config,
    domain::{EventKind, InvocationId, NormalizedEvent, Usage},
};
use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process::Command,
    sync::{Mutex, OnceLock},
};
use uuid::Uuid;

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
    fn normalize(&self, invocation_id: &InvocationId, raw: &Value) -> Result<NormalizedEvent>;
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
        adapters.insert("claude".into(), Box::new(BuiltinAdapter::new("claude")));
        adapters.insert("codex".into(), Box::new(BuiltinAdapter::new("codex")));
        adapters.insert("qwen".into(), Box::new(BuiltinAdapter::new("qwen")));
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

pub struct BuiltinAdapter {
    dialect: &'static str,
}
impl BuiltinAdapter {
    #[must_use]
    pub const fn new(dialect: &'static str) -> Self {
        Self { dialect }
    }
}

#[async_trait]
impl AgentAdapter for BuiltinAdapter {
    fn dialect(&self) -> &'static str {
        self.dialect
    }
    fn prepare_launch(&self, args: &[String], private_dir: &Path) -> Result<LaunchPreparation> {
        if self.dialect != "qwen" || qwen_has_user_side_channel(args) {
            return Ok(LaunchPreparation::default());
        }
        if probe_qwen_dual_output("qwen") {
            let path = private_dir.join("qwen-events.jsonl");
            Ok(LaunchPreparation {
                extra_args: vec!["--json-file".into(), path.to_string_lossy().into_owned()],
                environment: vec![],
                side_channel: Some(path),
            })
        } else {
            Ok(LaunchPreparation::default())
        }
    }
    fn normalize(&self, invocation_id: &InvocationId, raw: &Value) -> Result<NormalizedEvent> {
        normalize_provider(self.dialect, invocation_id, raw)
    }
    async fn setup(
        &self,
        home: &Path,
        executable: &Path,
        action: SetupAction,
    ) -> Result<SetupReport> {
        let path = match self.dialect {
            "claude" => home.join(".claude/settings.json"),
            "codex" => home.join(".codex/hooks.json"),
            "qwen" => home.join(".qwen/settings.json"),
            _ => bail!("unknown dialect"),
        };
        let mut report = merge_hook_config(&path, self.dialect, executable, action)?;
        if self.dialect == "codex" && action != SetupAction::Remove {
            report
                .message
                .push_str("; review or refresh trust with Codex /hooks");
        }
        Ok(report)
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

fn normalize_provider(provider: &str, id: &InvocationId, raw: &Value) -> Result<NormalizedEvent> {
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
    let kind = if lower.contains("userprompt")
        || lower.contains("promptsubmit")
        || lower.contains("turnstart")
    {
        EventKind::NewTurn
    } else if lower.contains("permission") || notification.contains("permission_prompt") {
        EventKind::WaitingApproval
    } else if lower.contains("question")
        || lower.contains("needs_input")
        || lower.contains("userinputrequest")
        || notification.contains("needs_input")
    {
        EventKind::WaitingInput
    } else if lower.contains("pretool") || lower.contains("toolstart") {
        EventKind::Working
    } else if lower.contains("failure") {
        EventKind::Failed
    } else if lower == "stop" || lower.contains("turnend") || notification.contains("idle_prompt") {
        EventKind::Completed
    } else if lower.contains("sessionend") {
        EventKind::SessionEnded
    } else {
        EventKind::Enrichment
    };
    let usage = raw.get("usage").map(|u| Usage {
        input_tokens: u.get("input_tokens").and_then(Value::as_u64),
        output_tokens: u.get("output_tokens").and_then(Value::as_u64),
        context_tokens: u.get("context_tokens").and_then(Value::as_u64),
    });
    Ok(NormalizedEvent {
        schema_version: 1,
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
        observed_at: Utc::now(),
        received_at: Utc::now(),
        source: "hook".into(),
        kind,
        provider_session_id: raw
            .get("session_id")
            .and_then(Value::as_str)
            .map(str::to_owned),
        usage,
        turn_id: raw
            .get("turn_id")
            .and_then(Value::as_str)
            .map(str::to_owned),
    })
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
fn events(provider: &str) -> &'static [&'static str] {
    match provider {
        "claude" => &[
            "SessionStart",
            "UserPromptSubmit",
            "PreToolUse",
            "PermissionRequest",
            "Notification",
            "Stop",
            "StopFailure",
            "SessionEnd",
        ],
        "codex" => &[
            "SessionStart",
            "UserPromptSubmit",
            "PreToolUse",
            "PermissionRequest",
            "PostToolUse",
            "Stop",
            "SessionEnd",
        ],
        "qwen" => &[
            "SessionStart",
            "UserPromptSubmit",
            "PreToolUse",
            "PermissionRequest",
            "Notification",
            "Stop",
            "StopFailure",
            "SubagentStop",
            "SessionEnd",
        ],
        _ => &[],
    }
}

pub fn merge_hook_config(
    path: &Path,
    provider: &str,
    executable: &Path,
    action: SetupAction,
) -> Result<SetupReport> {
    if path
        .symlink_metadata()
        .is_ok_and(|metadata| metadata.file_type().is_symlink())
    {
        bail!("provider configuration must not be a symlink");
    }
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
    for event in events(provider) {
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
    if path
        .symlink_metadata()
        .is_ok_and(|metadata| metadata.file_type().is_symlink())
    {
        bail!("TOML configuration must not be a symlink");
    }
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
            Path::new("/bin/sessiontap"),
            SetupAction::Ensure,
        )
        .unwrap();
        assert!(!report.changed);
        assert_eq!(fs::read(&p).unwrap(), once);
        merge_hook_config(
            &p,
            "claude",
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
            merge_hook_config(&p, "qwen", Path::new("sessiontap"), SetupAction::Ensure).is_err()
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
            normalize_provider("claude", &id, &claude).unwrap().kind,
            EventKind::WaitingApproval
        );
        assert_eq!(
            normalize_provider("codex", &id, &codex).unwrap().kind,
            EventKind::WaitingInput
        );
        assert_eq!(
            normalize_provider("qwen", &id, &qwen).unwrap().kind,
            EventKind::Completed
        );
        assert!(
            !serde_json::to_string(&normalize_provider("qwen", &id, &qwen).unwrap())
                .unwrap()
                .contains("context_usage")
        );
    }

    #[test]
    fn hook_and_toml_merges_reject_symlink_targets() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let victim_json = temp.path().join("victim.json");
        fs::write(&victim_json, "{}").unwrap();
        let hook_link = temp.path().join("settings.json");
        symlink(&victim_json, &hook_link).unwrap();
        assert!(
            merge_hook_config(
                &hook_link,
                "claude",
                Path::new("sessiontap"),
                SetupAction::Ensure
            )
            .is_err()
        );
        assert_eq!(fs::read_to_string(&victim_json).unwrap(), "{}");

        let victim_toml = temp.path().join("victim.toml");
        fs::write(&victim_toml, "[user]\nvalue=1\n").unwrap();
        let toml_link = temp.path().join("config.toml");
        symlink(&victim_toml, &toml_link).unwrap();
        assert!(merge_owned_toml(&toml_link, "sessiontap", None).is_err());
        assert_eq!(
            fs::read_to_string(victim_toml).unwrap(),
            "[user]\nvalue=1\n"
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
            Path::new("sessiontap"),
            SetupAction::Ensure,
        )
        .unwrap();
        assert_eq!(path.metadata().unwrap().permissions().mode() & 0o777, 0o600);
    }
}
