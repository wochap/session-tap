use crate::{
    CollectSessionDataRequest, NormalizeContext, OpaqueCursor, SessionEnrichment, SetupAction,
    SetupReport,
    artifact::{
        ArtifactCursor, CollectError, Collected, SessionCollector, check_cancelled,
        cursor_unchanged, open_bounded_nofollow, optional_u64, validate_under_root,
    },
    bounded_field,
    dialect::HookDialect,
    driver::HookAdapter,
    normalized_tool_label, sanitize_bounded, status_excerpt, status_reason_context,
};
use anyhow::{Context, Result, bail};
use fs2::FileExt;
use serde_json::Value;
use sessiontap_core::ProviderId;
use sessiontap_core::domain::{
    ArtifactCollectionContext, EventKind, ProviderMetadata, StatusReasonContext,
    StatusReasonSource, ToolActivityPhase, ToolActivityUpdate, Usage,
};
use sessiontap_infra::fs::atomic_write;
use std::{
    collections::BTreeSet,
    ffi::OsString,
    fs::{self, OpenOptions},
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
};

const MAX_SCAN_BYTES: u64 = 64 * 1024 * 1024;
const MAX_LINE_BYTES: usize = 1024 * 1024;

/// Display name for an Antigravity model: drops a leading `gemini-` or `claude-` prefix.
pub(crate) fn model_label(model: &str) -> String {
    let stripped = model
        .strip_prefix("gemini-")
        .or_else(|| model.strip_prefix("claude-"))
        .unwrap_or(model);
    stripped.to_owned()
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

pub fn render_plugin_json() -> String {
    serde_json::to_string_pretty(&serde_json::json!({
        "name": "sessiontap",
        "description": "SessionTap lifecycle hooks for Antigravity CLI"
    }))
    .unwrap()
}

pub fn render_hooks_json(executable: &str) -> String {
    let quoted = shell_quote(executable);
    let handler = |event: &str, stdout_json: &str| -> serde_json::Value {
        serde_json::json!({
            "type": "command",
            "command": format!("{quoted} hook emit 'agy' '{event}' >/dev/null 2>&1 || true; printf '{stdout_json}\\n'"),
            "timeout": 3
        })
    };
    let flat_event =
        |event: &str| -> serde_json::Value { serde_json::json!([handler(event, "{}")]) };
    let matcher_event = |event: &str, stdout_json: &str| -> serde_json::Value {
        serde_json::json!([
            {
                "matcher": "*",
                "hooks": [handler(event, stdout_json)]
            }
        ])
    };
    let doc = serde_json::json!({
        "sessiontap": {
            "SessionStart": flat_event("SessionStart"),
            "PreInvocation": flat_event("PreInvocation"),
            "PreToolUse": matcher_event("PreToolUse", "{\"decision\":\"allow\"}"),
            "PostToolUse": matcher_event("PostToolUse", "{}"),
            "PostInvocation": flat_event("PostInvocation"),
            "Stop": flat_event("Stop")
        }
    });
    serde_json::to_string_pretty(&doc).unwrap()
}

fn is_plugin_disabled(config_root: &Path) -> bool {
    let config_file = config_root.join("config.json");
    if let Ok(content) = fs::read_to_string(&config_file) {
        if let Ok(value) = serde_json::from_str::<Value>(&content) {
            return value
                .get("plugins")
                .and_then(|p| p.get("sessiontap"))
                .and_then(|s| s.get("enabled"))
                .and_then(Value::as_bool)
                == Some(false);
        }
    }
    false
}

pub fn manage_plugin(home: &Path, executable: &Path, action: SetupAction) -> Result<SetupReport> {
    manage_plugin_in(
        home,
        std::env::var_os("GEMINI_CONFIG_DIR"),
        executable,
        action,
    )
}

pub fn manage_plugin_in(
    home: &Path,
    config_dir_env: Option<OsString>,
    executable: &Path,
    action: SetupAction,
) -> Result<SetupReport> {
    let executable_str = executable
        .to_str()
        .context("SessionTap executable path is not valid UTF-8")?;
    let config_dir = config_dir_env
        .filter(|dir| !dir.is_empty())
        .map_or_else(|| home.join(".gemini/config"), PathBuf::from);
    let plugin_dir = config_dir.join("plugins").join("sessiontap");
    let plugin_file = plugin_dir.join("plugin.json");
    let hooks_file = plugin_dir.join("hooks.json");

    let rendered_plugin = render_plugin_json();
    let rendered_hooks = render_hooks_json(executable_str);

    let existing_plugin = fs::read_to_string(&plugin_file).ok();
    let existing_hooks = fs::read_to_string(&hooks_file).ok();

    match action {
        SetupAction::Ensure => {
            let plugin_matches = existing_plugin.as_deref() == Some(rendered_plugin.as_str());
            let hooks_matches = existing_hooks.as_deref() == Some(rendered_hooks.as_str());

            if plugin_matches && hooks_matches {
                return Ok(SetupReport {
                    changed: false,
                    healthy: true,
                    message: "managed plugin healthy".into(),
                });
            }

            fs::create_dir_all(&plugin_dir)?;
            let lock_path = plugin_dir.join("sessiontap.lock");
            let lock = OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(&lock_path)?;
            lock.lock_exclusive()?;

            atomic_write(&plugin_file, rendered_plugin.as_bytes(), 0o600)?;
            atomic_write(&hooks_file, rendered_hooks.as_bytes(), 0o600)?;

            drop(lock);
            let _ = fs::remove_file(&lock_path);

            Ok(SetupReport {
                changed: true,
                healthy: true,
                message: "managed plugin installed".into(),
            })
        }
        SetupAction::Doctor => {
            let plugin_matches = existing_plugin.as_deref() == Some(rendered_plugin.as_str());
            let hooks_matches = existing_hooks.as_deref() == Some(rendered_hooks.as_str());
            let healthy = plugin_matches && hooks_matches;

            if !healthy {
                return Ok(SetupReport {
                    changed: false,
                    healthy: false,
                    message: "managed plugin needs refresh".into(),
                });
            }

            if is_plugin_disabled(&config_dir) {
                return Ok(SetupReport {
                    changed: false,
                    healthy: false,
                    message: "sessiontap plugin is disabled in config.json".into(),
                });
            }

            Ok(SetupReport {
                changed: false,
                healthy: true,
                message: "managed plugin healthy".into(),
            })
        }
        SetupAction::Remove => {
            let mut changed = false;
            if plugin_file.exists() {
                fs::remove_file(&plugin_file)?;
                changed = true;
            }
            if hooks_file.exists() {
                fs::remove_file(&hooks_file)?;
                changed = true;
            }
            let _ = fs::remove_dir(&plugin_dir);

            Ok(SetupReport {
                changed,
                healthy: true,
                message: if changed {
                    "managed plugin removed".into()
                } else {
                    "managed plugin absent".into()
                },
            })
        }
    }
}

pub type AgyAdapter = HookAdapter<AgyDialect, AgyCollector>;

#[allow(non_upper_case_globals)]
pub const AgyAdapter: AgyAdapter = HookAdapter::new(AgyDialect, AgyCollector, manage_plugin);

pub struct AgyDialect;

impl HookDialect for AgyDialect {
    fn id(&self) -> ProviderId {
        ProviderId::Agy
    }

    fn classify(&self, raw: &Value) -> Option<EventKind> {
        let name = raw
            .get("hook_event_name")
            .or_else(|| raw.get("event_name"))
            .or_else(|| raw.get("type"))
            .and_then(Value::as_str)?;
        match name {
            "SessionStart" => Some(EventKind::ProviderSessionStarted),
            "PreInvocation" => Some(EventKind::NewTurn),
            "PreToolUse" => {
                let tool_call = raw.get("toolCall");
                let tool_name = tool_call
                    .and_then(|t| t.get("name"))
                    .or_else(|| raw.get("tool_name"))
                    .and_then(Value::as_str);
                if tool_name == Some("ask_question") {
                    Some(EventKind::WaitingInput)
                } else if tool_name == Some("run_command")
                    && tool_call
                        .and_then(|t| t.get("args"))
                        .and_then(|a| a.get("BypassSandbox"))
                        .and_then(Value::as_bool)
                        == Some(true)
                {
                    Some(EventKind::WaitingApproval)
                } else {
                    Some(EventKind::Working)
                }
            }
            "PostToolUse" => Some(EventKind::Working),
            "PostInvocation" => Some(EventKind::Enrichment),
            "Stop" => match raw.get("terminationReason").and_then(Value::as_str) {
                Some("USER_CANCELED") => Some(EventKind::Interrupted),
                Some(
                    "ERROR"
                    | "MAX_INVOCATIONS"
                    | "MAX_FORCED_INVOCATIONS"
                    | "MAX_TOKEN_BUDGET_EXCEEDED",
                ) => Some(EventKind::Failed),
                Some("NO_TOOL_CALL") => Some(EventKind::Completed),
                _ => Some(EventKind::Completed),
            },
            _ => None,
        }
    }

    fn provider_event_id(&self, raw: &Value) -> Option<String> {
        raw.get("event_id")
            .or_else(|| raw.get("eventId"))
            .and_then(Value::as_str)
            .map(str::to_owned)
    }

    fn provider_session_id(&self, raw: &Value) -> Option<String> {
        raw.get("conversationId")
            .or_else(|| raw.get("session_id"))
            .and_then(Value::as_str)
            .map(str::to_owned)
    }

    fn start_reason(&self, raw: &Value) -> Option<String> {
        bounded_field(raw, &["source", "reason", "start_reason"], 32)
            .filter(|v| matches!(v.as_str(), "startup" | "clear" | "resume" | "compact"))
    }

    fn metadata(&self, raw: &Value) -> Option<ProviderMetadata> {
        let model = raw
            .get("modelName")
            .or_else(|| raw.get("model"))
            .and_then(Value::as_str)
            .and_then(|m| sanitize_bounded(m, 160))
            .filter(|m| !m.is_empty());
        let model_label = model.as_deref().map(model_label);
        let metadata = ProviderMetadata {
            model,
            model_label,
            effort: None,
            permission_mode: None,
            current_turn_id: None,
        };
        (metadata != ProviderMetadata::default()).then_some(metadata)
    }

    fn turn_id(&self, raw: &Value) -> Option<String> {
        raw.get("invocationNum")
            .or_else(|| raw.get("turn_id"))
            .map(|v| match v {
                Value::Number(n) => n.to_string(),
                Value::String(s) => s.clone(),
                _ => String::new(),
            })
            .filter(|s| !s.is_empty())
    }

    fn approval_reason(&self, raw: &Value) -> Option<StatusReasonContext> {
        let tool_call = raw.get("toolCall")?;
        let name = tool_call
            .get("name")
            .or_else(|| raw.get("tool_name"))
            .and_then(Value::as_str)?;
        let label = if name == "run_command" {
            "shell".to_string()
        } else {
            normalized_tool_label(name)?
        };
        let args = tool_call.get("args");
        let summary = args
            .and_then(|a| a.get("toolSummary").or_else(|| a.get("CommandLine")))
            .and_then(Value::as_str)
            .and_then(status_excerpt);
        let summary_text = match summary {
            Some(desc) => format!("{label} {desc}"),
            None => label,
        };
        Some(StatusReasonContext {
            summary: summary_text,
            source: if args.and_then(|a| a.get("toolSummary")).is_some() {
                StatusReasonSource::Description
            } else if args.and_then(|a| a.get("CommandLine")).is_some() {
                StatusReasonSource::Command
            } else {
                StatusReasonSource::ToolName
            },
        })
    }

    fn input_reason(&self, raw: &Value) -> Option<StatusReasonContext> {
        let question = raw
            .get("toolCall")
            .and_then(|tc| tc.get("args"))
            .and_then(|a| a.get("questions"))
            .and_then(Value::as_array)
            .and_then(|qs| qs.first())
            .and_then(|q| q.get("question"))
            .and_then(Value::as_str)
            .and_then(status_excerpt);
        if let Some(summary) = question {
            return Some(StatusReasonContext {
                summary,
                source: StatusReasonSource::Question,
            });
        }
        status_reason_context(raw, true)
    }

    fn completed_reason(&self, raw: &Value) -> Option<StatusReasonContext> {
        raw.get("last_assistant_message")
            .or_else(|| raw.get("excerpt"))
            .and_then(Value::as_str)
            .and_then(status_excerpt)
            .map(|summary| StatusReasonContext {
                summary,
                source: StatusReasonSource::AssistantMessage,
            })
    }

    fn failure_reason(&self, raw: &Value) -> Option<StatusReasonContext> {
        let category = raw
            .get("terminationReason")
            .or_else(|| raw.get("error"))
            .and_then(Value::as_str)
            .and_then(status_excerpt);
        category.map(|c| StatusReasonContext {
            summary: c,
            source: StatusReasonSource::FailureCategory,
        })
    }

    fn tool_activity(
        &self,
        raw: &Value,
        _context: &NormalizeContext<'_>,
    ) -> Option<ToolActivityUpdate> {
        let event = raw
            .get("hook_event_name")
            .or_else(|| raw.get("event_name"))
            .or_else(|| raw.get("type"))
            .and_then(Value::as_str)?;
        match event {
            "PreToolUse" => {
                let tool_call = raw.get("toolCall");
                let name = tool_call
                    .and_then(|tc| tc.get("name"))
                    .or_else(|| raw.get("tool_name"))
                    .and_then(Value::as_str)?;
                if name == "ask_question" {
                    return None;
                }
                let is_bypass = name == "run_command"
                    && tool_call
                        .and_then(|tc| tc.get("args"))
                        .and_then(|a| a.get("BypassSandbox"))
                        .and_then(Value::as_bool)
                        == Some(true);
                let phase = if is_bypass {
                    ToolActivityPhase::Attention
                } else {
                    ToolActivityPhase::Start
                };
                let label = if name == "run_command" {
                    "shell".to_string()
                } else {
                    normalized_tool_label(name)?
                };
                let correlation_id = raw
                    .get("stepIdx")
                    .map(|v| match v {
                        Value::Number(n) => n.to_string(),
                        Value::String(s) => s.clone(),
                        _ => String::new(),
                    })
                    .filter(|s| !s.is_empty());
                let detail = tool_call
                    .and_then(|tc| tc.get("args"))
                    .and_then(|a| a.get("toolSummary").or_else(|| a.get("CommandLine")))
                    .and_then(Value::as_str)
                    .and_then(status_excerpt);
                Some(ToolActivityUpdate {
                    phase,
                    label,
                    correlation_id,
                    detail,
                })
            }
            "PostToolUse" => {
                let error = raw.get("error").and_then(Value::as_str).unwrap_or("");
                let phase = if !error.trim().is_empty() {
                    ToolActivityPhase::Failure
                } else {
                    ToolActivityPhase::Finish
                };
                let tool_call = raw.get("toolCall");
                let name = tool_call
                    .and_then(|tc| tc.get("name"))
                    .or_else(|| raw.get("tool_name"))
                    .and_then(Value::as_str);
                let label = match name {
                    Some("run_command") => "shell".to_string(),
                    Some(n) => normalized_tool_label(n)?,
                    None => "tool".to_string(),
                };
                let correlation_id = raw
                    .get("stepIdx")
                    .map(|v| match v {
                        Value::Number(n) => n.to_string(),
                        Value::String(s) => s.clone(),
                        _ => String::new(),
                    })
                    .filter(|s| !s.is_empty());
                Some(ToolActivityUpdate {
                    phase,
                    label,
                    correlation_id,
                    detail: None,
                })
            }
            _ => None,
        }
    }

    fn collection_context(&self, raw: &Value) -> Option<ArtifactCollectionContext> {
        let conversation_id = self.provider_session_id(raw)?;
        let locator = raw
            .get("transcriptPath")
            .or_else(|| raw.get("transcript_path"))
            .and_then(Value::as_str)?;
        Some(ArtifactCollectionContext {
            adapter_identity: ProviderId::Agy,
            provider_session_id: conversation_id,
            locator: PathBuf::from(locator),
        })
        .filter(|c| !c.provider_session_id.is_empty() && !c.locator.as_os_str().is_empty())
    }
}

#[derive(Clone, Copy)]
pub struct AgyCollector;

impl SessionCollector for AgyCollector {
    fn collect(&self, request: &CollectSessionDataRequest) -> Result<Collected, CollectError> {
        Ok(scan(request).context("Antigravity collection")?)
    }
}

pub fn scan(request: &CollectSessionDataRequest) -> Result<Collected> {
    scan_in(request, std::env::var_os("AGY_DATA_DIR"))
}

pub fn scan_in(
    request: &CollectSessionDataRequest,
    data_dir_env: Option<OsString>,
) -> Result<Collected> {
    check_cancelled(request)?;
    let brain_root = data_dir_env
        .filter(|dir| !dir.is_empty())
        .map_or_else(
            || request.home.join(".gemini/antigravity-cli"),
            PathBuf::from,
        )
        .join("brain");
    let canonical =
        validate_under_root(&brain_root, &request.locator, Some("transcript"), "jsonl")?;
    check_cancelled(request)?;
    let canonical_root = fs::canonicalize(&brain_root)?;
    let expected_session = canonical_root.join(&request.key.provider_session_id);
    if !canonical.starts_with(&expected_session) {
        bail!("transcript does not belong to the authenticated session");
    }
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
    let mut seen_steps = BTreeSet::new();
    let mut usage_observed = false;
    let mut model = None;
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
            bail!("Antigravity transcript record exceeds limit");
        }
        if line.last() != Some(&b'\n') {
            break;
        }
        let record: Value =
            serde_json::from_slice(&line).context("malformed Antigravity transcript record")?;
        if let Some(m) = record
            .get("modelName")
            .or_else(|| record.get("model"))
            .and_then(Value::as_str)
            .and_then(|m| sanitize_bounded(m, 160))
        {
            model = Some(m);
        }
        if record.get("type").and_then(Value::as_str) != Some("PLANNER_RESPONSE") {
            continue;
        }
        let Some(step_index) = optional_u64(&record, "step_index")? else {
            continue;
        };
        if !seen_steps.insert(step_index) {
            continue;
        }
        let row_input = optional_u64(&record, "input_tokens")?
            .or(record
                .get("usage")
                .and_then(|u| optional_u64(u, "input_tokens").ok().flatten()))
            .unwrap_or(0);
        let row_cache = optional_u64(&record, "cache_read_tokens")?
            .or(record
                .get("usage")
                .and_then(|u| optional_u64(u, "cache_read_tokens").ok().flatten()))
            .unwrap_or(0);
        let row_output = optional_u64(&record, "output_tokens")?
            .or(record
                .get("usage")
                .and_then(|u| optional_u64(u, "output_tokens").ok().flatten()))
            .unwrap_or(0);

        usage_observed = true;
        let step_input = row_input
            .checked_add(row_cache)
            .context("Antigravity input token overflow")?;
        input = input
            .checked_add(step_input)
            .context("Antigravity cumulative input overflow")?;
        output = output
            .checked_add(row_output)
            .context("Antigravity cumulative output overflow")?;
        context = Some(step_input);
    }
    cursor.ensure_stable(&reader.into_inner())?;
    let usage = usage_observed.then_some(Usage {
        input_tokens: Some(input),
        output_tokens: Some(output),
        context_tokens: context,
        context_window_percent: None,
    });
    let model_label = model.as_deref().map(model_label);
    Ok(Collected::Complete {
        enrichment: SessionEnrichment {
            session_name: None,
            usage,
            model,
            model_label,
        },
        cursor: OpaqueCursor::new(cursor),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AgentAdapter, CollectionCancellation, ProviderSessionKey};
    use serde_json::json;
    use sessiontap_core::domain::InvocationId;

    #[test]
    fn model_label_strips_expected_prefixes() {
        assert_eq!(model_label("gemini-2.5-pro"), "2.5-pro");
        assert_eq!(model_label("gemini-1.5-flash"), "1.5-flash");
        assert_eq!(model_label("claude-3-7-sonnet"), "3-7-sonnet");
        assert_eq!(model_label("gpt-4o"), "gpt-4o");
    }

    #[test]
    fn plugin_management_lifecycle() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();
        let executable = Path::new("/opt/sessiontap/bin/sessiontap");

        // Doctor when uninstalled
        let report = manage_plugin_in(home, None, executable, SetupAction::Doctor).unwrap();
        assert!(!report.healthy);
        assert!(!report.changed);
        assert_eq!(report.message, "managed plugin needs refresh");

        // Ensure (install)
        let report = manage_plugin_in(home, None, executable, SetupAction::Ensure).unwrap();
        assert!(report.healthy);
        assert!(report.changed);
        assert_eq!(report.message, "managed plugin installed");

        let plugin_path = home.join(".gemini/config/plugins/sessiontap/plugin.json");
        let hooks_path = home.join(".gemini/config/plugins/sessiontap/hooks.json");
        assert!(plugin_path.exists());
        assert!(hooks_path.exists());

        // Idempotent Ensure
        let report = manage_plugin_in(home, None, executable, SetupAction::Ensure).unwrap();
        assert!(report.healthy);
        assert!(!report.changed);
        assert_eq!(report.message, "managed plugin healthy");

        // Doctor when healthy
        let report = manage_plugin_in(home, None, executable, SetupAction::Doctor).unwrap();
        assert!(report.healthy);
        assert!(!report.changed);
        assert_eq!(report.message, "managed plugin healthy");

        // Drift detection: change executable
        let other_exec = Path::new("/opt/other/sessiontap");
        let report = manage_plugin_in(home, None, other_exec, SetupAction::Doctor).unwrap();
        assert!(!report.healthy);
        assert_eq!(report.message, "managed plugin needs refresh");

        // Drift update
        let report = manage_plugin_in(home, None, other_exec, SetupAction::Ensure).unwrap();
        assert!(report.healthy);
        assert!(report.changed);
        let hooks_content = fs::read_to_string(&hooks_path).unwrap();
        assert!(hooks_content.contains("/opt/other/sessiontap"));

        // Doctor detects disabled plugin in config.json
        let config_file = home.join(".gemini/config/config.json");
        fs::write(
            &config_file,
            serde_json::to_string(&json!({
                "plugins": {
                    "sessiontap": {
                        "enabled": false
                    }
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let report = manage_plugin_in(home, None, other_exec, SetupAction::Doctor).unwrap();
        assert!(!report.healthy);
        assert_eq!(
            report.message,
            "sessiontap plugin is disabled in config.json"
        );

        // Clean removal
        let report = manage_plugin_in(home, None, other_exec, SetupAction::Remove).unwrap();
        assert!(report.healthy);
        assert!(report.changed);
        assert_eq!(report.message, "managed plugin removed");
        assert!(!plugin_path.exists());
        assert!(!hooks_path.exists());
        assert!(!home.join(".gemini/config/plugins/sessiontap").exists());
        // config.json must remain intact
        assert!(config_file.exists());

        // Repeated removal reports absent
        let report = manage_plugin_in(home, None, other_exec, SetupAction::Remove).unwrap();
        assert!(report.healthy);
        assert!(!report.changed);
        assert_eq!(report.message, "managed plugin absent");
    }

    #[test]
    fn custom_gemini_config_dir_env() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let custom = temp.path().join("custom_config");
        let executable = Path::new("/bin/sessiontap");

        let report = manage_plugin_in(
            &home,
            Some(custom.clone().into_os_string()),
            executable,
            SetupAction::Ensure,
        )
        .unwrap();
        assert!(report.healthy);
        assert!(custom.join("plugins/sessiontap/plugin.json").exists());
        assert!(!home.join(".gemini/config").exists());
    }

    #[test]
    fn dialect_normalizes_session_start_and_invocation() {
        let id = InvocationId::new();
        let start = AgentAdapter::normalize(
            &AgyAdapter,
            &id,
            &json!({
                "hook_event_name": "SessionStart",
                "conversationId": "conv-1",
                "modelName": "gemini-2.5-pro"
            }),
        )
        .unwrap()
        .into_event()
        .unwrap();

        assert_eq!(start.event.kind, EventKind::ProviderSessionStarted);
        assert_eq!(start.event.provider_session_id.as_deref(), Some("conv-1"));
        let meta = start.event.provider_metadata.unwrap();
        assert_eq!(meta.model.as_deref(), Some("gemini-2.5-pro"));
        assert_eq!(meta.model_label.as_deref(), Some("2.5-pro"));

        let invocation = AgentAdapter::normalize(
            &AgyAdapter,
            &id,
            &json!({
                "hook_event_name": "PreInvocation",
                "conversationId": "conv-1",
                "invocationNum": 2,
                "lastUserInput": "write tests"
            }),
        )
        .unwrap()
        .into_event()
        .unwrap();

        assert_eq!(invocation.event.kind, EventKind::NewTurn);
        assert_eq!(invocation.event.turn_id.as_deref(), Some("2"));

        let post_invocation = AgentAdapter::normalize(
            &AgyAdapter,
            &id,
            &json!({
                "hook_event_name": "PostInvocation",
                "conversationId": "conv-1"
            }),
        )
        .unwrap()
        .into_event()
        .unwrap();
        assert_eq!(post_invocation.event.kind, EventKind::Enrichment);
    }

    #[test]
    fn dialect_normalizes_pre_tool_use_scenarios() {
        let id = InvocationId::new();

        // ask_question -> WaitingInput
        let ask = AgentAdapter::normalize(
            &AgyAdapter,
            &id,
            &json!({
                "hook_event_name": "PreToolUse",
                "stepIdx": 3,
                "toolCall": {
                    "name": "ask_question",
                    "args": {
                        "questions": [
                            {"question": "Which database engine?"}
                        ]
                    }
                }
            }),
        )
        .unwrap()
        .into_event()
        .unwrap();

        assert_eq!(ask.event.kind, EventKind::WaitingInput);
        assert!(ask.event.tool_activity.is_none());
        let reason = ask.status_reason.unwrap();
        assert_eq!(reason.source, StatusReasonSource::Question);
        assert_eq!(reason.summary, "Which database engine?");

        // run_command BypassSandbox: true -> WaitingApproval + ToolActivityPhase::Attention
        let bypass = AgentAdapter::normalize(
            &AgyAdapter,
            &id,
            &json!({
                "hook_event_name": "PreToolUse",
                "stepIdx": 4,
                "toolCall": {
                    "name": "run_command",
                    "args": {
                        "BypassSandbox": true,
                        "toolSummary": "Run integration tests",
                        "CommandLine": "cargo test"
                    }
                }
            }),
        )
        .unwrap()
        .into_event()
        .unwrap();

        assert_eq!(bypass.event.kind, EventKind::WaitingApproval);
        let activity = bypass.event.tool_activity.unwrap();
        assert_eq!(activity.phase, ToolActivityPhase::Attention);
        assert_eq!(activity.label, "shell");
        assert_eq!(activity.correlation_id.as_deref(), Some("4"));
        let reason = bypass.status_reason.unwrap();
        assert_eq!(reason.summary, "shell Run integration tests");

        // run_command BypassSandbox: false -> Working + ToolActivityPhase::Start
        let sandboxed = AgentAdapter::normalize(
            &AgyAdapter,
            &id,
            &json!({
                "hook_event_name": "PreToolUse",
                "stepIdx": 5,
                "toolCall": {
                    "name": "run_command",
                    "args": {
                        "BypassSandbox": false,
                        "CommandLine": "ls"
                    }
                }
            }),
        )
        .unwrap()
        .into_event()
        .unwrap();

        assert_eq!(sandboxed.event.kind, EventKind::Working);
        assert!(sandboxed.status_reason.is_none());
        let activity = sandboxed.event.tool_activity.unwrap();
        assert_eq!(activity.phase, ToolActivityPhase::Start);
        assert_eq!(activity.label, "shell");
        assert_eq!(activity.correlation_id.as_deref(), Some("5"));

        // other tools -> Working + ToolActivityPhase::Start
        let read = AgentAdapter::normalize(
            &AgyAdapter,
            &id,
            &json!({
                "hook_event_name": "PreToolUse",
                "stepIdx": 6,
                "toolCall": {
                    "name": "read_file",
                    "args": {
                        "Path": "/src/main.rs"
                    }
                }
            }),
        )
        .unwrap()
        .into_event()
        .unwrap();

        assert_eq!(read.event.kind, EventKind::Working);
        let activity = read.event.tool_activity.unwrap();
        assert_eq!(activity.phase, ToolActivityPhase::Start);
        assert_eq!(activity.label, "read_file");
        assert_eq!(activity.correlation_id.as_deref(), Some("6"));
    }

    #[test]
    fn dialect_normalizes_post_tool_use() {
        let id = InvocationId::new();

        // Successful post tool use
        let ok = AgentAdapter::normalize(
            &AgyAdapter,
            &id,
            &json!({
                "hook_event_name": "PostToolUse",
                "stepIdx": 7,
                "toolCall": {
                    "name": "run_command"
                }
            }),
        )
        .unwrap()
        .into_event()
        .unwrap();

        assert_eq!(ok.event.kind, EventKind::Working);
        let activity = ok.event.tool_activity.unwrap();
        assert_eq!(activity.phase, ToolActivityPhase::Finish);
        assert_eq!(activity.label, "shell");
        assert_eq!(activity.correlation_id.as_deref(), Some("7"));

        // Failed post tool use
        let fail = AgentAdapter::normalize(
            &AgyAdapter,
            &id,
            &json!({
                "hook_event_name": "PostToolUse",
                "stepIdx": 8,
                "error": "command returned exit code 1",
                "toolCall": {
                    "name": "run_command"
                }
            }),
        )
        .unwrap()
        .into_event()
        .unwrap();

        assert_eq!(fail.event.kind, EventKind::Working);
        let activity = fail.event.tool_activity.unwrap();
        assert_eq!(activity.phase, ToolActivityPhase::Failure);
        assert_eq!(activity.label, "shell");
        assert_eq!(activity.correlation_id.as_deref(), Some("8"));
    }

    #[test]
    fn dialect_normalizes_stop_termination_reasons() {
        let id = InvocationId::new();

        // USER_CANCELED -> Interrupted
        let stop_cancel = AgentAdapter::normalize(
            &AgyAdapter,
            &id,
            &json!({
                "hook_event_name": "Stop",
                "terminationReason": "USER_CANCELED"
            }),
        )
        .unwrap()
        .into_event()
        .unwrap();
        assert_eq!(stop_cancel.event.kind, EventKind::Interrupted);

        // ERROR and MAX_* -> Failed
        for reason in [
            "ERROR",
            "MAX_INVOCATIONS",
            "MAX_FORCED_INVOCATIONS",
            "MAX_TOKEN_BUDGET_EXCEEDED",
        ] {
            let stop_err = AgentAdapter::normalize(
                &AgyAdapter,
                &id,
                &json!({
                    "hook_event_name": "Stop",
                    "terminationReason": reason
                }),
            )
            .unwrap()
            .into_event()
            .unwrap();
            assert_eq!(stop_err.event.kind, EventKind::Failed);
        }

        // NO_TOOL_CALL -> Completed
        let stop_ok = AgentAdapter::normalize(
            &AgyAdapter,
            &id,
            &json!({
                "hook_event_name": "Stop",
                "terminationReason": "NO_TOOL_CALL"
            }),
        )
        .unwrap()
        .into_event()
        .unwrap();
        assert_eq!(stop_ok.event.kind, EventKind::Completed);
    }

    #[test]
    fn collector_deduplicates_and_accounts_usage() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();
        let session_id = "test-session-123";
        let brain_dir = home
            .join(".gemini/antigravity-cli/brain")
            .join(session_id)
            .join(".system_generated/logs");
        fs::create_dir_all(&brain_dir).unwrap();
        let transcript = brain_dir.join("transcript.jsonl");

        let lines = [
            json!({
                "type": "USER_INPUT",
                "step_index": 0
            }),
            json!({
                "type": "PLANNER_RESPONSE",
                "step_index": 1,
                "input_tokens": 100,
                "cache_read_tokens": 50,
                "output_tokens": 20,
                "modelName": "gemini-2.5-pro"
            }),
            // Duplicate step 1
            json!({
                "type": "PLANNER_RESPONSE",
                "step_index": 1,
                "input_tokens": 100,
                "cache_read_tokens": 50,
                "output_tokens": 20
            }),
            json!({
                "type": "PLANNER_RESPONSE",
                "step_index": 2,
                "input_tokens": 200,
                "cache_read_tokens": 100,
                "output_tokens": 40
            }),
        ];
        let mut content = String::new();
        for l in lines {
            content.push_str(&serde_json::to_string(&l).unwrap());
            content.push('\n');
        }
        fs::write(&transcript, &content).unwrap();

        let cancellation = CollectionCancellation::default();
        let request = CollectSessionDataRequest {
            home: home.to_path_buf(),
            key: ProviderSessionKey {
                configured_provider: "agy".into(),
                adapter_identity: ProviderId::Agy,
                provider_session_id: session_id.to_string(),
            },
            locator: transcript.clone(),
            prior_cursor: None,
            cancellation: cancellation.clone(),
        };

        let result = scan(&request).unwrap();
        match result {
            Collected::Complete { enrichment, cursor } => {
                let usage = enrichment.usage.unwrap();
                // Step 1: 100 + 50 = 150 input, 20 output
                // Step 2: 200 + 100 = 300 input, 40 output
                // Total input: 450, total output: 60, context (latest step input): 300
                assert_eq!(usage.input_tokens, Some(450));
                assert_eq!(usage.output_tokens, Some(60));
                assert_eq!(usage.context_tokens, Some(300));
                assert_eq!(enrichment.model.as_deref(), Some("gemini-2.5-pro"));
                assert_eq!(enrichment.model_label.as_deref(), Some("2.5-pro"));

                // Incremental collection returns Unchanged
                let next_req = CollectSessionDataRequest {
                    home: home.to_path_buf(),
                    key: ProviderSessionKey {
                        configured_provider: "agy".into(),
                        adapter_identity: ProviderId::Agy,
                        provider_session_id: session_id.to_string(),
                    },
                    locator: transcript.clone(),
                    prior_cursor: Some(cursor),
                    cancellation: cancellation.clone(),
                };
                let next_res = scan(&next_req).unwrap();
                assert!(matches!(next_res, Collected::Unchanged { .. }));
            }
            Collected::Unchanged { .. } => panic!("expected Complete on first scan"),
        }

        // Test cancellation
        cancellation.cancel();
        let cancel_req = CollectSessionDataRequest {
            home: home.to_path_buf(),
            key: ProviderSessionKey {
                configured_provider: "agy".into(),
                adapter_identity: ProviderId::Agy,
                provider_session_id: session_id.to_string(),
            },
            locator: transcript.clone(),
            prior_cursor: None,
            cancellation,
        };
        assert!(AgyCollector.collect(&cancel_req).is_err());
    }

    #[test]
    fn collector_validates_path_and_session() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();
        let session_id = "sess-1";
        let other_session = "sess-2";
        let brain_dir = home
            .join(".gemini/antigravity-cli/brain")
            .join(other_session)
            .join(".system_generated/logs");
        fs::create_dir_all(&brain_dir).unwrap();
        let transcript = brain_dir.join("transcript.jsonl");
        fs::write(&transcript, "{}\n").unwrap();

        // Mismatched session id
        let request = CollectSessionDataRequest {
            home: home.to_path_buf(),
            key: ProviderSessionKey {
                configured_provider: "agy".into(),
                adapter_identity: ProviderId::Agy,
                provider_session_id: session_id.to_string(),
            },
            locator: transcript.clone(),
            prior_cursor: None,
            cancellation: CollectionCancellation::default(),
        };
        let err = scan(&request).err().unwrap();
        assert!(
            err.to_string()
                .contains("does not belong to the authenticated session")
        );

        // Path outside brain root
        let outside = temp.path().join("outside.jsonl");
        fs::write(&outside, "{}\n").unwrap();
        let outside_req = CollectSessionDataRequest {
            home: home.to_path_buf(),
            key: ProviderSessionKey {
                configured_provider: "agy".into(),
                adapter_identity: ProviderId::Agy,
                provider_session_id: session_id.to_string(),
            },
            locator: outside,
            prior_cursor: None,
            cancellation: CollectionCancellation::default(),
        };
        assert!(scan(&outside_req).is_err());
    }
}
