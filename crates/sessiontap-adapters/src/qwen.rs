use crate::{
    AgentAdapter, LaunchPreparation, SetupAction, SetupReport, bounded_field,
    completed_reason_context, failed_reason_context, is_subagent_payload, merge_hook_config,
    probe_qwen_dual_output, provider_metadata, qwen_has_user_side_channel, sanitize_bounded,
    status_reason_context,
};
use anyhow::Result;
use async_trait::async_trait;
use chrono::Utc;
use serde_json::Value;
use sessiontap_core::domain::{
    AdapterOutcome, EventKind, InvocationId, NormalizedAdapterEvent, NormalizedEvent, Usage,
};
use std::path::Path;
use uuid::Uuid;

pub use crate::JsonlTail as QwenJsonlTail;
pub const HOOK_EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PermissionRequest",
    "AskUserQuestion",
    "Notification",
    "Stop",
    "StopFailure",
    "SubagentStop",
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
        let ask = name.contains("permission")
            && raw
                .get("tool_name")
                .and_then(Value::as_str)
                .is_some_and(|tool| {
                    tool.eq_ignore_ascii_case("AskUserQuestion")
                        || tool.eq_ignore_ascii_case("ask_user_question")
                });
        let empty_prompt = (name.contains("userprompt") || name.contains("promptsubmit"))
            && raw
                .get("prompt")
                .and_then(Value::as_str)
                .is_some_and(|prompt| prompt.trim().is_empty());
        let kind = if name.contains("sessionstart") {
            EventKind::ProviderSessionStarted
        } else if name.contains("sessionend") {
            EventKind::ProviderSessionEnded
        } else if notification == "idle_prompt" {
            EventKind::Idle
        } else if empty_prompt {
            EventKind::Enrichment
        } else if name.contains("userprompt")
            || name.contains("promptsubmit")
            || name.contains("turnstart")
        {
            EventKind::NewTurn
        } else if ask {
            EventKind::WaitingInput
        } else if name == "notification" && notification == "permission_prompt" {
            EventKind::Enrichment
        } else if name.contains("permission") {
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
        } else if name.contains("failure") {
            EventKind::Failed
        } else if name == "stop" || name.contains("turnend") {
            EventKind::Completed
        } else {
            EventKind::Enrichment
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
                usage,
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
            &home.join(".qwen/settings.json"),
            "qwen",
            HOOK_EVENTS,
            executable,
            action,
        )
    }
}
