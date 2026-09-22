//! The one `AgentAdapter` implementation, composed from a provider's hook
//! dialect and session collector.

use crate::{
    AgentAdapter, BoundedDiagnostic, CollectSessionDataRequest, CollectionOutcome,
    LaunchPreparation, SetupAction, SetupReport,
    artifact::{CollectError, Collected, SessionCollector},
    dialect::{HookDialect, NormalizeContext},
};
use anyhow::Result;
use async_trait::async_trait;
use chrono::Utc;
use serde_json::Value;
use sessiontap_core::{
    ProviderId,
    domain::{
        AdapterOutcome, EventEvidence, EventKind, InvocationId, NormalizedAdapterEvent,
        NormalizedEvent,
    },
};
use std::path::Path;
use uuid::Uuid;

/// Installs, checks, or removes the provider's managed hooks.
pub type SetupFn = fn(home: &Path, executable: &Path, action: SetupAction) -> Result<SetupReport>;

/// Prepares extra launch arguments and an optional side channel.
pub type LaunchFn =
    fn(args: &[String], private_dir: &Path, executable: &Path) -> Result<LaunchPreparation>;

fn no_launch_preparation(
    _args: &[String],
    _private_dir: &Path,
    _executable: &Path,
) -> Result<LaunchPreparation> {
    Ok(LaunchPreparation::default())
}

pub struct HookAdapter<D, C> {
    dialect: D,
    collector: C,
    setup: SetupFn,
    prepare_launch: LaunchFn,
}

impl<D, C> HookAdapter<D, C> {
    pub const fn new(dialect: D, collector: C, setup: SetupFn) -> Self {
        Self {
            dialect,
            collector,
            setup,
            prepare_launch: no_launch_preparation,
        }
    }

    #[must_use]
    pub const fn with_launch(mut self, prepare_launch: LaunchFn) -> Self {
        self.prepare_launch = prepare_launch;
        self
    }
}

impl<D: HookDialect, C> HookAdapter<D, C> {
    fn build(
        &self,
        id: &InvocationId,
        raw: &Value,
        kind: EventKind,
        evidence: EventEvidence,
        context: &NormalizeContext<'_>,
    ) -> NormalizedAdapterEvent {
        let dialect = &self.dialect;
        let received_at = Utc::now();
        let provider_event_id = dialect.provider_event_id(raw);
        let status_reason = match kind {
            EventKind::WaitingApproval => dialect.approval_reason(raw),
            EventKind::WaitingInput => dialect.input_reason(raw),
            EventKind::Completed => dialect.completed_reason(raw),
            EventKind::Failed => dialect.failure_reason(raw),
            _ => None,
        };
        NormalizedAdapterEvent {
            event: NormalizedEvent {
                schema_version: sessiontap_core::SCHEMA_VERSION,
                event_id: provider_event_id
                    .clone()
                    .unwrap_or_else(|| Uuid::new_v4().to_string()),
                invocation_id: id.clone(),
                provider_event_id,
                provider: dialect.id().as_str().into(),
                observed_at: dialect.observed_at(raw).unwrap_or(received_at),
                received_at,
                evidence,
                provider_session_id: dialect.provider_session_id(raw),
                provider_session_name: dialect.session_name(raw),
                provider_session_start_reason: (kind == EventKind::ProviderSessionStarted)
                    .then(|| dialect.start_reason(raw))
                    .flatten(),
                provider_metadata: dialect.metadata(raw),
                usage: dialect.inline_usage(raw),
                turn_id: dialect.turn_id(raw),
                tool_activity: dialect.tool_activity(raw, context),
                kind,
            },
            status_reason,
            collection_context: dialect.collection_context(raw),
        }
    }
}

#[async_trait]
impl<D: HookDialect, C: SessionCollector + Clone> AgentAdapter for HookAdapter<D, C> {
    fn provider_id(&self) -> ProviderId {
        self.dialect.id()
    }

    fn prepare_launch(
        &self,
        args: &[String],
        private_dir: &Path,
        executable: &Path,
    ) -> Result<LaunchPreparation> {
        (self.prepare_launch)(args, private_dir, executable)
    }

    fn normalize_with_evidence(
        &self,
        id: &InvocationId,
        raw: &Value,
        evidence: EventEvidence,
        context: &NormalizeContext<'_>,
    ) -> Result<AdapterOutcome> {
        if self.dialect.is_subagent(raw) {
            return Ok(AdapterOutcome::Ignored);
        }
        let Some(kind) = self.dialect.classify(raw) else {
            return Ok(AdapterOutcome::Ignored);
        };
        Ok(AdapterOutcome::Event(Box::new(
            self.build(id, raw, kind, evidence, context),
        )))
    }

    async fn collect_session_data(&self, request: CollectSessionDataRequest) -> CollectionOutcome {
        if !C::COLLECTS {
            return CollectionOutcome::Unsupported;
        }
        let collector = self.collector.clone();
        match tokio::task::spawn_blocking(move || collector.collect(&request)).await {
            Ok(Ok(Collected::Complete { enrichment, cursor })) => {
                CollectionOutcome::Complete { enrichment, cursor }
            }
            Ok(Ok(Collected::Unchanged { cursor })) => CollectionOutcome::Unchanged { cursor },
            Ok(Err(CollectError::Cancelled)) => CollectionOutcome::Cancelled,
            Ok(Err(CollectError::Failed(error))) => {
                CollectionOutcome::Failed(BoundedDiagnostic::new(format!("{error:#}")))
            }
            Err(error) => CollectionOutcome::Failed(BoundedDiagnostic::new(error.to_string())),
        }
    }

    async fn setup(
        &self,
        home: &Path,
        executable: &Path,
        action: SetupAction,
    ) -> Result<SetupReport> {
        (self.setup)(home, executable, action)
    }
}

#[cfg(test)]
impl<D: HookDialect, C: SessionCollector + Clone> HookAdapter<D, C> {
    /// Test helper for cases that expect a normalized root event.
    pub fn normalize(&self, id: &InvocationId, raw: &Value) -> Result<NormalizedAdapterEvent> {
        <Self as AgentAdapter>::normalize(self, id, raw)?
            .into_event()
            .ok_or_else(|| anyhow::anyhow!("payload was ignored"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        CollectionCancellation, OpaqueCursor, ProviderSessionKey, SessionEnrichment,
        artifact::{NoCollector, check_cancelled},
    };
    use serde_json::json;
    use sessiontap_core::domain::{ProviderMetadata, ToolActivityUpdate};
    use std::path::PathBuf;

    struct TestDialect;

    impl HookDialect for TestDialect {
        fn id(&self) -> ProviderId {
            ProviderId::Codex
        }
        fn classify(&self, raw: &Value) -> Option<EventKind> {
            match raw.get("kind")?.as_str()? {
                "start" => Some(EventKind::ProviderSessionStarted),
                "approve" => Some(EventKind::WaitingApproval),
                "done" => Some(EventKind::Completed),
                _ => None,
            }
        }
        fn start_reason(&self, raw: &Value) -> Option<String> {
            raw.get("why").and_then(Value::as_str).map(str::to_owned)
        }
        fn metadata(&self, _raw: &Value) -> Option<ProviderMetadata> {
            None
        }
        fn tool_activity(
            &self,
            _raw: &Value,
            context: &NormalizeContext<'_>,
        ) -> Option<ToolActivityUpdate> {
            context.workspace.map(|workspace| ToolActivityUpdate {
                phase: sessiontap_core::domain::ToolActivityPhase::Start,
                label: workspace.to_string_lossy().into_owned(),
                correlation_id: None,
                detail: None,
            })
        }
    }

    #[derive(Clone)]
    enum TestCollector {
        Complete,
        Unchanged,
        Cancelled,
        Failed,
    }

    impl SessionCollector for TestCollector {
        fn collect(&self, request: &CollectSessionDataRequest) -> Result<Collected, CollectError> {
            check_cancelled(request)?;
            match self {
                Self::Complete => Ok(Collected::Complete {
                    enrichment: SessionEnrichment {
                        session_name: Some("n".into()),
                        usage: None,
                    },
                    cursor: OpaqueCursor::new(1_u8),
                }),
                Self::Unchanged => Ok(Collected::Unchanged {
                    cursor: OpaqueCursor::new(2_u8),
                }),
                Self::Cancelled => Err(CollectError::Cancelled),
                Self::Failed => Err(anyhow::anyhow!("inner").context("outer").into()),
            }
        }
    }

    fn setup(_home: &Path, _executable: &Path, _action: SetupAction) -> Result<SetupReport> {
        Ok(SetupReport {
            changed: false,
            healthy: true,
            message: "test".into(),
        })
    }

    fn request() -> CollectSessionDataRequest {
        CollectSessionDataRequest {
            home: PathBuf::from("/nonexistent"),
            key: ProviderSessionKey {
                configured_provider: "alias".into(),
                adapter_identity: ProviderId::Codex,
                provider_session_id: "s".into(),
            },
            locator: PathBuf::from("/nonexistent"),
            prior_cursor: None,
            cancellation: CollectionCancellation::default(),
        }
    }

    #[test]
    fn driver_builds_events_from_dialect_components() {
        let adapter = HookAdapter::new(TestDialect, NoCollector, setup);
        let id = InvocationId::new();
        assert_eq!(adapter.provider_id(), ProviderId::Codex);
        assert_eq!(adapter.dialect(), ProviderId::Codex.as_str());

        let ignored = [
            json!({"kind": "other"}),
            json!({"kind": "done", "agent_id": "child"}),
        ];
        for raw in ignored {
            assert_eq!(
                AgentAdapter::normalize(&adapter, &id, &raw).unwrap(),
                AdapterOutcome::Ignored
            );
        }

        let started = adapter
            .normalize(
                &id,
                &json!({"kind": "start", "why": "resume", "event_id": "e1", "session_id": "s"}),
            )
            .unwrap();
        assert_eq!(started.event.event_id, "e1");
        assert_eq!(started.event.provider_event_id.as_deref(), Some("e1"));
        assert_eq!(started.event.provider, "codex");
        assert_eq!(started.event.provider_session_id.as_deref(), Some("s"));
        assert_eq!(
            started.event.provider_session_start_reason.as_deref(),
            Some("resume")
        );
        assert_eq!(started.event.observed_at, started.event.received_at);
        assert!(started.status_reason.is_none());

        let approval = adapter
            .normalize(
                &id,
                &json!({"kind": "approve", "why": "resume", "tool_name": "Bash"}),
            )
            .unwrap();
        assert!(approval.event.provider_session_start_reason.is_none());
        assert!(approval.event.provider_event_id.is_none());
        assert!(uuid::Uuid::parse_str(&approval.event.event_id).is_ok());
        assert_eq!(approval.status_reason.unwrap().summary, "bash");

        let done = adapter
            .normalize(
                &id,
                &json!({"kind": "done", "last_assistant_message": "finished"}),
            )
            .unwrap();
        assert_eq!(done.status_reason.unwrap().summary, "finished");
    }

    #[test]
    fn normalization_context_reaches_the_dialect() {
        let adapter = HookAdapter::new(TestDialect, NoCollector, setup);
        let id = InvocationId::new();
        let raw = json!({"kind": "done"});
        let bound = adapter
            .normalize_with_evidence(
                &id,
                &raw,
                EventEvidence::managed_hook(1),
                &NormalizeContext {
                    workspace: Some(Path::new("/work")),
                },
            )
            .unwrap()
            .into_event()
            .unwrap();
        assert_eq!(bound.event.tool_activity.unwrap().label, "/work");
        assert!(
            adapter
                .normalize(&id, &raw)
                .unwrap()
                .event
                .tool_activity
                .is_none()
        );
    }

    #[tokio::test]
    async fn collection_outcomes_are_mapped_once() {
        let outcome = |collector| async move {
            HookAdapter::new(TestDialect, collector, setup)
                .collect_session_data(request())
                .await
        };
        assert!(matches!(
            outcome(TestCollector::Complete).await,
            CollectionOutcome::Complete { enrichment, .. } if enrichment.session_name.as_deref() == Some("n")
        ));
        assert!(matches!(
            outcome(TestCollector::Unchanged).await,
            CollectionOutcome::Unchanged { .. }
        ));
        assert!(matches!(
            outcome(TestCollector::Cancelled).await,
            CollectionOutcome::Cancelled
        ));
        match outcome(TestCollector::Failed).await {
            CollectionOutcome::Failed(diagnostic) => {
                assert_eq!(diagnostic.message(), "outer: inner")
            }
            _ => panic!("expected failure"),
        }

        let cancelled = request();
        cancelled.cancellation.cancel();
        assert!(matches!(
            HookAdapter::new(TestDialect, TestCollector::Complete, setup)
                .collect_session_data(cancelled)
                .await,
            CollectionOutcome::Cancelled
        ));

        assert!(matches!(
            HookAdapter::new(TestDialect, NoCollector, setup)
                .collect_session_data(request())
                .await,
            CollectionOutcome::Unsupported
        ));
    }

    #[tokio::test]
    async fn setup_and_launch_are_delegated() {
        fn launch(args: &[String], dir: &Path, executable: &Path) -> Result<LaunchPreparation> {
            Ok(LaunchPreparation {
                extra_args: vec![
                    args.len().to_string(),
                    dir.display().to_string(),
                    executable.display().to_string(),
                ],
                ..LaunchPreparation::default()
            })
        }
        let plain = HookAdapter::new(TestDialect, NoCollector, setup);
        assert!(
            plain
                .prepare_launch(&[], Path::new("/d"), Path::new("/x"))
                .unwrap()
                .extra_args
                .is_empty()
        );
        let custom = HookAdapter::new(TestDialect, NoCollector, setup).with_launch(launch);
        assert_eq!(
            custom
                .prepare_launch(&["a".into()], Path::new("/d"), Path::new("/x"))
                .unwrap()
                .extra_args,
            vec!["1", "/d", "/x"]
        );
        let report = custom
            .setup(Path::new("/h"), Path::new("/x"), SetupAction::Doctor)
            .await
            .unwrap();
        assert_eq!(report.message, "test");
    }
}
