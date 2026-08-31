use crate::Broker;
use sessiontap_adapters::usage;
use sessiontap_core::domain::{
    ArtifactCollectionContext, ArtifactLocator, CollectorCursor, CollectorDialect,
    CollectorGeneration, EventEvidence, EventKind, EvidenceChannel, InvocationId, NormalizedEvent,
    StatuslineObservation, Usage,
};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
};
use tokio::sync::Semaphore;

#[derive(Clone)]
pub(crate) struct UsageCoordinator {
    inner: Arc<Inner>,
}

struct Inner {
    broker: Broker,
    home: PathBuf,
    states: Mutex<HashMap<InvocationId, InvocationCollection>>,
    workers: Arc<Semaphore>,
}

#[derive(Default)]
struct InvocationCollection {
    generation: CollectorGeneration,
    running: bool,
    locator: Option<ArtifactLocator>,
    cursor: Option<CollectorCursor>,
    claude_context: Option<ClaudeContext>,
}

#[derive(Clone)]
struct ClaudeContext {
    session_id: String,
    transcript_path: PathBuf,
    context_tokens: Option<u64>,
    context_window_percent: Option<u8>,
}

impl UsageCoordinator {
    pub(crate) fn new(broker: Broker, home: PathBuf, worker_limit: usize) -> Self {
        Self {
            inner: Arc::new(Inner {
                broker,
                home,
                states: Mutex::new(HashMap::new()),
                workers: Arc::new(Semaphore::new(worker_limit.max(1))),
            }),
        }
    }

    pub(crate) fn schedule(
        &self,
        invocation_id: InvocationId,
        context: Option<ArtifactCollectionContext>,
    ) {
        let should_start = {
            let mut states = self.inner.states.lock().expect("usage state lock poisoned");
            let state = states.entry(invocation_id.clone()).or_default();
            state.generation.0 = state.generation.0.saturating_add(1);
            if let Some(context) = context {
                if state.locator.as_ref() != Some(&context.locator) {
                    state.cursor = None;
                    let compatible_claude_context = context.locator.dialect
                        == CollectorDialect::Claude
                        && state.claude_context.as_ref().is_some_and(|observation| {
                            observation.session_id == context.locator.provider_session_id
                                && observation.transcript_path == context.locator.transcript_path
                        });
                    if !compatible_claude_context {
                        state.claude_context = None;
                    }
                }
                state.locator = Some(context.locator);
            }
            if state.running {
                false
            } else {
                state.running = true;
                true
            }
        };
        if should_start {
            let coordinator = self.clone();
            tokio::spawn(async move {
                coordinator.run(invocation_id).await;
            });
        }
    }

    pub(crate) fn observe_statusline(
        &self,
        invocation_id: InvocationId,
        observation: StatuslineObservation,
    ) {
        let locator = ArtifactLocator {
            dialect: CollectorDialect::Claude,
            provider_session_id: observation.provider_session_id.clone(),
            transcript_path: observation.transcript_path.clone(),
        };
        {
            let mut states = self.inner.states.lock().expect("usage state lock poisoned");
            let state = states.entry(invocation_id.clone()).or_default();
            state.claude_context = Some(ClaudeContext {
                session_id: observation.provider_session_id,
                transcript_path: observation.transcript_path,
                context_tokens: observation.context_tokens,
                context_window_percent: observation.context_window_percent,
            });
        }
        self.schedule(invocation_id, Some(ArtifactCollectionContext { locator }));
    }

    async fn run(&self, invocation_id: InvocationId) {
        loop {
            let (generation, locator, cursor) = {
                let states = self.inner.states.lock().expect("usage state lock poisoned");
                let state = states
                    .get(&invocation_id)
                    .expect("scheduled usage state missing");
                (
                    state.generation,
                    state.locator.clone(),
                    state.cursor.clone(),
                )
            };
            let Some(locator) = locator else {
                self.finish_or_repeat(&invocation_id, generation);
                return;
            };
            let permit = match self.inner.workers.clone().acquire_owned().await {
                Ok(permit) => permit,
                Err(_) => return,
            };
            let home = self.inner.home.clone();
            let scan_locator = locator.clone();
            let result = tokio::task::spawn_blocking(move || {
                let _permit = permit;
                usage::collect(&home, &scan_locator, cursor.as_ref())
            })
            .await;
            let Ok(result) = result else {
                eprintln!("sessiontapd: usage worker stopped unexpectedly");
                if !self.finish_or_repeat(&invocation_id, generation) {
                    return;
                }
                continue;
            };
            match result {
                Ok(result) => self.apply_if_current(&invocation_id, generation, &locator, result),
                Err(error) => eprintln!("sessiontapd: usage collection failed: {error}"),
            }
            if !self.finish_or_repeat(&invocation_id, generation) {
                return;
            }
        }
    }

    fn finish_or_repeat(
        &self,
        invocation_id: &InvocationId,
        completed: CollectorGeneration,
    ) -> bool {
        let mut states = self.inner.states.lock().expect("usage state lock poisoned");
        let Some(state) = states.get_mut(invocation_id) else {
            return false;
        };
        if state.generation > completed {
            true
        } else {
            state.running = false;
            false
        }
    }

    fn apply_if_current(
        &self,
        invocation_id: &InvocationId,
        generation: CollectorGeneration,
        locator: &ArtifactLocator,
        result: usage::CollectionResult,
    ) {
        let claude_context = {
            let mut states = self.inner.states.lock().expect("usage state lock poisoned");
            let Some(state) = states.get_mut(invocation_id) else {
                return;
            };
            if state.generation != generation || state.locator.as_ref() != Some(locator) {
                return;
            }
            state.cursor = Some(result.cursor);
            state.claude_context.clone()
        };
        if !usage::identity_matches(&self.inner.home, locator, &result.identity) {
            return;
        }
        let Ok(snapshot) = self.inner.broker.storage.invocation(invocation_id) else {
            return;
        };
        if snapshot
            .provider_session
            .as_ref()
            .map(|session| session.id.as_str())
            != Some(locator.provider_session_id.as_str())
        {
            return;
        }
        let mut usage = Usage {
            input_tokens: result.observation.input_tokens,
            output_tokens: result.observation.output_tokens,
            context_tokens: result.observation.context_tokens,
            context_window_percent: result.observation.context_window_percent,
        };
        if locator.dialect == CollectorDialect::Claude {
            usage.context_tokens = None;
            usage.context_window_percent = None;
            if let Some(context) = claude_context.filter(|context| {
                context.session_id == locator.provider_session_id
                    && context.transcript_path == locator.transcript_path
            }) {
                usage.context_tokens = context.context_tokens;
                usage.context_window_percent = context.context_window_percent;
            }
        }
        let now = chrono::Utc::now();
        let event = NormalizedEvent {
            schema_version: sessiontap_core::SCHEMA_VERSION,
            event_id: format!("usage:{}:{}", invocation_id, generation.0),
            invocation_id: invocation_id.clone(),
            provider_event_id: None,
            provider: snapshot.provider,
            observed_at: now,
            received_at: now,
            evidence: EventEvidence::local(EvidenceChannel::ProviderArtifact),
            kind: EventKind::Enrichment,
            provider_session_id: None,
            provider_session_name: None,
            provider_session_start_reason: None,
            provider_metadata: None,
            usage: Some(usage),
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
            Err(error) => eprintln!("sessiontapd: collected usage was not applied: {error}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sessiontap_core::domain::{
        Activity, ActivityConfirmation, Capabilities, InvocationSnapshot, Lifecycle,
        ProcessMetadata, ProviderSession, PublicStatus,
    };
    use sessiontap_storage::Storage;
    use std::{collections::BTreeMap, fs, path::Path, time::Duration};
    use tokio::sync::broadcast;

    fn snapshot(session: &str) -> InvocationSnapshot {
        let now = chrono::Utc::now();
        InvocationSnapshot {
            schema_version: sessiontap_core::SCHEMA_VERSION,
            revision: 0,
            invocation_id: InvocationId::new(),
            provider: "company-qwen".into(),
            executable: "qwen".into(),
            args: vec![],
            cwd: "/work".into(),
            process: ProcessMetadata::default(),
            created_at: now,
            updated_at: now,
            lifecycle: Lifecycle::Alive,
            activity: Activity::Idle,
            state_started_at: now,
            last_state_asserted_at: Some(now),
            activity_confirmation: ActivityConfirmation::Live,
            last_evidence: None,
            source_ordering: vec![],
            current_tool_activity: None,
            status: PublicStatus::Idle,
            provider_session: Some(ProviderSession {
                id: session.into(),
                generation: 1,
                ..Default::default()
            }),
            provider_metadata: None,
            usage: None,
            repository: None,
            multiplexer: None,
            capabilities: Capabilities::default(),
            turn_generation: 0,
            completed_generation: None,
        }
    }

    fn coordinator(temp: &Path, snapshot: &InvocationSnapshot) -> UsageCoordinator {
        let storage = Arc::new(Storage::memory().unwrap());
        storage.register(snapshot, "secret", None).unwrap();
        let (updates, _) = broadcast::channel(32);
        let broker = Broker {
            storage,
            updates,
            sinks: Arc::new(BTreeMap::new()),
            source_id: Arc::from(""),
            source_name: Arc::new(None),
        };
        UsageCoordinator::new(broker, temp.to_path_buf(), 1)
    }

    async fn wait_for_usage(coordinator: &UsageCoordinator, id: &InvocationId) -> Usage {
        for _ in 0..100 {
            if let Some(usage) = coordinator
                .inner
                .broker
                .storage
                .invocation(id)
                .unwrap()
                .usage
            {
                return usage;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("usage was not collected")
    }

    #[tokio::test]
    async fn hook_bursts_coalesce_and_collection_failures_preserve_usage() {
        let temp = tempfile::tempdir().unwrap();
        let session = "session-1";
        let root = temp.path().join(".qwen/projects/p/chats");
        fs::create_dir_all(&root).unwrap();
        let path = root.join(format!("{session}.jsonl"));
        fs::write(&path, format!(r#"{{"sessionId":"{session}","type":"assistant","usageMetadata":{{"promptTokenCount":8,"candidatesTokenCount":2}},"contextWindowSize":100}}"#) + "\n").unwrap();
        let snapshot = snapshot(session);
        let id = snapshot.invocation_id.clone();
        let coordinator = coordinator(temp.path(), &snapshot);
        let context = ArtifactCollectionContext {
            locator: ArtifactLocator {
                dialect: CollectorDialect::Qwen,
                provider_session_id: session.into(),
                transcript_path: path.clone(),
            },
        };
        for _ in 0..20 {
            coordinator.schedule(id.clone(), Some(context.clone()));
        }
        let usage = wait_for_usage(&coordinator, &id).await;
        assert_eq!(usage.input_tokens, Some(8));
        assert_eq!(usage.output_tokens, Some(2));

        use std::io::Write;
        writeln!(
            fs::OpenOptions::new().append(true).open(path).unwrap(),
            "not-json"
        )
        .unwrap();
        coordinator.schedule(id.clone(), None);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            coordinator
                .inner
                .broker
                .storage
                .invocation(&id)
                .unwrap()
                .usage,
            Some(usage)
        );
    }

    #[tokio::test]
    async fn stale_provider_session_result_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let session = "old";
        let root = temp.path().join(".qwen/projects/p/chats");
        fs::create_dir_all(&root).unwrap();
        let path = root.join("old.jsonl");
        fs::write(&path, String::from(r#"{"sessionId":"old","type":"assistant","usageMetadata":{"promptTokenCount":8,"candidatesTokenCount":2},"contextWindowSize":100}"#) + "\n").unwrap();
        let snapshot = snapshot(session);
        let id = snapshot.invocation_id.clone();
        let coordinator = coordinator(temp.path(), &snapshot);
        let now = chrono::Utc::now();
        let event = NormalizedEvent {
            schema_version: 1,
            event_id: "new-session".into(),
            invocation_id: id.clone(),
            provider_event_id: None,
            provider: "company-qwen".into(),
            observed_at: now,
            received_at: now,
            evidence: EventEvidence::managed_hook(1),
            kind: EventKind::ProviderSessionStarted,
            provider_session_id: Some("new".into()),
            provider_session_name: None,
            provider_session_start_reason: Some("clear".into()),
            provider_metadata: None,
            usage: None,
            turn_id: None,
            tool_activity: None,
        };
        coordinator
            .inner
            .broker
            .storage
            .apply_event(&event, None)
            .unwrap();
        coordinator.schedule(
            id.clone(),
            Some(ArtifactCollectionContext {
                locator: ArtifactLocator {
                    dialect: CollectorDialect::Qwen,
                    provider_session_id: session.into(),
                    transcript_path: path,
                },
            }),
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            coordinator
                .inner
                .broker
                .storage
                .invocation(&id)
                .unwrap()
                .usage
                .is_none()
        );
    }

    #[tokio::test]
    async fn early_refresh_is_a_noop_and_claude_sources_merge_then_clear_context() {
        let temp = tempfile::tempdir().unwrap();
        let session = "claude-session";
        let root = temp.path().join(".claude/projects/p");
        fs::create_dir_all(&root).unwrap();
        let path = root.join(format!("{session}.jsonl"));
        fs::write(
            &path,
            format!(
                r#"{{"sessionId":"{session}","type":"assistant","message":{{"id":"response-1","usage":{{"input_tokens":10,"cache_read_input_tokens":2,"cache_creation_input_tokens":3,"output_tokens":4}}}}}}"#
            ) + "\n",
        )
        .unwrap();
        let mut snapshot = snapshot(session);
        snapshot.provider = "company-claude".into();
        let id = snapshot.invocation_id.clone();
        let coordinator = coordinator(temp.path(), &snapshot);

        coordinator.schedule(id.clone(), None);
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(
            coordinator
                .inner
                .broker
                .storage
                .invocation(&id)
                .unwrap()
                .usage
                .is_none()
        );

        coordinator.observe_statusline(
            id.clone(),
            StatuslineObservation {
                provider_session_id: session.into(),
                transcript_path: path.clone(),
                context_tokens: Some(15),
                context_window_percent: Some(25),
            },
        );
        let merged = wait_for_usage(&coordinator, &id).await;
        assert_eq!(merged.input_tokens, Some(15));
        assert_eq!(merged.output_tokens, Some(4));
        assert_eq!(merged.context_tokens, Some(15));
        assert_eq!(merged.context_window_percent, Some(25));

        coordinator.observe_statusline(
            id.clone(),
            StatuslineObservation {
                provider_session_id: session.into(),
                transcript_path: path,
                context_tokens: None,
                context_window_percent: None,
            },
        );
        for _ in 0..100 {
            let usage = coordinator
                .inner
                .broker
                .storage
                .invocation(&id)
                .unwrap()
                .usage
                .unwrap();
            if usage.context_tokens.is_none() {
                assert_eq!(usage.input_tokens, Some(15));
                assert_eq!(usage.output_tokens, Some(4));
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("Claude context clear was not applied");
    }
}
