//! Per-provider interpretation of raw hook payloads.

use crate::{completed_reason_context, failed_reason_context, status_reason_context};
use chrono::{DateTime, Utc};
use serde_json::Value;
use sessiontap_core::{
    ProviderId,
    domain::{
        ArtifactCollectionContext, ChildAgentRef, EventKind, ProviderMetadata, StatusReasonContext,
        ToolActivityUpdate, Usage,
    },
};
use std::path::Path;

/// Trusted inputs supplied by the authenticated launch, never by the payload.
#[derive(Debug, Clone, Copy, Default)]
pub struct NormalizeContext<'a> {
    /// Invocation workspace that bounds workspace-relative tool targets.
    pub workspace: Option<&'a Path>,
}

/// How one provider's hook payloads map onto normalized events. Every method
/// is pure over the raw payload; the shared driver assembles the event.
pub trait HookDialect: Send + Sync + 'static {
    fn id(&self) -> ProviderId;

    /// Maps a payload to an event kind, or `None` to ignore it.
    fn classify(&self, raw: &Value) -> Option<EventKind>;

    /// Sanitized bounded identity of the child agent a payload belongs to.
    /// `None` treats the payload as a root payload; a provider that does not
    /// track child agents must ignore their payloads in `classify`.
    fn child_agent(&self, _raw: &Value) -> Option<ChildAgentRef> {
        None
    }

    /// Provider-assigned event id; the driver generates one when absent.
    fn provider_event_id(&self, raw: &Value) -> Option<String> {
        raw.get("event_id")
            .and_then(Value::as_str)
            .map(str::to_owned)
    }

    /// Provider-reported observation time; defaults to receipt time.
    fn observed_at(&self, _raw: &Value) -> Option<DateTime<Utc>> {
        None
    }

    fn provider_session_id(&self, raw: &Value) -> Option<String> {
        raw.get("session_id")
            .and_then(Value::as_str)
            .map(str::to_owned)
    }

    fn session_name(&self, _raw: &Value) -> Option<String> {
        None
    }

    /// Start reason for a provider session start, from the provider's
    /// allowlist. Only consulted for `ProviderSessionStarted`.
    fn start_reason(&self, raw: &Value) -> Option<String>;

    fn metadata(&self, raw: &Value) -> Option<ProviderMetadata>;

    fn inline_usage(&self, _raw: &Value) -> Option<Usage> {
        None
    }

    fn turn_id(&self, _raw: &Value) -> Option<String> {
        None
    }

    fn approval_reason(&self, raw: &Value) -> Option<StatusReasonContext> {
        status_reason_context(raw, false)
    }

    fn input_reason(&self, raw: &Value) -> Option<StatusReasonContext> {
        status_reason_context(raw, true)
    }

    fn completed_reason(&self, raw: &Value) -> Option<StatusReasonContext> {
        completed_reason_context(raw)
    }

    fn failure_reason(&self, raw: &Value) -> Option<StatusReasonContext> {
        failed_reason_context(raw)
    }

    fn tool_activity(
        &self,
        raw: &Value,
        context: &NormalizeContext<'_>,
    ) -> Option<ToolActivityUpdate>;

    /// Private artifact locator for the provider's session collector.
    fn collection_context(&self, _raw: &Value) -> Option<ArtifactCollectionContext> {
        None
    }
}
