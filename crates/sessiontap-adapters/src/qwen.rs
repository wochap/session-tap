use crate::{
    AgentAdapter, LaunchPreparation, SetupAction, SetupReport, bounded_field,
    completed_reason_context, failed_reason_context, is_subagent_payload, merge_hook_config,
    probe_qwen_dual_output, provider_metadata, qwen_has_user_side_channel, sanitize_bounded,
    status_reason_context, tool_activity_update,
};
use anyhow::Result;
use async_trait::async_trait;
use chrono::Utc;
use serde_json::Value;
use sessiontap_core::domain::{
    AdapterOutcome, EventEvidence, EventKind, InvocationId, NormalizedAdapterEvent,
    NormalizedEvent, Usage,
};
use std::path::Path;
use uuid::Uuid;

pub use crate::JsonlTail as QwenJsonlTail;
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
        })))
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
