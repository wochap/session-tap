use crate::{
    AgentAdapter, SetupAction, SetupReport, bounded_field, completed_reason_context,
    is_subagent_payload, merge_hook_config, provider_metadata, sanitize_bounded,
    status_reason_context, tool_activity_update,
};
use anyhow::Result;
use async_trait::async_trait;
use chrono::Utc;
use serde_json::Value;
use sessiontap_core::domain::{
    AdapterOutcome, EventEvidence, EventKind, InvocationId, NormalizedAdapterEvent, NormalizedEvent,
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
    }
}
