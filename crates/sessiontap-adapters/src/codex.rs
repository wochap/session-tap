use crate::{
    AgentAdapter, SetupAction, SetupReport, bounded_field, completed_reason_context,
    is_subagent_payload, merge_hook_config, provider_metadata, sanitize_bounded,
    status_reason_context,
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
    "PermissionRequest",
    "UserInputRequest",
    "PostToolUse",
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
    fn normalize(&self, id: &InvocationId, raw: &Value) -> Result<AdapterOutcome> {
        if is_subagent_payload(raw) {
            return Ok(AdapterOutcome::Ignored);
        }
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
        let kind = if name.contains("sessionstart") {
            EventKind::ProviderSessionStarted
        } else if name.contains("sessionend") {
            EventKind::ProviderSessionEnded
        } else if name.contains("userprompt")
            || name.contains("promptsubmit")
            || name.contains("turnstart")
        {
            EventKind::NewTurn
        } else if name.contains("permission") || notification.contains("permission_prompt") {
            EventKind::WaitingApproval
        } else if name.contains("question")
            || name.contains("elicitation")
            || name.contains("needs_input")
            || name.contains("userinputrequest")
            || notification.contains("needs_input")
        {
            EventKind::WaitingInput
        } else if name.contains("pretool")
            || name.contains("posttool")
            || name.contains("toolstart")
            || name.contains("toolend")
        {
            EventKind::Working
        } else if name == "stop" || name.contains("turnend") {
            EventKind::Completed
        } else {
            EventKind::Enrichment
        };
        Ok(AdapterOutcome::Event(Box::new(build(id, raw, kind))))
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

fn build(id: &InvocationId, raw: &Value, kind: EventKind) -> NormalizedAdapterEvent {
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
            source: "hook".into(),
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
        },
        status_reason,
    }
}
