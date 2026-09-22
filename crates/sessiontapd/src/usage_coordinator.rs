use crate::Broker;
use sessiontap_adapters::{
    AdapterRegistry, BoundedDiagnostic, CollectSessionDataRequest, CollectionCancellation,
    CollectionOutcome, OpaqueCursor, ProviderSessionKey, SessionEnrichment,
};
use sessiontap_core::domain::{
    ArtifactCollectionContext, EventEvidence, EventKind, EvidenceChannel, InvocationId,
    NormalizedEvent,
};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::{Notify, Semaphore};

const DEBOUNCE_QUIET: Duration = Duration::from_millis(150);
const MAX_DEFERRAL: Duration = Duration::from_secs(2);

#[derive(Clone)]
pub(crate) struct UsageCoordinator {
    inner: Arc<Inner>,
}

struct Inner {
    broker: Broker,
    home: PathBuf,
    registry: Arc<AdapterRegistry>,
    states: Mutex<CoordinatorState>,
    workers: Arc<Semaphore>,
}

#[derive(Default)]
struct CoordinatorState {
    sessions: HashMap<ProviderSessionKey, SessionState>,
    invocation_keys: HashMap<InvocationId, ProviderSessionKey>,
}

struct SessionState {
    generation: u64,
    locator: PathBuf,
    cursor: Option<OpaqueCursor>,
    bindings: HashMap<InvocationId, Binding>,
    cancellation: Option<CollectionCancellation>,
    notify: Arc<Notify>,
    worker_running: bool,
    first_pending_at: Instant,
    last_event_at: Instant,
}

enum Settled {
    /// The generation was cancelled; wait for the newer one.
    Retry,
    /// The generation finished, with a diagnostic to log on failure.
    Done(Option<BoundedDiagnostic>),
}

#[derive(Clone)]
struct Binding {
    credential: String,
}

impl UsageCoordinator {
    pub(crate) fn new(
        broker: Broker,
        home: PathBuf,
        registry: Arc<AdapterRegistry>,
        worker_limit: usize,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                broker,
                home,
                registry,
                states: Mutex::new(CoordinatorState::default()),
                workers: Arc::new(Semaphore::new(worker_limit.max(1))),
            }),
        }
    }

    pub(crate) fn schedule(
        &self,
        configured_provider: String,
        invocation_id: InvocationId,
        credential: String,
        context: Option<ArtifactCollectionContext>,
    ) {
        let Some(context) = context else { return };
        if context.provider_session_id.trim().is_empty()
            || self
                .inner
                .registry
                .resolve(&configured_provider)
                .is_none_or(|(adapter, _)| adapter.provider_id() != context.adapter_identity)
        {
            return;
        }
        let key = ProviderSessionKey {
            configured_provider,
            adapter_identity: context.adapter_identity,
            provider_session_id: context.provider_session_id,
        };
        let (start_worker, notify) = {
            let mut coordinator = self
                .inner
                .states
                .lock()
                .expect("collection state lock poisoned");
            if let Some(old_key) = coordinator
                .invocation_keys
                .insert(invocation_id.clone(), key.clone())
                && old_key != key
                && let Some(old) = coordinator.sessions.get_mut(&old_key)
            {
                old.bindings.remove(&invocation_id);
                old.generation = old.generation.saturating_add(1);
                if let Some(cancellation) = &old.cancellation {
                    cancellation.cancel();
                }
                old.notify.notify_one();
            }
            let now = Instant::now();
            let state = coordinator
                .sessions
                .entry(key.clone())
                .or_insert_with(|| SessionState {
                    generation: 0,
                    locator: context.locator.clone(),
                    cursor: None,
                    bindings: HashMap::new(),
                    cancellation: None,
                    notify: Arc::new(Notify::new()),
                    worker_running: false,
                    first_pending_at: now,
                    last_event_at: now,
                });
            state.generation = state.generation.saturating_add(1);
            state.locator = context.locator;
            state.last_event_at = now;
            state.bindings.insert(invocation_id, Binding { credential });
            if let Some(cancellation) = &state.cancellation {
                if !cancellation.is_cancelled() {
                    state.first_pending_at = now;
                }
                cancellation.cancel();
            }
            state.notify.notify_one();
            let start = !state.worker_running;
            if start {
                state.worker_running = true;
                state.first_pending_at = now;
            }
            (start, state.notify.clone())
        };
        if start_worker {
            let coordinator = self.clone();
            tokio::spawn(async move {
                coordinator.run(key, notify).await;
            });
        }
    }

    async fn run(&self, key: ProviderSessionKey, notify: Arc<Notify>) {
        loop {
            let deadline = {
                let states = self
                    .inner
                    .states
                    .lock()
                    .expect("collection state lock poisoned");
                let Some(state) = states.sessions.get(&key) else {
                    return;
                };
                (state.last_event_at + DEBOUNCE_QUIET).min(state.first_pending_at + MAX_DEFERRAL)
            };
            let sleep = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline));
            tokio::pin!(sleep);
            tokio::select! {
                () = notify.notified() => continue,
                () = &mut sleep => {}
            }

            let (generation, locator, cursor, cancellation, has_bindings) = {
                let mut states = self
                    .inner
                    .states
                    .lock()
                    .expect("collection state lock poisoned");
                let Some(state) = states.sessions.get_mut(&key) else {
                    return;
                };
                let cancellation = CollectionCancellation::default();
                state.cancellation = Some(cancellation.clone());
                (
                    state.generation,
                    state.locator.clone(),
                    state.cursor.clone(),
                    cancellation,
                    !state.bindings.is_empty(),
                )
            };
            if !has_bindings {
                self.finish(&key, generation);
                return;
            }
            let adapter = self.inner.registry.get(key.adapter_identity);
            let permit = match self.inner.workers.clone().acquire_owned().await {
                Ok(permit) => permit,
                Err(_) => return,
            };
            if cancellation.is_cancelled() {
                drop(permit);
                continue;
            }
            let outcome = adapter
                .collect_session_data(CollectSessionDataRequest {
                    home: self.inner.home.clone(),
                    key: key.clone(),
                    locator,
                    prior_cursor: cursor,
                    cancellation: cancellation.clone(),
                })
                .await;
            drop(permit);

            let current = {
                let mut states = self
                    .inner
                    .states
                    .lock()
                    .expect("collection state lock poisoned");
                let Some(state) = states.sessions.get_mut(&key) else {
                    return;
                };
                state.cancellation = None;
                state.generation == generation && !cancellation.is_cancelled()
            };
            if !current {
                continue;
            }
            match self.settle(&key, generation, outcome) {
                Settled::Retry => continue,
                Settled::Done(Some(diagnostic)) => {
                    eprintln!(
                        "sessiontapd: provider collection failed: {}",
                        diagnostic.message()
                    );
                }
                Settled::Done(None) => {}
            }
            if self.finish(&key, generation) {
                continue;
            }
            return;
        }
    }

    /// Applies the outcome of a current generation. Only a completed scan
    /// publishes enrichment; unchanged and unsupported outcomes leave verified
    /// state untouched, and only failures produce a diagnostic.
    fn settle(
        &self,
        key: &ProviderSessionKey,
        generation: u64,
        outcome: CollectionOutcome,
    ) -> Settled {
        let cursor = match outcome {
            CollectionOutcome::Complete { enrichment, cursor } => {
                self.apply_if_current(key, generation, enrichment);
                cursor
            }
            CollectionOutcome::Unchanged { cursor } => cursor,
            CollectionOutcome::Cancelled => return Settled::Retry,
            CollectionOutcome::Unsupported => return Settled::Done(None),
            CollectionOutcome::Failed(diagnostic) => return Settled::Done(Some(diagnostic)),
        };
        if let Some(state) = self
            .inner
            .states
            .lock()
            .expect("collection state lock poisoned")
            .sessions
            .get_mut(key)
            && state.generation == generation
        {
            state.cursor = Some(cursor);
        }
        Settled::Done(None)
    }

    fn finish(&self, key: &ProviderSessionKey, generation: u64) -> bool {
        let mut states = self
            .inner
            .states
            .lock()
            .expect("collection state lock poisoned");
        let Some(state) = states.sessions.get_mut(key) else {
            return false;
        };
        if state.generation != generation {
            state.first_pending_at = Instant::now();
            true
        } else {
            state.worker_running = false;
            false
        }
    }

    fn apply_if_current(
        &self,
        key: &ProviderSessionKey,
        generation: u64,
        enrichment: SessionEnrichment,
    ) {
        let bindings = {
            let states = self
                .inner
                .states
                .lock()
                .expect("collection state lock poisoned");
            let Some(state) = states.sessions.get(key) else {
                return;
            };
            if state.generation != generation {
                return;
            }
            state.bindings.clone()
        };
        for (invocation_id, binding) in bindings {
            if !self
                .inner
                .broker
                .storage
                .credential_matches(
                    &invocation_id,
                    &key.configured_provider,
                    &binding.credential,
                )
                .unwrap_or(false)
            {
                continue;
            }
            let Ok(snapshot) = self.inner.broker.storage.invocation(&invocation_id) else {
                continue;
            };
            if snapshot.provider != key.configured_provider
                || snapshot
                    .provider_session
                    .as_ref()
                    .map(|session| session.id.as_str())
                    != Some(key.provider_session_id.as_str())
            {
                continue;
            }
            let now = chrono::Utc::now();
            let event = NormalizedEvent {
                schema_version: sessiontap_core::SCHEMA_VERSION,
                event_id: format!("collection:{}:{}", invocation_id, generation),
                invocation_id: invocation_id.clone(),
                provider_event_id: None,
                provider: key.configured_provider.clone(),
                observed_at: now,
                received_at: now,
                evidence: EventEvidence::local(EvidenceChannel::ProviderArtifact),
                kind: EventKind::Enrichment,
                provider_session_id: Some(key.provider_session_id.clone()),
                provider_session_name: enrichment.session_name.clone(),
                provider_session_start_reason: None,
                provider_metadata: None,
                usage: enrichment.usage.clone(),
                turn_id: None,
                tool_activity: None,
            };
            let publish = self.inner.broker.publish();
            match self
                .inner
                .broker
                .storage
                .apply_event_with_context(&event, None, Some(&publish))
            {
                Ok(Some(update)) => {
                    let _ = self.inner.broker.updates.send(update);
                }
                Ok(None) => {}
                Err(error) => {
                    eprintln!("sessiontapd: collected enrichment was not applied: {error}")
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sessiontap_adapters::artifact::ArtifactCursor;
    use sessiontap_core::{config::Config, domain::Usage};
    use sessiontap_storage::Storage;
    use std::collections::BTreeMap;
    use tokio::sync::broadcast;

    fn coordinator(temp: &tempfile::TempDir) -> UsageCoordinator {
        let storage = Arc::new(Storage::open(&temp.path().join("state.db")).unwrap());
        let (updates, _) = broadcast::channel(16);
        let broker = Broker {
            storage,
            updates,
            sinks: Arc::new(BTreeMap::new()),
            source_id: Arc::from("test"),
            source_name: Arc::new(None),
        };
        UsageCoordinator::new(
            broker,
            temp.path().to_path_buf(),
            Arc::new(AdapterRegistry::new(&Config::default())),
            2,
        )
    }

    fn context(adapter: &str, session: &str) -> ArtifactCollectionContext {
        ArtifactCollectionContext {
            adapter_identity: adapter.parse().unwrap(),
            provider_session_id: session.into(),
            locator: PathBuf::from(format!("{session}.jsonl")),
        }
    }

    #[tokio::test]
    async fn bindings_share_provider_qualified_state_and_providers_stay_isolated() {
        let temp = tempfile::tempdir().unwrap();
        let coordinator = coordinator(&temp);
        let first = InvocationId::new();
        let second = InvocationId::new();
        coordinator.schedule(
            "qwen".into(),
            first,
            "a".into(),
            Some(context("qwen", "same")),
        );
        coordinator.schedule(
            "qwen".into(),
            second,
            "b".into(),
            Some(context("qwen", "same")),
        );
        coordinator.schedule(
            "claude".into(),
            InvocationId::new(),
            "c".into(),
            Some(context("claude", "same")),
        );
        let states = coordinator.inner.states.lock().unwrap();
        assert_eq!(states.sessions.len(), 2);
        let qwen = states
            .sessions
            .iter()
            .find(|(key, _)| key.configured_provider == "qwen")
            .unwrap()
            .1;
        assert_eq!(qwen.bindings.len(), 2);
        assert_eq!(qwen.generation, 2);
    }

    #[tokio::test]
    async fn newer_event_cancels_running_generation_and_missing_session_does_not_schedule() {
        let temp = tempfile::tempdir().unwrap();
        let coordinator = coordinator(&temp);
        let invocation = InvocationId::new();
        coordinator.schedule("qwen".into(), invocation.clone(), "a".into(), None);
        assert!(coordinator.inner.states.lock().unwrap().sessions.is_empty());

        coordinator.schedule(
            "qwen".into(),
            invocation.clone(),
            "a".into(),
            Some(context("qwen", "s1")),
        );
        let cancellation = CollectionCancellation::default();
        {
            let mut states = coordinator.inner.states.lock().unwrap();
            states.sessions.values_mut().next().unwrap().cancellation = Some(cancellation.clone());
        }
        coordinator.schedule(
            "qwen".into(),
            invocation,
            "a".into(),
            Some(context("qwen", "s1")),
        );
        assert!(cancellation.is_cancelled());
        assert_eq!(
            coordinator
                .inner
                .states
                .lock()
                .unwrap()
                .sessions
                .values()
                .next()
                .unwrap()
                .generation,
            2
        );
    }

    fn register_bound_invocation(
        coordinator: &UsageCoordinator,
        provider: &str,
        session: &str,
    ) -> (ProviderSessionKey, InvocationId, u64) {
        use sessiontap_core::domain::{
            Activity, ActivityConfirmation, Capabilities, InvocationSnapshot, Lifecycle,
            ProcessMetadata, ProviderSession, derive_status,
        };
        let now = chrono::Utc::now();
        let invocation = InvocationId::new();
        let snapshot = InvocationSnapshot {
            schema_version: sessiontap_core::SCHEMA_VERSION,
            revision: 0,
            invocation_id: invocation.clone(),
            provider: provider.into(),
            executable: provider.into(),
            args: vec![],
            cwd: "/tmp".into(),
            process: ProcessMetadata::default(),
            created_at: now,
            updated_at: now,
            lifecycle: Lifecycle::Alive,
            activity: Activity::Idle,
            state_started_at: now,
            last_state_asserted_at: None,
            activity_confirmation: ActivityConfirmation::Live,
            last_evidence: None,
            source_ordering: vec![],
            current_tool_activity: None,
            status: derive_status(Lifecycle::Alive, Activity::Idle),
            provider_session: Some(ProviderSession {
                id: session.into(),
                ..ProviderSession::default()
            }),
            provider_metadata: None,
            usage: None,
            repository: None,
            multiplexer: None,
            capabilities: Capabilities::default(),
            turn_generation: 0,
            completed_generation: None,
        };
        coordinator
            .inner
            .broker
            .storage
            .register(&snapshot, "credential", None)
            .unwrap();
        // Scheduling without awaiting keeps the spawned worker parked, so the
        // test drives `settle` directly for the generation it created.
        coordinator.schedule(
            provider.into(),
            invocation.clone(),
            "credential".into(),
            Some(context(provider, session)),
        );
        let states = coordinator.inner.states.lock().unwrap();
        let (key, state) = states.sessions.iter().next().unwrap();
        (key.clone(), invocation, state.generation)
    }

    fn stored_cursor(
        coordinator: &UsageCoordinator,
        key: &ProviderSessionKey,
    ) -> Option<ArtifactCursor> {
        coordinator.inner.states.lock().unwrap().sessions[key]
            .cursor
            .as_ref()
            .and_then(|cursor| cursor.downcast_ref::<ArtifactCursor>().copied())
    }

    #[tokio::test]
    async fn unchanged_collection_applies_no_enrichment_and_keeps_cursor() {
        let temp = tempfile::tempdir().unwrap();
        let coordinator = coordinator(&temp);
        let (key, invocation, generation) = register_bound_invocation(&coordinator, "claude", "s1");
        let storage = coordinator.inner.broker.storage.clone();
        let cursor = ArtifactCursor {
            device: 1,
            inode: 2,
            stable_len: 3,
        };
        let usage = Usage {
            input_tokens: Some(10),
            output_tokens: Some(2),
            context_tokens: Some(10),
            context_window_percent: None,
        };
        assert!(matches!(
            coordinator.settle(
                &key,
                generation,
                CollectionOutcome::Complete {
                    enrichment: SessionEnrichment {
                        session_name: None,
                        usage: Some(usage.clone()),
                    },
                    cursor: OpaqueCursor::new(cursor),
                },
            ),
            Settled::Done(None)
        ));
        assert_eq!(
            storage.invocation(&invocation).unwrap().usage,
            Some(usage.clone())
        );
        assert_eq!(stored_cursor(&coordinator, &key), Some(cursor));

        let revision = storage.revision().unwrap();
        let mut updates = coordinator.inner.broker.updates.subscribe();
        assert!(matches!(
            coordinator.settle(
                &key,
                generation,
                CollectionOutcome::Unchanged {
                    cursor: OpaqueCursor::new(cursor),
                },
            ),
            Settled::Done(None)
        ));
        assert_eq!(storage.revision().unwrap(), revision);
        assert!(updates.try_recv().is_err());
        assert_eq!(storage.invocation(&invocation).unwrap().usage, Some(usage));
        assert_eq!(stored_cursor(&coordinator, &key), Some(cursor));
    }

    #[tokio::test]
    async fn unsupported_collection_is_a_silent_no_op() {
        let temp = tempfile::tempdir().unwrap();
        let coordinator = coordinator(&temp);
        let (key, invocation, generation) = register_bound_invocation(&coordinator, "pi", "s1");
        let storage = coordinator.inner.broker.storage.clone();
        let revision = storage.revision().unwrap();
        assert!(matches!(
            coordinator.settle(&key, generation, CollectionOutcome::Unsupported),
            Settled::Done(None)
        ));
        assert_eq!(storage.revision().unwrap(), revision);
        assert!(storage.invocation(&invocation).unwrap().usage.is_none());
        assert!(stored_cursor(&coordinator, &key).is_none());
        assert!(matches!(
            coordinator.settle(
                &key,
                generation,
                CollectionOutcome::Failed(BoundedDiagnostic::new("boom"))
            ),
            Settled::Done(Some(_))
        ));
    }

    #[test]
    fn debounce_is_trailing_edge_with_bounded_deferral() {
        assert_eq!(DEBOUNCE_QUIET, Duration::from_millis(150));
        assert_eq!(MAX_DEFERRAL, Duration::from_secs(2));
        assert!(MAX_DEFERRAL > DEBOUNCE_QUIET);
    }
}
