//! Background workers: sink delivery and the stale-working sweep.

use crate::{
    app::App,
    sinks::{DeliveryOutcome, Sink},
};
use anyhow::Result;
use sessiontap_core::config::DaemonConfig;
use sessiontap_storage::Storage;
use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
    time::Duration,
};
use tokio::time::Instant;

/// Snapshot retry backoff doubles from one second up to a one-minute cap.
/// This curve is fixed; only the tuning knobs in `DaemonConfig` vary.
const SNAPSHOT_BACKOFF_START: Duration = Duration::from_secs(1);
const SNAPSHOT_BACKOFF_MAX_SECS: u64 = 60;
const SNAPSHOT_BACKOFF_MAX: Duration = Duration::from_secs(SNAPSHOT_BACKOFF_MAX_SECS);

/// Delivers baseline snapshots and outbox records to built sinks.
pub struct SinkWorker {
    storage: Arc<Storage>,
    sinks: BTreeMap<String, Box<dyn Sink>>,
    source_id: String,
    source_name: Option<String>,
    batch: usize,
    max_rejected_attempts: u32,
    snapshot_backoff: HashMap<String, (Instant, Duration)>,
}

impl SinkWorker {
    #[must_use]
    pub fn new(app: &App, sinks: BTreeMap<String, Box<dyn Sink>>, daemon: &DaemonConfig) -> Self {
        let publish = app.publish_config();
        Self {
            storage: app.storage().clone(),
            sinks,
            source_id: publish.source_id.clone(),
            source_name: publish.source_name.clone(),
            batch: daemon.outbox_batch,
            max_rejected_attempts: daemon.max_rejected_attempts,
            snapshot_backoff: HashMap::new(),
        }
    }

    pub async fn run(mut self, poll: Duration) {
        loop {
            if let Err(error) = self.deliver_snapshots().await {
                eprintln!("sessiontapd: snapshot delivery failed: {error}");
            }
            if let Err(error) = self.process_outbox_once().await {
                eprintln!("sessiontapd: outbox delivery failed: {error}");
            }
            tokio::time::sleep(poll).await;
        }
    }

    /// Delivers the baseline source snapshot for every sink that needs one
    /// and has not yet established it, before any incremental updates.
    pub async fn deliver_snapshots(&mut self) -> Result<()> {
        if self.source_id.is_empty() {
            return Ok(());
        }
        for (name, sink) in self.sinks.iter().filter(|(_, sink)| sink.needs_baseline()) {
            if !self.storage.hub_snapshot_due(name)? {
                self.snapshot_backoff.remove(name);
                continue;
            }
            if let Some((next, _)) = self.snapshot_backoff.get(name)
                && Instant::now() < *next
            {
                continue;
            }
            let (revision, payload) = self
                .storage
                .hub_source_snapshot(&self.source_id, self.source_name.as_deref())?;
            if sink.deliver_snapshot(&payload).await == DeliveryOutcome::Ack {
                self.storage.hub_snapshot_delivered(name, revision)?;
                self.snapshot_backoff.remove(name);
            } else {
                let delay = self
                    .snapshot_backoff
                    .get(name)
                    .map_or(SNAPSHOT_BACKOFF_START, |(_, delay)| {
                        (*delay * 2).min(SNAPSHOT_BACKOFF_MAX)
                    });
                self.snapshot_backoff
                    .insert(name.clone(), (Instant::now() + delay, delay));
                eprintln!(
                    "sessiontapd: sink '{name}' snapshot delivery pending (retry in {delay:?})"
                );
            }
        }
        Ok(())
    }

    /// Delivers one batch of due outbox records and returns how many were
    /// due.
    pub async fn process_outbox_once(&self) -> Result<usize> {
        let records = self.storage.due_outbox(self.batch)?;
        let count = records.len();
        for record in records {
            let Some(sink) = self.sinks.get(&record.sink_name) else {
                continue;
            };
            if sink.needs_baseline() && self.storage.hub_snapshot_due(&record.sink_name)? {
                // Hold incremental updates until the baseline snapshot is
                // delivered so the receiver never sees an update gap.
                continue;
            }
            let (sink_name, event_id) = (&record.sink_name, &record.event_id);
            match sink.deliver(&record.payload).await {
                DeliveryOutcome::Ack => self.storage.acknowledge(sink_name, event_id)?,
                DeliveryOutcome::Retry => {
                    self.storage.retry(sink_name, event_id, record.attempts)?;
                }
                DeliveryOutcome::SnapshotRequired => {
                    self.storage.hub_reset_snapshot(sink_name)?;
                    self.storage.retry(sink_name, event_id, record.attempts)?;
                }
                DeliveryOutcome::Reject => {
                    // Bounded drop policy: a permanently rejected record must
                    // not occupy the bounded outbox forever.
                    if record.attempts + 1 >= self.max_rejected_attempts {
                        eprintln!(
                            "sessiontapd: dropping undeliverable event '{event_id}' for sink '{sink_name}'"
                        );
                        self.storage.acknowledge(sink_name, event_id)?;
                    } else {
                        self.storage.retry(sink_name, event_id, record.attempts)?;
                    }
                }
            }
        }
        Ok(count)
    }
}

/// Periodically expires stale working state and broadcasts the results.
pub async fn stale_working_worker(app: App, interval: Duration) {
    let mut interval = tokio::time::interval(interval);
    interval.tick().await;
    loop {
        interval.tick().await;
        if let Err(error) = app.expire_stale_working(chrono::Utc::now()) {
            eprintln!("sessiontapd: stale-working sweep failed: {error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{
        App, Collection, PublishConfig,
        tests::{event, snapshot},
    };
    use async_trait::async_trait;
    use sessiontap_adapters::AdapterRegistry;
    use sessiontap_core::{
        config::{Config, SinkConfig},
        domain::EventKind,
    };
    use std::sync::Mutex;

    /// Replays scripted outcomes and records delivered payload kinds.
    struct ScriptedSink {
        name: String,
        baseline: bool,
        outcomes: Mutex<Vec<DeliveryOutcome>>,
        delivered: Arc<Mutex<Vec<&'static str>>>,
    }

    #[async_trait]
    impl Sink for ScriptedSink {
        fn name(&self) -> &str {
            &self.name
        }
        fn needs_baseline(&self) -> bool {
            self.baseline
        }
        async fn deliver(&self, _: &[u8]) -> DeliveryOutcome {
            self.delivered.lock().unwrap().push("update");
            self.outcomes.lock().unwrap().remove(0)
        }
        async fn deliver_snapshot(&self, _: &[u8]) -> DeliveryOutcome {
            self.delivered.lock().unwrap().push("snapshot");
            DeliveryOutcome::Ack
        }
    }

    fn hub_config() -> BTreeMap<String, SinkConfig> {
        toml::from_str::<Config>(
            "version=1\nsource_id='h'\n[sinks.hub]\ntype='hub'\nenabled=true\nurl='http://127.0.0.1:9/ingest'\n",
        )
        .unwrap()
        .sinks
    }

    fn setup(
        database: &std::path::Path,
        outcomes: Vec<DeliveryOutcome>,
        daemon: DaemonConfig,
    ) -> (App, SinkWorker, Arc<Mutex<Vec<&'static str>>>) {
        let app = App::new(
            Arc::new(Storage::open(database).unwrap()),
            PublishConfig {
                sinks: hub_config(),
                source_id: "h".into(),
                source_name: None,
            },
            &daemon,
            Arc::new(sessiontap_infra::multiplexer::MultiplexerRegistry::empty()),
            Collection {
                home: "/nonexistent".into(),
                registry: Arc::new(AdapterRegistry::new(&Config::default())),
            },
        );
        let delivered = Arc::new(Mutex::new(Vec::new()));
        let sink: Box<dyn Sink> = Box::new(ScriptedSink {
            name: "hub".into(),
            baseline: true,
            outcomes: Mutex::new(outcomes),
            delivered: delivered.clone(),
        });
        let worker = SinkWorker::new(&app, BTreeMap::from([("hub".into(), sink)]), &daemon);
        (app, worker, delivered)
    }

    /// Retry backoff moves records seconds into the future; pull them back.
    fn make_due(database: &std::path::Path) {
        rusqlite::Connection::open(database)
            .unwrap()
            .execute(
                "UPDATE sink_outbox SET next_attempt_at='1970-01-01T00:00:00+00:00'",
                [],
            )
            .unwrap();
    }

    fn outbox_len(database: &std::path::Path) -> u64 {
        rusqlite::Connection::open(database)
            .unwrap()
            .query_row("SELECT COUNT(*) FROM sink_outbox", [], |row| row.get(0))
            .unwrap()
    }

    #[tokio::test]
    async fn ack_retry_snapshot_required_and_reject_paths() {
        let daemon = DaemonConfig {
            max_rejected_attempts: 2,
            ..DaemonConfig::default()
        };
        let temp = tempfile::tempdir().unwrap();
        let database = temp.path().join("state.db");
        let (app, mut worker, delivered) = setup(
            &database,
            vec![
                DeliveryOutcome::Retry,
                DeliveryOutcome::SnapshotRequired,
                DeliveryOutcome::Reject,
                DeliveryOutcome::Reject,
                DeliveryOutcome::Ack,
            ],
            daemon,
        );
        let initial = snapshot();
        app.register(initial.clone(), "credential").unwrap();
        app.bind_child(&initial.invocation_id, "credential", 42, None)
            .unwrap();

        // Updates are held until the baseline snapshot lands, which subsumes
        // the registration update.
        assert_eq!(worker.process_outbox_once().await.unwrap(), 1);
        assert!(delivered.lock().unwrap().is_empty());
        worker.deliver_snapshots().await.unwrap();
        assert_eq!(*delivered.lock().unwrap(), ["snapshot"]);
        assert!(app.storage().due_outbox(10).unwrap().is_empty());

        app.ingest_hook(
            initial.provider.clone(),
            initial.invocation_id.clone(),
            "credential".into(),
            event(&initial, "turn", EventKind::NewTurn),
            None,
            None,
        )
        .unwrap();

        // Retry keeps the record with backoff.
        assert_eq!(worker.process_outbox_once().await.unwrap(), 1);
        assert!(app.storage().due_outbox(10).unwrap().is_empty());
        make_due(&database);

        // SnapshotRequired resets the baseline and holds the record.
        worker.process_outbox_once().await.unwrap();
        assert!(app.storage().hub_snapshot_due("hub").unwrap());
        make_due(&database);
        worker.process_outbox_once().await.unwrap();
        assert_eq!(*delivered.lock().unwrap(), ["snapshot", "update", "update"]);
        worker.deliver_snapshots().await.unwrap();
        assert!(!app.storage().hub_snapshot_due("hub").unwrap());

        // The snapshot subsumed the held update; queue another for Reject.
        app.ingest_hook(
            initial.provider.clone(),
            initial.invocation_id.clone(),
            "credential".into(),
            event(&initial, "stop", EventKind::Completed),
            None,
            None,
        )
        .unwrap();
        worker.process_outbox_once().await.unwrap();
        assert_eq!(outbox_len(&database), 1);
        make_due(&database);
        worker.process_outbox_once().await.unwrap();
        assert_eq!(
            outbox_len(&database),
            0,
            "second rejection reaches max_rejected_attempts and drops"
        );
    }
}
