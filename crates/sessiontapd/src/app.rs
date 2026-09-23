//! Request-level daemon service. Every socket request maps to one method so
//! behavior is testable without a socket.

use crate::usage_coordinator::{EnrichmentApplier, UsageCoordinator};
use anyhow::{Context, Result, bail};
use sessiontap_adapters::AdapterRegistry;
use sessiontap_core::{
    config::{DaemonConfig, SinkConfig},
    domain::{
        ArtifactCollectionContext, InvocationId, InvocationSnapshot, NormalizedEvent,
        PublicAgentView, StatusReasonContext, changed_public_fields, project_public,
    },
};
use sessiontap_infra::multiplexer::MultiplexerRegistry;
use sessiontap_storage::{AppliedUpdate, Publish, Storage};
use std::{collections::BTreeMap, path::PathBuf, sync::Arc};
use tokio::sync::broadcast;

/// Sink delivery context captured once from configuration.
#[derive(Debug, Clone, Default)]
pub struct PublishConfig {
    pub sinks: BTreeMap<String, SinkConfig>,
    pub source_id: String,
    pub source_name: Option<String>,
}

impl PublishConfig {
    #[must_use]
    pub fn publish(&self) -> Publish<'_> {
        Publish {
            sinks: &self.sinks,
            source_id: &self.source_id,
            source_name: self.source_name.as_deref(),
        }
    }
}

/// Shared state behind [`App`]. The usage coordinator holds this directly as
/// its [`EnrichmentApplier`]; the coordinator itself lives in `App`, so there
/// is no reference cycle.
pub struct AppCore {
    storage: Arc<Storage>,
    updates: broadcast::Sender<AppliedUpdate>,
    publish: PublishConfig,
    multiplexers: Arc<MultiplexerRegistry>,
}

impl AppCore {
    fn broadcast(&self, update: Option<AppliedUpdate>) {
        if let Some(update) = update {
            let _ = self.updates.send(update);
        }
    }
}

impl EnrichmentApplier for AppCore {
    fn apply_enrichment(
        &self,
        invocation: &InvocationId,
        credential: &str,
        event: NormalizedEvent,
    ) -> Result<()> {
        if !self
            .storage
            .credential_matches(invocation, &event.provider, credential)?
        {
            return Ok(());
        }
        let snapshot = self.storage.invocation(invocation)?;
        if snapshot.provider != event.provider
            || snapshot
                .provider_session
                .as_ref()
                .map(|session| session.id.as_str())
                != event.provider_session_id.as_deref()
        {
            return Ok(());
        }
        let publish = self.publish.publish();
        let update = self
            .storage
            .apply_event_with_context(&event, None, Some(&publish))?;
        self.broadcast(update);
        Ok(())
    }
}

/// Artifact collection inputs for the usage coordinator.
pub struct Collection {
    pub home: PathBuf,
    pub registry: Arc<AdapterRegistry>,
}

#[derive(Clone)]
pub struct App {
    core: Arc<AppCore>,
    usage: UsageCoordinator,
}

impl App {
    #[must_use]
    pub fn new(
        storage: Arc<Storage>,
        publish: PublishConfig,
        daemon: &DaemonConfig,
        multiplexers: Arc<MultiplexerRegistry>,
        collection: Collection,
    ) -> Self {
        let (updates, _) = broadcast::channel(daemon.update_buffer.max(1));
        let core = Arc::new(AppCore {
            storage,
            updates,
            publish,
            multiplexers,
        });
        let usage = UsageCoordinator::new(
            core.clone(),
            collection.home,
            collection.registry,
            daemon.collection_workers,
        );
        Self { core, usage }
    }

    #[must_use]
    pub fn storage(&self) -> &Arc<Storage> {
        &self.core.storage
    }

    #[must_use]
    pub fn publish_config(&self) -> &PublishConfig {
        &self.core.publish
    }

    pub fn status(&self) -> Result<(u64, Vec<PublicAgentView>)> {
        self.core.storage.public_snapshot()
    }

    pub fn register(&self, snapshot: InvocationSnapshot, credential: &str) -> Result<()> {
        let publish = self.core.publish.publish();
        let revision = self
            .core
            .storage
            .register(&snapshot, credential, Some(&publish))?;
        let mut registered = snapshot;
        registered.revision = revision;
        let view = project_public(&registered, None);
        self.core.broadcast(Some(AppliedUpdate {
            revision,
            delivery_id: format!("synthetic:register:{}:{revision}", registered.invocation_id),
            changed: changed_public_fields(None, &view),
            view,
        }));
        Ok(())
    }

    pub fn bind_child(
        &self,
        invocation_id: &InvocationId,
        credential: &str,
        child_pid: u32,
        start_identity: Option<String>,
    ) -> Result<()> {
        let publish = self.core.publish.publish();
        let update = self.core.storage.bind_child(
            invocation_id,
            credential,
            child_pid,
            start_identity,
            Some(&publish),
        )?;
        self.core.broadcast(update);
        Ok(())
    }

    pub fn lifecycle_exit(
        &self,
        invocation_id: &InvocationId,
        credential: &str,
        exit_code: Option<i32>,
        signal: Option<i32>,
    ) -> Result<()> {
        let publish = self.core.publish.publish();
        let update = self.core.storage.mark_exit(
            invocation_id,
            credential,
            exit_code,
            signal,
            Some(&publish),
        )?;
        self.core.broadcast(update);
        Ok(())
    }

    /// Validates the hook context, applies the event, and schedules artifact
    /// collection when the hook carried a collection context.
    pub fn ingest_hook(
        &self,
        provider: String,
        invocation_id: InvocationId,
        credential: String,
        mut event: NormalizedEvent,
        status_reason: Option<StatusReasonContext>,
        collection_context: Option<ArtifactCollectionContext>,
    ) -> Result<()> {
        if event.invocation_id != invocation_id
            || event.provider != provider
            || collection_context.as_ref().is_some_and(|context| {
                event.provider_session_id.as_deref() != Some(context.provider_session_id.as_str())
            })
            || !self
                .core
                .storage
                .credential_matches(&invocation_id, &provider, &credential)?
        {
            bail!("unknown or invalid hook context");
        }
        event.received_at = chrono::Utc::now();
        let publish = self.core.publish.publish();
        let update = self.core.storage.apply_event_with_context(
            &event,
            status_reason.as_ref(),
            Some(&publish),
        )?;
        self.core.broadcast(update);
        self.usage
            .schedule(provider, invocation_id, credential, collection_context);
        Ok(())
    }

    /// Captures the pane through the adapter for the invocation's recorded
    /// backend; an unregistered backend fails before any command runs.
    pub fn capture(&self, invocation_id: &InvocationId) -> Result<String> {
        let (metadata, pid) = self.multiplexer_target(invocation_id)?;
        self.core
            .multiplexers
            .require(metadata.backend)?
            .capture(&metadata, pid)
    }

    pub fn send_input(&self, invocation_id: &InvocationId, text: &str) -> Result<()> {
        let (metadata, pid) = self.multiplexer_target(invocation_id)?;
        self.core
            .multiplexers
            .require(metadata.backend)?
            .send_input(&metadata, pid, text.as_bytes())
    }

    fn multiplexer_target(
        &self,
        invocation_id: &InvocationId,
    ) -> Result<(sessiontap_core::domain::MultiplexerMetadata, u32)> {
        let snapshot = self.core.storage.invocation(invocation_id)?;
        let metadata = snapshot
            .multiplexer
            .context("invocation is not in a multiplexer")?;
        let pid = snapshot
            .process
            .child_pid
            .context("invocation has no child PID")?;
        Ok((metadata, pid))
    }

    /// Subscribes before reading the snapshot so no committed update between
    /// the two can be missed; callers skip updates at or below the returned
    /// revision.
    pub fn subscribe(
        &self,
    ) -> Result<(
        u64,
        Vec<PublicAgentView>,
        broadcast::Receiver<AppliedUpdate>,
    )> {
        let receiver = self.core.updates.subscribe();
        let (revision, views) = self.core.storage.public_snapshot()?;
        Ok((revision, views, receiver))
    }

    pub fn reconcile(
        &self,
        is_alive: impl Fn(u32, Option<&str>) -> bool,
        retention_days: u64,
    ) -> Result<usize> {
        let publish = self.core.publish.publish();
        self.core
            .storage
            .reconcile(is_alive, retention_days, Some(&publish))
    }

    pub fn expire_stale_working(&self, now: chrono::DateTime<chrono::Utc>) -> Result<()> {
        let publish = self.core.publish.publish();
        for update in self
            .core
            .storage
            .expire_stale_working_at(now, Some(&publish))?
        {
            self.core.broadcast(Some(update));
        }
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use chrono::Utc;
    use sessiontap_core::{
        config::Config,
        domain::{
            Activity, ActivityConfirmation, Capabilities, ChildAgentRef, EventEvidence, EventKind,
            Lifecycle, MultiplexerBackend, MultiplexerMetadata, ProcessMetadata, PublicField,
            PublicReasonKind, PublicStatus, StatusReasonSource, ToolActivityPhase,
            ToolActivityUpdate, derive_status,
        },
    };
    use sessiontap_infra::multiplexer::UnsupportedBackend;
    use std::collections::BTreeSet;

    pub(crate) fn app(storage: Storage) -> App {
        App::new(
            Arc::new(storage),
            PublishConfig::default(),
            &DaemonConfig::default(),
            Arc::new(MultiplexerRegistry::empty()),
            Collection {
                home: PathBuf::from("/nonexistent"),
                registry: Arc::new(AdapterRegistry::new(&Config::default())),
            },
        )
    }

    pub(crate) fn snapshot() -> InvocationSnapshot {
        let now = Utc::now();
        InvocationSnapshot {
            schema_version: sessiontap_core::SCHEMA_VERSION,
            revision: 0,
            invocation_id: InvocationId::new(),
            provider: "claude".into(),
            executable: "claude".into(),
            args: vec![],
            cwd: "/tmp".into(),
            process: ProcessMetadata::default(),
            created_at: now,
            updated_at: now,
            lifecycle: Lifecycle::Starting,
            activity: Activity::Idle,
            state_started_at: now,
            last_state_asserted_at: None,
            activity_confirmation: ActivityConfirmation::Live,
            last_evidence: None,
            source_ordering: vec![],
            current_tool_activity: None,
            status: derive_status(Lifecycle::Starting, Activity::Idle),
            provider_session: None,
            provider_metadata: None,
            usage: None,
            repository: None,
            multiplexer: None,
            capabilities: Capabilities::default(),
            turn_generation: 0,
            completed_generation: None,
            children: Vec::new(),
        }
    }

    pub(crate) fn event(
        initial: &InvocationSnapshot,
        id: &str,
        kind: EventKind,
    ) -> NormalizedEvent {
        NormalizedEvent {
            schema_version: 1,
            event_id: id.into(),
            invocation_id: initial.invocation_id.clone(),
            provider_event_id: None,
            provider: initial.provider.clone(),
            observed_at: Utc::now(),
            received_at: Utc::now(),
            evidence: EventEvidence::managed_hook(1),
            kind,
            provider_session_id: None,
            provider_session_name: None,
            provider_session_start_reason: None,
            provider_metadata: None,
            usage: None,
            turn_id: None,
            tool_activity: None,
            child_agent: None,
        }
    }

    #[tokio::test]
    async fn register_broadcasts_complete_view() {
        let app = app(Storage::memory().unwrap());
        let (_, views, mut receiver) = app.subscribe().unwrap();
        assert!(views.is_empty());
        let initial = snapshot();
        app.register(initial.clone(), "credential").unwrap();
        let update = receiver.recv().await.unwrap();
        assert_eq!(update.view.invocation_id, initial.invocation_id);
        assert_eq!(update.changed.len(), 12);
        assert_eq!(app.status().unwrap().1.len(), 1);
    }

    #[tokio::test]
    async fn bind_then_exit_broadcasts_in_revision_order() {
        let app = app(Storage::memory().unwrap());
        let initial = snapshot();
        app.register(initial.clone(), "credential").unwrap();
        let (revision, _, mut receiver) = app.subscribe().unwrap();
        app.bind_child(
            &initial.invocation_id,
            "credential",
            42,
            Some("start".into()),
        )
        .unwrap();
        // Binding changes internal process state only, so nothing is
        // broadcast; the stored snapshot still records it.
        assert!(receiver.try_recv().is_err());
        assert_eq!(
            app.storage()
                .invocation(&initial.invocation_id)
                .unwrap()
                .process
                .child_pid,
            Some(42)
        );
        app.lifecycle_exit(&initial.invocation_id, "credential", Some(0), None)
            .unwrap();
        let exited = receiver.recv().await.unwrap();
        assert!(revision < exited.revision);
        assert_eq!(exited.view.status, PublicStatus::Stopped);
        assert!(
            app.bind_child(&initial.invocation_id, "wrong", 1, None)
                .is_err()
        );
    }

    #[tokio::test]
    async fn ingest_rejects_mismatched_context_and_applies_valid_hooks() {
        let app = app(Storage::memory().unwrap());
        let initial = snapshot();
        app.register(initial.clone(), "credential").unwrap();
        app.bind_child(&initial.invocation_id, "credential", 42, None)
            .unwrap();
        let (_, _, mut receiver) = app.subscribe().unwrap();
        assert!(
            app.ingest_hook(
                initial.provider.clone(),
                initial.invocation_id.clone(),
                "wrong".into(),
                event(&initial, "bad", EventKind::NewTurn),
                None,
                None,
            )
            .is_err()
        );
        assert!(
            app.ingest_hook(
                "codex".into(),
                initial.invocation_id.clone(),
                "credential".into(),
                event(&initial, "bad-provider", EventKind::NewTurn),
                None,
                None,
            )
            .is_err()
        );
        app.ingest_hook(
            initial.provider.clone(),
            initial.invocation_id.clone(),
            "credential".into(),
            event(&initial, "turn", EventKind::NewTurn),
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            receiver.recv().await.unwrap().view.status,
            PublicStatus::Running
        );
    }

    #[tokio::test]
    async fn child_change_while_root_is_stopped_publishes_children_only() {
        let app = app(Storage::memory().unwrap());
        let initial = snapshot();
        app.register(initial.clone(), "credential").unwrap();
        app.bind_child(&initial.invocation_id, "credential", 42, None)
            .unwrap();
        let ingest = |event: NormalizedEvent, reason: Option<StatusReasonContext>| {
            app.ingest_hook(
                initial.provider.clone(),
                initial.invocation_id.clone(),
                "credential".into(),
                event,
                reason,
                None,
            )
            .unwrap();
        };
        let child = |id: &str, kind: EventKind| {
            let mut value = event(&initial, id, kind);
            value.child_agent = Some(ChildAgentRef {
                agent_id: "agent-1".into(),
                agent_type: "Explore".into(),
            });
            value
        };
        ingest(event(&initial, "turn", EventKind::NewTurn), None);
        ingest(child("child-start", EventKind::NewTurn), None);
        ingest(
            event(&initial, "stop", EventKind::Completed),
            Some(StatusReasonContext {
                summary: "Done".into(),
                source: StatusReasonSource::AssistantMessage,
            }),
        );
        let (_, _, mut receiver) = app.subscribe().unwrap();

        let mut approval = child("child-approval", EventKind::WaitingApproval);
        approval.tool_activity = Some(ToolActivityUpdate {
            phase: ToolActivityPhase::Attention,
            label: "shell".into(),
            correlation_id: Some("toolu_1".into()),
            detail: None,
        });
        ingest(approval.clone(), None);
        let update = receiver.recv().await.unwrap();
        assert_eq!(
            update.changed,
            BTreeSet::from([PublicField::UpdatedAt, PublicField::Children])
        );
        assert_eq!(update.view.status, PublicStatus::Stopped);
        assert_eq!(update.view.reason.as_ref().unwrap().summary, "Done");
        let children = update.view.children.as_ref().unwrap();
        assert_eq!(children[0].status, PublicStatus::Blocked);
        assert_eq!(
            children[0].reason.as_ref().unwrap().kind,
            Some(PublicReasonKind::Approval)
        );

        ingest(approval, None);
        assert!(receiver.try_recv().is_err());

        ingest(event(&initial, "next-turn", EventKind::NewTurn), None);
        let update = receiver.recv().await.unwrap();
        assert!(update.changed.contains(&PublicField::Children));
        assert!(update.view.children.is_none());
        assert_eq!(update.view.status, PublicStatus::Running);
    }

    #[tokio::test]
    async fn capture_requires_multiplexer_metadata() {
        let app = app(Storage::memory().unwrap());
        let initial = snapshot();
        app.register(initial.clone(), "credential").unwrap();
        let error = app.capture(&initial.invocation_id).unwrap_err();
        assert!(error.to_string().contains("not in a multiplexer"));
        assert!(app.send_input(&InvocationId::new(), "x").is_err());
    }

    #[tokio::test]
    async fn unregistered_backend_is_unsupported_without_running_commands() {
        let app = app(Storage::memory().unwrap());
        let mut initial = snapshot();
        initial.process.child_pid = Some(std::process::id());
        initial.multiplexer = Some(MultiplexerMetadata {
            backend: MultiplexerBackend::Tmux,
            socket: "/nonexistent/tmux.sock".into(),
            pane_id: "%0".into(),
            ..Default::default()
        });
        app.register(initial.clone(), "credential").unwrap();
        for error in [
            app.capture(&initial.invocation_id).unwrap_err(),
            app.send_input(&initial.invocation_id, "x").unwrap_err(),
        ] {
            assert_eq!(
                error.downcast_ref::<UnsupportedBackend>(),
                Some(&UnsupportedBackend(MultiplexerBackend::Tmux))
            );
        }
    }

    #[tokio::test]
    async fn enrichment_requires_matching_credential_and_session() {
        let app = app(Storage::memory().unwrap());
        let initial = snapshot();
        app.register(initial.clone(), "credential").unwrap();
        let revision = app.storage().revision().unwrap();
        let mut enrichment = event(&initial, "collect", EventKind::Enrichment);
        enrichment.provider_session_id = Some("other".into());
        app.core
            .apply_enrichment(&initial.invocation_id, "credential", enrichment.clone())
            .unwrap();
        app.core
            .apply_enrichment(&initial.invocation_id, "wrong", enrichment)
            .unwrap();
        assert_eq!(app.storage().revision().unwrap(), revision);
    }
}
