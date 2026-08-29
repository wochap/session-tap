use crate::{
    AgentAdapter, SetupAction, SetupReport, bounded_field, claude_session_name,
    completed_reason_context, failed_reason_context, is_subagent_payload, merge_hook_config,
    provider_metadata, sanitize_bounded, status_reason_context,
};
use anyhow::Result;
use async_trait::async_trait;
use chrono::Utc;
use serde_json::Value;
use sessiontap_core::domain::{
    AdapterOutcome, EventKind, InvocationId, NormalizedAdapterEvent, NormalizedEvent,
};
use std::path::Path;
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
    fn normalize(&self, id: &InvocationId, raw: &Value) -> Result<AdapterOutcome> {
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
                source: "hook".into(),
                kind: kind.clone(),
                provider_session_id: raw
                    .get("session_id")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                provider_session_name: claude_session_name(id, raw),
                provider_session_start_reason: (kind == EventKind::ProviderSessionStarted)
                    .then(|| bounded_field(raw, &["source", "reason", "start_reason"], 32))
                    .flatten()
                    .filter(|v| matches!(v.as_str(), "startup" | "clear" | "resume" | "compact")),
                provider_metadata: provider_metadata(raw, Some("prompt_id")),
                usage: None,
                turn_id,
            },
            status_reason,
        })))
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
