use crate::{
    AgentAdapter, SetupAction, SetupReport, attention_context, bounded_field, claude_session_name,
    failure_context, merge_hook_config, provider_metadata, sanitize_bounded,
};
use anyhow::Result;
use async_trait::async_trait;
use chrono::Utc;
use serde_json::Value;
use sessiontap_core::domain::{EventKind, InvocationId, NormalizedAdapterEvent, NormalizedEvent};
use std::path::Path;
use uuid::Uuid;

pub const HOOK_EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PermissionRequest",
    "Elicitation",
    "Notification",
    "Stop",
    "StopFailure",
    "SessionEnd",
];
pub struct ClaudeAdapter;

#[async_trait]
impl AgentAdapter for ClaudeAdapter {
    fn dialect(&self) -> &'static str {
        "claude"
    }
    fn normalize(&self, id: &InvocationId, raw: &Value) -> Result<NormalizedAdapterEvent> {
        let name = raw
            .get("hook_event_name")
            .or_else(|| raw.get("event_name"))
            .or_else(|| raw.get("type"))
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_ascii_lowercase();
        let notification = raw
            .get("notification_type")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_ascii_lowercase();
        let ask = name.contains("permission")
            && raw
                .get("tool_name")
                .and_then(Value::as_str)
                .is_some_and(|tool| {
                    tool.eq_ignore_ascii_case("AskUserQuestion")
                        || tool.eq_ignore_ascii_case("ask_user_question")
                });
        let kind = if name.contains("sessionstart") {
            EventKind::ProviderSessionStarted
        } else if name.contains("sessionend") {
            EventKind::ProviderSessionEnded
        } else if name.contains("userprompt")
            || name.contains("promptsubmit")
            || name.contains("turnstart")
        {
            EventKind::NewTurn
        } else if ask {
            EventKind::WaitingInput
        } else if name.contains("permission") || notification.contains("permission_prompt") {
            EventKind::WaitingApproval
        } else if name.contains("question")
            || name.contains("elicitation")
            || name.contains("needs_input")
            || notification.contains("needs_input")
        {
            EventKind::WaitingInput
        } else if name.contains("pretool")
            || name.contains("posttool")
            || name.contains("toolstart")
            || name.contains("toolend")
        {
            EventKind::Working
        } else if name.contains("failure") {
            EventKind::Failed
        } else if name == "stop" || name.contains("turnend") {
            EventKind::Completed
        } else {
            EventKind::Enrichment
        };
        let now = Utc::now();
        let attention = match kind {
            EventKind::WaitingApproval => attention_context(raw, false),
            EventKind::WaitingInput => attention_context(raw, true),
            _ => None,
        };
        let failure = (kind == EventKind::Failed).then(|| failure_context(raw));
        let turn_id = raw
            .get("turn_id")
            .or_else(|| raw.get("prompt_id"))
            .and_then(Value::as_str)
            .and_then(|v| sanitize_bounded(v, 128));
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
                provider: "claude".into(),
                observed_at: now,
                received_at: now,
                source: "hook".into(),
                kind: kind.clone(),
                provider_session_id: raw
                    .get("session_id")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                provider_session_name: claude_session_name(raw),
                provider_session_start_reason: (kind == EventKind::ProviderSessionStarted)
                    .then(|| bounded_field(raw, &["source", "reason", "start_reason"], 32))
                    .flatten()
                    .filter(|v| matches!(v.as_str(), "startup" | "clear" | "resume" | "compact")),
                provider_metadata: provider_metadata(raw, Some("prompt_id")),
                usage: None,
                turn_id,
            },
            attention,
            failure,
        })
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
