use anyhow::{Context, Result, bail};
use chrono::{Duration, Utc};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use sessiontap_core::{
    SCHEMA_VERSION,
    config::SinkConfig,
    domain::{
        ATTENTION_MAX_BYTES, ATTENTION_MAX_CHARS, ActiveAttention, Activity, AttentionContext,
        EventKind, FailureContext, InvocationId, InvocationSnapshot, Lifecycle, LiveEventMetadata,
        NormalizedEvent, derive_status,
    },
    protocol::{
        HUB_SCHEMA_VERSION, HubEnvelope, HubEventMetadata, SinkEvent, SourceCapabilities,
        SourceIdentity,
    },
};
use std::{collections::BTreeMap, path::Path, sync::Mutex};

const MIGRATION: &str = r#"
CREATE TABLE IF NOT EXISTS schema_migrations (version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL);
INSERT OR IGNORE INTO schema_migrations(version,applied_at) VALUES (1,CURRENT_TIMESTAMP);
INSERT OR IGNORE INTO schema_migrations(version,applied_at) VALUES (2,CURRENT_TIMESTAMP);
CREATE TABLE IF NOT EXISTS broker_meta (key TEXT PRIMARY KEY, value INTEGER NOT NULL);
INSERT OR IGNORE INTO broker_meta(key,value) VALUES ('revision',0);
CREATE TABLE IF NOT EXISTS invocations (
 invocation_id TEXT PRIMARY KEY, provider TEXT NOT NULL, credential TEXT NOT NULL,
 snapshot_json TEXT NOT NULL, stopped_at TEXT, turn_generation INTEGER NOT NULL DEFAULT 0,
 completed_generation INTEGER
);
CREATE TABLE IF NOT EXISTS normalized_events (
 event_id TEXT PRIMARY KEY, invocation_id TEXT NOT NULL, revision INTEGER NOT NULL,
 received_at TEXT NOT NULL, event_json TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS event_dedup (event_id TEXT PRIMARY KEY, committed_at TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS sink_outbox (
 sink_name TEXT NOT NULL, event_id TEXT NOT NULL, revision INTEGER NOT NULL,
 payload BLOB NOT NULL, attempts INTEGER NOT NULL DEFAULT 0, next_attempt_at TEXT NOT NULL,
 PRIMARY KEY(sink_name,event_id)
);
CREATE TABLE IF NOT EXISTS local_active_attention (
 invocation_id TEXT PRIMARY KEY REFERENCES invocations(invocation_id) ON DELETE CASCADE,
 kind TEXT NOT NULL, attention_json TEXT NOT NULL CHECK(length(attention_json) <= 2048),
 updated_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS hub_sink_state (
 sink_name TEXT PRIMARY KEY,
 snapshot_revision INTEGER
);
"#;
const MAX_OUTBOX_RECORDS_PER_SINK: u64 = 1_024;

/// Delivery context shared by every transition that must become sink-visible.
pub struct Publish<'a> {
    pub sinks: &'a BTreeMap<String, SinkConfig>,
    pub source_id: &'a str,
    pub source_name: Option<&'a str>,
}

pub struct Storage {
    conn: Mutex<Connection>,
}

impl Storage {
    pub fn open(path: &Path) -> Result<Self> {
        if path
            .symlink_metadata()
            .is_ok_and(|metadata| metadata.file_type().is_symlink())
        {
            bail!("database path must not be a symlink");
        }
        let conn = Connection::open(path).with_context(|| format!("open {}", path.display()))?;
        use std::fs;
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.execute_batch(MIGRATION)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(MIGRATION)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn revision(&self) -> Result<u64> {
        let conn = self.conn.lock().expect("storage mutex poisoned");
        Ok(conn.query_row(
            "SELECT value FROM broker_meta WHERE key='revision'",
            [],
            |r| r.get(0),
        )?)
    }

    pub fn register(
        &self,
        snapshot: &InvocationSnapshot,
        credential: &str,
        publish: Option<&Publish<'_>>,
    ) -> Result<u64> {
        let mut conn = self.conn.lock().expect("storage mutex poisoned");
        let tx = conn.transaction()?;
        let revision = next_revision(&tx)?;
        let mut value = snapshot.clone();
        value.revision = revision;
        persist_snapshot(&tx, &value, credential)?;
        let event_id = synthetic_event_id("register", &value.invocation_id, revision);
        enqueue_transition(
            &tx,
            publish,
            &value,
            &event_id,
            HubEventMetadata {
                kind: EventKind::Enrichment,
                observed_at: value.updated_at,
                received_at: value.updated_at,
                failure: None,
            },
            None,
        )?;
        tx.commit()?;
        Ok(revision)
    }

    pub fn credential_matches(
        &self,
        id: &InvocationId,
        provider: &str,
        credential: &str,
    ) -> Result<bool> {
        let conn = self.conn.lock().expect("storage mutex poisoned");
        let row: Option<(String, String)> = conn
            .query_row(
                "SELECT provider,credential FROM invocations WHERE invocation_id=?1",
                [id.to_string()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        Ok(row.is_some_and(|(p, c)| {
            constant_time_eq(p.as_bytes(), provider.as_bytes())
                && constant_time_eq(c.as_bytes(), credential.as_bytes())
        }))
    }

    pub fn bind_child(
        &self,
        id: &InvocationId,
        credential: &str,
        pid: u32,
        identity: Option<String>,
        publish: Option<&Publish<'_>>,
    ) -> Result<InvocationSnapshot> {
        self.mutate_authenticated(
            id,
            credential,
            false,
            |snapshot| {
                snapshot.process.child_pid = Some(pid);
                snapshot.process.start_identity = identity;
                snapshot.lifecycle = Lifecycle::Alive;
            },
            EventKind::Enrichment,
            "bind_child",
            publish,
        )
    }

    pub fn mark_exit(
        &self,
        id: &InvocationId,
        credential: &str,
        code: Option<i32>,
        signal: Option<i32>,
        publish: Option<&Publish<'_>>,
    ) -> Result<InvocationSnapshot> {
        self.mutate_authenticated(
            id,
            credential,
            true,
            |snapshot| {
                snapshot.process.exit_code = code;
                snapshot.process.signal = signal;
                snapshot.lifecycle = Lifecycle::Exited;
            },
            EventKind::SessionEnded,
            "lifecycle_exit",
            publish,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn mutate_authenticated(
        &self,
        id: &InvocationId,
        credential: &str,
        clear_attention: bool,
        f: impl FnOnce(&mut InvocationSnapshot),
        kind: EventKind,
        synthetic_label: &str,
        publish: Option<&Publish<'_>>,
    ) -> Result<InvocationSnapshot> {
        let mut conn = self.conn.lock().expect("storage mutex poisoned");
        let tx = conn.transaction()?;
        let (stored_credential, raw, generation, completed): (String, String, u64, Option<u64>) = tx.query_row(
            "SELECT credential,snapshot_json,turn_generation,completed_generation FROM invocations WHERE invocation_id=?1", [id.to_string()],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        ).context("unknown invocation")?;
        if !constant_time_eq(stored_credential.as_bytes(), credential.as_bytes()) {
            bail!("invalid invocation credential");
        }
        let mut snapshot: InvocationSnapshot = serde_json::from_str(&raw)?;
        snapshot.turn_generation = generation;
        snapshot.completed_generation = completed;
        f(&mut snapshot);
        snapshot.revision = next_revision(&tx)?;
        snapshot.updated_at = Utc::now();
        snapshot.status = derive_status(snapshot.lifecycle, snapshot.activity);
        persist_snapshot(&tx, &snapshot, credential)?;
        if clear_attention {
            clear_attention_row(&tx, id)?;
        }
        let attention = current_attention(&tx, id)?;
        let event_id = synthetic_event_id(synthetic_label, id, snapshot.revision);
        enqueue_transition(
            &tx,
            publish,
            &snapshot,
            &event_id,
            HubEventMetadata {
                kind,
                observed_at: snapshot.updated_at,
                received_at: snapshot.updated_at,
                failure: None,
            },
            attention,
        )?;
        tx.commit()?;
        Ok(snapshot)
    }

    pub fn apply_event(
        &self,
        event: &NormalizedEvent,
        publish: Option<&Publish<'_>>,
    ) -> Result<Option<InvocationSnapshot>> {
        Ok(self
            .apply_event_with_context(event, None, None, publish)?
            .map(|update| update.snapshot))
    }

    pub fn apply_event_with_context(
        &self,
        event: &NormalizedEvent,
        attention: Option<&AttentionContext>,
        failure: Option<FailureContext>,
        publish: Option<&Publish<'_>>,
    ) -> Result<Option<AppliedUpdate>> {
        if attention.is_some_and(|context| {
            context.summary.is_empty()
                || context.summary.len() > ATTENTION_MAX_BYTES
                || context.summary.chars().count() > ATTENTION_MAX_CHARS
                || context.summary.chars().any(char::is_control)
        }) {
            bail!("attention context is not bounded normalized text");
        }
        let mut conn = self.conn.lock().expect("storage mutex poisoned");
        let tx = conn.transaction()?;
        if tx
            .query_row(
                "SELECT 1 FROM event_dedup WHERE event_id=?1",
                [&event.event_id],
                |_| Ok(()),
            )
            .optional()?
            .is_some()
        {
            return Ok(None);
        }
        let (credential, raw, generation, completed): (String, String, u64, Option<u64>) = tx.query_row(
            "SELECT credential,snapshot_json,turn_generation,completed_generation FROM invocations WHERE invocation_id=?1", [event.invocation_id.to_string()],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        ).context("unknown invocation")?;
        let mut snapshot: InvocationSnapshot = serde_json::from_str(&raw)?;
        snapshot.turn_generation = generation;
        snapshot.completed_generation = completed;
        let mut effective_kind = event.kind.clone();
        if matches!(event.kind, EventKind::Completed | EventKind::Failed)
            && snapshot.completed_generation == Some(snapshot.turn_generation)
        {
            effective_kind = EventKind::Enrichment;
        }
        if matches!(
            event.kind,
            EventKind::WaitingApproval | EventKind::WaitingInput
        ) && matches!(
            snapshot.activity,
            Activity::WaitingApproval | Activity::WaitingInput
        ) {
            effective_kind = EventKind::Enrichment;
        }
        reduce(&mut snapshot, event);
        let revision = next_revision(&tx)?;
        snapshot.revision = revision;
        snapshot.updated_at = Utc::now();
        snapshot.status = derive_status(snapshot.lifecycle, snapshot.activity);
        persist_snapshot(&tx, &snapshot, &credential)?;
        match event.kind {
            EventKind::WaitingApproval | EventKind::WaitingInput => {
                if let Some(context) = attention {
                    let active = ActiveAttention {
                        kind: event.kind.clone(),
                        context: context.clone(),
                    };
                    tx.execute("INSERT INTO local_active_attention(invocation_id,kind,attention_json,updated_at) VALUES (?1,?2,?3,?4) ON CONFLICT(invocation_id) DO UPDATE SET kind=excluded.kind,attention_json=excluded.attention_json,updated_at=excluded.updated_at", params![event.invocation_id.to_string(), serde_json::to_string(&event.kind)?, serde_json::to_string(&active)?, Utc::now().to_rfc3339()])?;
                }
            }
            EventKind::NewTurn
            | EventKind::Working
            | EventKind::Completed
            | EventKind::Failed
            | EventKind::SessionEnded => clear_attention_row(&tx, &event.invocation_id)?,
            EventKind::Enrichment => {}
        }
        tx.execute(
            "INSERT INTO event_dedup(event_id,committed_at) VALUES (?1,?2)",
            params![event.event_id, Utc::now().to_rfc3339()],
        )?;
        tx.execute("INSERT INTO normalized_events(event_id,invocation_id,revision,received_at,event_json) VALUES (?1,?2,?3,?4,?5)", params![event.event_id, event.invocation_id.to_string(), revision, event.received_at.to_rfc3339(), serde_json::to_string(event)?])?;
        let current = current_attention(&tx, &event.invocation_id)?;
        enqueue_transition(
            &tx,
            publish,
            &snapshot,
            &event.event_id,
            HubEventMetadata {
                kind: effective_kind.clone(),
                observed_at: event.observed_at,
                received_at: event.received_at,
                failure,
            },
            current,
        )?;
        tx.commit()?;
        Ok(Some(AppliedUpdate {
            snapshot,
            event: LiveEventMetadata {
                kind: effective_kind,
                attention: attention.cloned(),
                failure,
            },
        }))
    }

    pub fn snapshot(&self) -> Result<(u64, Vec<InvocationSnapshot>)> {
        let conn = self.conn.lock().expect("storage mutex poisoned");
        let revision = conn.query_row(
            "SELECT value FROM broker_meta WHERE key='revision'",
            [],
            |r| r.get(0),
        )?;
        let mut stmt = conn.prepare("SELECT snapshot_json,turn_generation,completed_generation FROM invocations ORDER BY rowid")?;
        let values = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, u64>(1)?,
                    row.get::<_, Option<u64>>(2)?,
                ))
            })?
            .map(|raw| {
                let (raw, generation, completed) = raw?;
                let mut snapshot: InvocationSnapshot = serde_json::from_str(&raw)?;
                snapshot.turn_generation = generation;
                snapshot.completed_generation = completed;
                Ok(snapshot)
            })
            .collect::<Result<Vec<_>>>()?;
        Ok((revision, values))
    }

    pub fn snapshot_with_attention(
        &self,
    ) -> Result<(
        u64,
        Vec<InvocationSnapshot>,
        BTreeMap<InvocationId, ActiveAttention>,
    )> {
        let conn = self.conn.lock().expect("storage mutex poisoned");
        let revision = conn.query_row(
            "SELECT value FROM broker_meta WHERE key='revision'",
            [],
            |r| r.get(0),
        )?;
        let mut snapshots_stmt = conn.prepare("SELECT snapshot_json,turn_generation,completed_generation FROM invocations ORDER BY rowid")?;
        let snapshots = snapshots_stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, u64>(1)?,
                    row.get::<_, Option<u64>>(2)?,
                ))
            })?
            .map(|row| {
                let (raw, generation, completed) = row?;
                let mut snapshot: InvocationSnapshot = serde_json::from_str(&raw)?;
                snapshot.turn_generation = generation;
                snapshot.completed_generation = completed;
                Ok(snapshot)
            })
            .collect::<Result<Vec<_>>>()?;
        let mut stmt =
            conn.prepare("SELECT invocation_id,attention_json FROM local_active_attention")?;
        let attention = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .map(|row| {
                let (id, raw) = row?;
                Ok((id.parse()?, serde_json::from_str(&raw)?))
            })
            .collect::<Result<BTreeMap<_, _>>>()?;
        Ok((revision, snapshots, attention))
    }

    /// Builds a canonical versioned source snapshot envelope at the current
    /// consistent revision. The mutex guarantees the snapshot and revision are
    /// captured atomically with respect to committed transitions.
    pub fn hub_source_snapshot(
        &self,
        source_id: &str,
        source_name: Option<&str>,
    ) -> Result<(u64, Vec<u8>)> {
        let (revision, invocations, active_attention) = self.snapshot_with_attention()?;
        let envelope = HubEnvelope::Snapshot {
            schema_version: HUB_SCHEMA_VERSION,
            source: SourceIdentity {
                id: source_id.to_owned(),
                display_name: source_name.map(str::to_owned),
                capabilities: SourceCapabilities::default(),
            },
            revision,
            invocations,
            active_attention,
        };
        Ok((revision, serde_json::to_vec(&envelope)?))
    }

    /// Returns true when the hub sink has not yet delivered a baseline
    /// snapshot (new sink or repair required).
    pub fn hub_snapshot_due(&self, sink: &str) -> Result<bool> {
        let conn = self.conn.lock().expect("storage mutex poisoned");
        conn.execute(
            "INSERT OR IGNORE INTO hub_sink_state(sink_name,snapshot_revision) VALUES (?1,NULL)",
            [sink],
        )?;
        Ok(conn
            .query_row(
                "SELECT snapshot_revision FROM hub_sink_state WHERE sink_name=?1",
                [sink],
                |r| r.get::<_, Option<u64>>(0),
            )?
            .is_none())
    }

    /// Records a successful baseline snapshot delivery at `revision` and
    /// removes outbox updates subsumed by that snapshot.
    pub fn hub_snapshot_delivered(&self, sink: &str, revision: u64) -> Result<()> {
        let conn = self.conn.lock().expect("storage mutex poisoned");
        conn.execute(
            "INSERT INTO hub_sink_state(sink_name,snapshot_revision) VALUES (?1,?2) ON CONFLICT(sink_name) DO UPDATE SET snapshot_revision=excluded.snapshot_revision",
            params![sink, revision],
        )?;
        conn.execute(
            "DELETE FROM sink_outbox WHERE sink_name=?1 AND revision<=?2",
            params![sink, revision],
        )?;
        Ok(())
    }

    /// Marks the sink as needing a fresh baseline snapshot, for example after
    /// the receiver reported that it has no state for this source.
    pub fn hub_reset_snapshot(&self, sink: &str) -> Result<()> {
        let conn = self.conn.lock().expect("storage mutex poisoned");
        conn.execute(
            "INSERT INTO hub_sink_state(sink_name,snapshot_revision) VALUES (?1,NULL) ON CONFLICT(sink_name) DO UPDATE SET snapshot_revision=NULL",
            [sink],
        )?;
        Ok(())
    }

    pub fn invocation(&self, id: &InvocationId) -> Result<InvocationSnapshot> {
        let conn = self.conn.lock().expect("storage mutex poisoned");
        let (raw, generation, completed): (String, u64, Option<u64>) = conn
            .query_row(
                "SELECT snapshot_json,turn_generation,completed_generation FROM invocations WHERE invocation_id=?1",
                [id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .context("unknown invocation")?;
        let mut snapshot: InvocationSnapshot = serde_json::from_str(&raw)?;
        snapshot.turn_generation = generation;
        snapshot.completed_generation = completed;
        Ok(snapshot)
    }

    pub fn reconcile(
        &self,
        is_alive: impl Fn(u32, Option<&str>) -> bool,
        retention_days: u64,
        publish: Option<&Publish<'_>>,
    ) -> Result<usize> {
        let (_, snapshots) = self.snapshot()?;
        let mut changed = 0;
        for snapshot in snapshots {
            if matches!(snapshot.lifecycle, Lifecycle::Alive | Lifecycle::Starting)
                && !snapshot
                    .process
                    .child_pid
                    .is_some_and(|pid| is_alive(pid, snapshot.process.start_identity.as_deref()))
            {
                let mut conn = self.conn.lock().expect("storage mutex poisoned");
                let tx = conn.transaction()?;
                let mut lost = snapshot;
                lost.lifecycle = Lifecycle::Lost;
                lost.status = derive_status(lost.lifecycle, lost.activity);
                lost.updated_at = Utc::now();
                lost.revision = next_revision(&tx)?;
                let credential: String = tx.query_row(
                    "SELECT credential FROM invocations WHERE invocation_id=?1",
                    [lost.invocation_id.to_string()],
                    |r| r.get(0),
                )?;
                persist_snapshot(&tx, &lost, &credential)?;
                clear_attention_row(&tx, &lost.invocation_id)?;
                let event_id =
                    synthetic_event_id("reconcile_lost", &lost.invocation_id, lost.revision);
                enqueue_transition(
                    &tx,
                    publish,
                    &lost,
                    &event_id,
                    HubEventMetadata {
                        kind: EventKind::SessionEnded,
                        observed_at: lost.updated_at,
                        received_at: lost.updated_at,
                        failure: None,
                    },
                    None,
                )?;
                tx.commit()?;
                changed += 1;
            }
        }
        let cutoff = (Utc::now()
            - Duration::days(i64::try_from(retention_days).unwrap_or(i64::MAX)))
        .to_rfc3339();
        let conn = self.conn.lock().expect("storage mutex poisoned");
        conn.execute(
            "DELETE FROM invocations WHERE stopped_at IS NOT NULL AND stopped_at < ?1",
            [cutoff],
        )?;
        Ok(changed)
    }

    pub fn due_outbox(&self, limit: usize) -> Result<Vec<OutboxRecord>> {
        let conn = self.conn.lock().expect("storage mutex poisoned");
        let mut stmt = conn.prepare("SELECT sink_name,event_id,payload,attempts FROM sink_outbox WHERE next_attempt_at<=?1 ORDER BY revision LIMIT ?2")?;
        Ok(stmt
            .query_map(params![Utc::now().to_rfc3339(), limit], |r| {
                Ok(OutboxRecord {
                    sink_name: r.get(0)?,
                    event_id: r.get(1)?,
                    payload: r.get(2)?,
                    attempts: r.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub fn acknowledge(&self, sink: &str, event: &str) -> Result<()> {
        self.conn.lock().expect("storage mutex poisoned").execute(
            "DELETE FROM sink_outbox WHERE sink_name=?1 AND event_id=?2",
            params![sink, event],
        )?;
        Ok(())
    }
    pub fn retry(&self, sink: &str, event: &str, attempts: u32) -> Result<()> {
        let delay = 2_i64.saturating_pow(attempts.min(10)).min(300);
        self.conn.lock().expect("storage mutex poisoned").execute("UPDATE sink_outbox SET attempts=attempts+1,next_attempt_at=?3 WHERE sink_name=?1 AND event_id=?2", params![sink,event,(Utc::now()+Duration::seconds(delay)).to_rfc3339()])?;
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct OutboxRecord {
    pub sink_name: String,
    pub event_id: String,
    pub payload: Vec<u8>,
    pub attempts: u32,
}

#[derive(Debug, Clone)]
pub struct AppliedUpdate {
    pub snapshot: InvocationSnapshot,
    pub event: LiveEventMetadata,
}

/// Stable source-scoped identity for transitions that have no provider event.
fn synthetic_event_id(label: &str, invocation_id: &InvocationId, revision: u64) -> String {
    format!("synthetic:{label}:{invocation_id}:{revision}")
}

fn current_attention(tx: &Transaction<'_>, id: &InvocationId) -> Result<Option<ActiveAttention>> {
    let raw: Option<String> = tx
        .query_row(
            "SELECT attention_json FROM local_active_attention WHERE invocation_id=?1",
            [id.to_string()],
            |r| r.get(0),
        )
        .optional()?;
    raw.map(|raw| serde_json::from_str(&raw).map_err(Into::into))
        .transpose()
}

/// Enqueues one delivery per enabled sink in the same transaction as the
/// committed transition. Hub sinks receive the canonical versioned update
/// envelope; legacy stdout/HTTP sinks keep the sanitized `SinkEvent` shape.
fn enqueue_transition(
    tx: &Transaction<'_>,
    publish: Option<&Publish<'_>>,
    snapshot: &InvocationSnapshot,
    event_id: &str,
    event: HubEventMetadata,
    attention: Option<ActiveAttention>,
) -> Result<()> {
    let Some(publish) = publish else {
        return Ok(());
    };
    for (name, config) in publish.sinks.iter().filter(|(_, c)| c.enabled()) {
        let payload = if config.is_hub() {
            if publish.source_id.is_empty() {
                continue;
            }
            serde_json::to_vec(&HubEnvelope::Update {
                schema_version: HUB_SCHEMA_VERSION,
                source_id: publish.source_id.to_owned(),
                event_id: event_id.to_owned(),
                revision: snapshot.revision,
                event: event.clone(),
                snapshot: Box::new(snapshot.clone()),
                attention: attention.clone(),
            })?
        } else {
            let sink_event = SinkEvent {
                schema_version: SCHEMA_VERSION,
                event_id: event_id.to_owned(),
                revision: snapshot.revision,
                snapshot: snapshot.clone(),
            };
            let mut safe = serde_json::to_value(&sink_event)?;
            if !config.fields().is_empty() {
                if let Some(snapshot) = safe
                    .get_mut("snapshot")
                    .and_then(serde_json::Value::as_object_mut)
                {
                    snapshot.retain(|field, _| {
                        matches!(
                            field.as_str(),
                            "schema_version"
                                | "revision"
                                | "invocation_id"
                                | "provider"
                                | "lifecycle"
                                | "activity"
                                | "status"
                                | "updated_at"
                        ) || config.fields().iter().any(|allowed| allowed == field)
                    });
                }
            }
            serde_json::to_vec(&safe)?
        };
        if payload.len() <= config.max_payload_bytes() {
            tx.execute(
                "INSERT OR IGNORE INTO sink_outbox(sink_name,event_id,revision,payload,next_attempt_at) SELECT ?1,?2,?3,?4,?5 WHERE (SELECT COUNT(*) FROM sink_outbox WHERE sink_name=?1) < ?6",
                params![name, event_id, snapshot.revision, payload, Utc::now().to_rfc3339(), MAX_OUTBOX_RECORDS_PER_SINK],
            )?;
        }
    }
    Ok(())
}

fn clear_attention_row(tx: &Transaction<'_>, id: &InvocationId) -> Result<()> {
    tx.execute(
        "DELETE FROM local_active_attention WHERE invocation_id=?1",
        [id.to_string()],
    )?;
    Ok(())
}

fn next_revision(tx: &Transaction<'_>) -> Result<u64> {
    tx.execute(
        "UPDATE broker_meta SET value=value+1 WHERE key='revision'",
        [],
    )?;
    Ok(tx.query_row(
        "SELECT value FROM broker_meta WHERE key='revision'",
        [],
        |r| r.get(0),
    )?)
}
fn persist_snapshot(
    tx: &Transaction<'_>,
    snapshot: &InvocationSnapshot,
    credential: &str,
) -> Result<()> {
    let stopped = matches!(snapshot.lifecycle, Lifecycle::Exited | Lifecycle::Lost)
        .then(|| snapshot.updated_at.to_rfc3339());
    tx.execute("INSERT INTO invocations(invocation_id,provider,credential,snapshot_json,stopped_at,turn_generation,completed_generation) VALUES (?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(invocation_id) DO UPDATE SET provider=excluded.provider,credential=excluded.credential,snapshot_json=excluded.snapshot_json,stopped_at=excluded.stopped_at,turn_generation=excluded.turn_generation,completed_generation=excluded.completed_generation", params![snapshot.invocation_id.to_string(), snapshot.provider, credential, serde_json::to_string(snapshot)?, stopped,snapshot.turn_generation,snapshot.completed_generation])?;
    Ok(())
}

fn reduce(snapshot: &mut InvocationSnapshot, event: &NormalizedEvent) {
    match event.kind {
        EventKind::NewTurn => {
            snapshot.turn_generation += 1;
            snapshot.completed_generation = None;
            snapshot.activity = Activity::Working;
        }
        EventKind::Working if snapshot.completed_generation != Some(snapshot.turn_generation) => {
            snapshot.activity = Activity::Working
        }
        EventKind::WaitingInput => snapshot.activity = Activity::WaitingInput,
        EventKind::WaitingApproval => snapshot.activity = Activity::WaitingApproval,
        EventKind::Completed | EventKind::Failed => {
            snapshot.activity = Activity::Idle;
            snapshot.completed_generation = Some(snapshot.turn_generation);
        }
        EventKind::SessionEnded => snapshot.lifecycle = Lifecycle::Exited,
        EventKind::Enrichment | EventKind::Working => {}
    }
    if let Some(id) = &event.provider_session_id {
        snapshot.provider_session = Some(sessiontap_core::domain::ProviderSession {
            id: id.clone(),
            name: event.provider_session_name.clone().or_else(|| {
                snapshot
                    .provider_session
                    .as_ref()
                    .filter(|session| session.id == *id)
                    .and_then(|session| session.name.clone())
            }),
        });
    }
    if event.usage.is_some() {
        snapshot.usage.clone_from(&event.usage);
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0_u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use sessiontap_core::domain::{Capabilities, ProcessMetadata};
    use uuid::Uuid;
    fn snapshot() -> InvocationSnapshot {
        let now = Utc::now();
        InvocationSnapshot {
            schema_version: SCHEMA_VERSION,
            revision: 0,
            invocation_id: InvocationId::new(),
            provider: "claude".into(),
            executable: "claude".into(),
            args: vec![],
            cwd: "/tmp".into(),
            process: ProcessMetadata::default(),
            created_at: now,
            updated_at: now,
            lifecycle: Lifecycle::Alive,
            activity: Activity::Idle,
            status: derive_status(Lifecycle::Alive, Activity::Idle),
            provider_session: None,
            usage: None,
            repository: None,
            multiplexer: None,
            capabilities: Capabilities::default(),
            turn_generation: 0,
            completed_generation: None,
        }
    }
    fn event(s: &InvocationSnapshot, kind: EventKind, id: &str) -> NormalizedEvent {
        NormalizedEvent {
            schema_version: 1,
            event_id: id.into(),
            invocation_id: s.invocation_id.clone(),
            provider_event_id: None,
            provider: s.provider.clone(),
            observed_at: Utc::now(),
            received_at: Utc::now(),
            source: "test".into(),
            kind,
            provider_session_id: None,
            provider_session_name: None,
            usage: None,
            turn_id: None,
        }
    }
    fn hub_sinks() -> BTreeMap<String, SinkConfig> {
        let mut sinks = BTreeMap::new();
        sinks.insert(
            "hub".into(),
            SinkConfig::Hub {
                enabled: true,
                url: "http://127.0.0.1:8931/ingest".into(),
                token_env: None,
                token_file: None,
                timeout_ms: 100,
                max_payload_bytes: 64 * 1024,
                trusted_addresses: vec![],
            },
        );
        sinks
    }
    fn hub_publish(sinks: &BTreeMap<String, SinkConfig>) -> Publish<'_> {
        Publish {
            sinks,
            source_id: "host",
            source_name: Some("Host"),
        }
    }
    fn hub_updates(db: &Storage) -> Vec<HubEnvelope> {
        let conn = db.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT payload FROM sink_outbox WHERE sink_name='hub' ORDER BY revision")
            .unwrap();
        stmt.query_map([], |r| r.get::<_, Vec<u8>>(0))
            .unwrap()
            .map(|raw| serde_json::from_slice(&raw.unwrap()).unwrap())
            .collect()
    }
    #[test]
    fn duplicate_and_late_work_are_ignored() {
        let db = Storage::memory().unwrap();
        let s = snapshot();
        db.register(&s, "secret", None).unwrap();
        db.apply_event(&event(&s, EventKind::NewTurn, "1"), None)
            .unwrap();
        db.apply_event(&event(&s, EventKind::Completed, "2"), None)
            .unwrap();
        let late = db
            .apply_event(&event(&s, EventKind::Working, "3"), None)
            .unwrap()
            .unwrap();
        assert_eq!(late.activity, Activity::Idle);
        assert!(
            db.apply_event(&event(&s, EventKind::Working, "3"), None)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn provider_session_name_updates_and_is_preserved() {
        let mut snapshot = snapshot();
        let mut named = event(&snapshot, EventKind::Working, "named");
        named.provider_session_id = Some("provider-session".into());
        named.provider_session_name = Some("First name".into());
        reduce(&mut snapshot, &named);
        assert_eq!(
            snapshot.provider_session.as_ref().unwrap().name.as_deref(),
            Some("First name")
        );

        let mut unnamed = event(&snapshot, EventKind::Working, "unnamed");
        unnamed.provider_session_id = Some("provider-session".into());
        reduce(&mut snapshot, &unnamed);
        assert_eq!(
            snapshot.provider_session.as_ref().unwrap().name.as_deref(),
            Some("First name")
        );

        let mut renamed = event(&snapshot, EventKind::Working, "renamed");
        renamed.provider_session_id = Some("provider-session".into());
        renamed.provider_session_name = Some("Second name".into());
        reduce(&mut snapshot, &renamed);
        assert_eq!(
            snapshot.provider_session.unwrap().name.as_deref(),
            Some("Second name")
        );
    }
    #[test]
    fn active_attention_replaces_restores_and_clears() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state.sqlite3");
        let s = snapshot();
        let db = Storage::open(&path).unwrap();
        db.register(&s, "secret", None).unwrap();
        let first = AttentionContext {
            summary: "First".into(),
            source: sessiontap_core::domain::AttentionSource::Question,
        };
        let second = AttentionContext {
            summary: "Second".into(),
            source: sessiontap_core::domain::AttentionSource::Description,
        };
        let update = db
            .apply_event_with_context(
                &event(&s, EventKind::WaitingInput, "a"),
                Some(&first),
                None,
                None,
            )
            .unwrap()
            .unwrap();
        assert_eq!(update.event.kind, EventKind::WaitingInput);
        let update = db
            .apply_event_with_context(
                &event(&s, EventKind::WaitingInput, "b"),
                Some(&second),
                None,
                None,
            )
            .unwrap()
            .unwrap();
        assert_eq!(update.event.kind, EventKind::Enrichment);
        drop(db);
        let db = Storage::open(&path).unwrap();
        let (_, _, active) = db.snapshot_with_attention().unwrap();
        assert_eq!(active[&s.invocation_id].context.summary, "Second");
        db.apply_event(&event(&s, EventKind::Working, "c"), None)
            .unwrap();
        assert!(db.snapshot_with_attention().unwrap().2.is_empty());
    }

    #[test]
    fn repeated_terminal_is_enrichment_and_private_context_is_not_persisted_publicly() {
        let db = Storage::memory().unwrap();
        let s = snapshot();
        db.register(&s, "secret", None).unwrap();
        let attention = AttentionContext {
            summary: "PRIVATE".into(),
            source: sessiontap_core::domain::AttentionSource::Description,
        };
        db.apply_event_with_context(
            &event(&s, EventKind::WaitingApproval, "a"),
            Some(&attention),
            None,
            None,
        )
        .unwrap();
        let first = db
            .apply_event_with_context(&event(&s, EventKind::Completed, "b"), None, None, None)
            .unwrap()
            .unwrap();
        let second = db
            .apply_event_with_context(&event(&s, EventKind::Completed, "c"), None, None, None)
            .unwrap()
            .unwrap();
        assert_eq!(first.event.kind, EventKind::Completed);
        assert_eq!(second.event.kind, EventKind::Enrichment);
        assert!(
            !serde_json::to_string(&db.invocation(&s.invocation_id).unwrap())
                .unwrap()
                .contains("PRIVATE")
        );
    }

    #[test]
    fn local_context_never_enters_event_history_or_sink_outbox() {
        let db = Storage::memory().unwrap();
        let s = snapshot();
        db.register(&s, "secret", None).unwrap();
        let mut sinks = BTreeMap::new();
        sinks.insert(
            "debug".into(),
            SinkConfig::Stdout {
                enabled: true,
                fields: vec![],
            },
        );
        let publish = Publish {
            sinks: &sinks,
            source_id: "",
            source_name: None,
        };
        let attention = AttentionContext {
            summary: "PRIVATE-CONTEXT".into(),
            source: sessiontap_core::domain::AttentionSource::Description,
        };
        db.apply_event_with_context(
            &event(&s, EventKind::WaitingApproval, "private-boundary"),
            Some(&attention),
            Some(FailureContext::Unknown),
            Some(&publish),
        )
        .unwrap();
        let conn = db.conn.lock().unwrap();
        let history: String = conn
            .query_row(
                "SELECT event_json FROM normalized_events WHERE event_id='private-boundary'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let payload: Vec<u8> = conn
            .query_row(
                "SELECT payload FROM sink_outbox WHERE event_id='private-boundary'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let persisted: String = conn
            .query_row(
                "SELECT snapshot_json FROM invocations WHERE invocation_id=?1",
                [s.invocation_id.to_string()],
                |r| r.get(0),
            )
            .unwrap();
        for value in [history.as_bytes(), payload.as_slice(), persisted.as_bytes()] {
            assert!(!String::from_utf8_lossy(value).contains("PRIVATE-CONTEXT"));
        }
    }
    #[test]
    fn concurrent_hook_burst_is_idempotent() {
        use std::sync::Arc;
        let db = Arc::new(Storage::memory().unwrap());
        let s = snapshot();
        db.register(&s, "s", None).unwrap();
        let joins = (0..20)
            .map(|n| {
                let db = db.clone();
                let e = event(&s, EventKind::Working, &format!("{n}"));
                std::thread::spawn(move || db.apply_event(&e, None).unwrap())
            })
            .collect::<Vec<_>>();
        for j in joins {
            j.join().unwrap();
        }
        assert_eq!(db.revision().unwrap(), 21);
    }
    #[test]
    fn credential_and_provider_must_match() {
        let db = Storage::memory().unwrap();
        let s = snapshot();
        db.register(&s, "secret", None).unwrap();
        assert!(
            db.credential_matches(&s.invocation_id, "claude", "secret")
                .unwrap()
        );
        assert!(
            !db.credential_matches(&s.invocation_id, "codex", "secret")
                .unwrap()
        );
        assert!(
            !db.credential_matches(&s.invocation_id, "claude", "bad")
                .unwrap()
        );
    }
    #[test]
    fn event_id_accepts_uuid() {
        assert!(Uuid::parse_str(&Uuid::new_v4().to_string()).is_ok());
    }
    #[test]
    fn outbox_survives_restart_and_acknowledges_once() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state.sqlite3");
        let snapshot = snapshot();
        let mut sinks = BTreeMap::new();
        sinks.insert(
            "remote".into(),
            SinkConfig::Http {
                enabled: true,
                url: "http://127.0.0.1:8787/events".into(),
                token_env: None,
                token_file: None,
                timeout_ms: 100,
                max_payload_bytes: 4096,
                fields: vec!["cwd".into()],
            },
        );
        let publish = Publish {
            sinks: &sinks,
            source_id: "",
            source_name: None,
        };
        {
            let db = Storage::open(&path).unwrap();
            db.register(&snapshot, "secret", Some(&publish)).unwrap();
            db.apply_event(
                &event(&snapshot, EventKind::NewTurn, "stable-event"),
                Some(&publish),
            )
            .unwrap();
        }
        let db = Storage::open(&path).unwrap();
        let mut records = db.due_outbox(10).unwrap();
        records.retain(|r| r.event_id == "stable-event");
        assert_eq!(records.len(), 1);
        db.acknowledge("remote", "stable-event").unwrap();
        assert!(
            db.due_outbox(10)
                .unwrap()
                .iter()
                .all(|r| r.event_id != "stable-event")
        );
    }
    #[test]
    fn startup_reconciles_lost_processes_and_expires_old_stopped_rows() {
        let db = Storage::memory().unwrap();
        let mut live = snapshot();
        live.process.child_pid = Some(424_242);
        db.register(&live, "live-secret", None).unwrap();
        assert_eq!(db.reconcile(|_, _| false, 7, None).unwrap(), 1);
        assert_eq!(
            db.invocation(&live.invocation_id).unwrap().lifecycle,
            Lifecycle::Lost
        );

        let mut old = snapshot();
        old.invocation_id = InvocationId::new();
        old.lifecycle = Lifecycle::Exited;
        old.updated_at = Utc::now() - Duration::days(8);
        old.status = derive_status(old.lifecycle, old.activity);
        db.register(&old, "old-secret", None).unwrap();
        db.reconcile(|_, _| false, 7, None).unwrap();
        assert!(db.invocation(&old.invocation_id).is_err());
    }

    #[test]
    fn database_is_private_and_rejects_symlink_target() {
        use std::os::unix::{fs::PermissionsExt, fs::symlink};
        let temp = tempfile::tempdir().unwrap();
        let database = temp.path().join("sessiontap.sqlite3");
        let db = Storage::open(&database).unwrap();
        assert_eq!(
            database.metadata().unwrap().permissions().mode() & 0o777,
            0o600
        );
        drop(db);

        let victim = temp.path().join("victim.sqlite3");
        std::fs::write(&victim, b"unchanged").unwrap();
        let link = temp.path().join("linked.sqlite3");
        symlink(&victim, &link).unwrap();
        assert!(Storage::open(&link).is_err());
        assert_eq!(std::fs::read(victim).unwrap(), b"unchanged");
    }

    #[test]
    fn concurrent_session_load_keeps_unique_snapshots_and_revisions() {
        use std::sync::Arc;
        let db = Arc::new(Storage::memory().unwrap());
        let joins = (0..128)
            .map(|_| {
                let db = Arc::clone(&db);
                std::thread::spawn(move || {
                    let snapshot = snapshot();
                    let id = snapshot.invocation_id.clone();
                    db.register(&snapshot, "credential", None).unwrap();
                    id
                })
            })
            .collect::<Vec<_>>();
        let ids = joins
            .into_iter()
            .map(|join| join.join().unwrap())
            .collect::<std::collections::HashSet<_>>();
        let (revision, snapshots) = db.snapshot().unwrap();
        assert_eq!(ids.len(), 128);
        assert_eq!(snapshots.len(), 128);
        assert_eq!(revision, 128);
    }

    #[test]
    fn sink_backlog_is_bounded_under_burst_load() {
        let db = Storage::memory().unwrap();
        let snapshot = snapshot();
        let mut sinks = BTreeMap::new();
        sinks.insert(
            "slow".into(),
            SinkConfig::Http {
                enabled: true,
                url: "http://127.0.0.1:9/events".into(),
                token_env: None,
                token_file: None,
                timeout_ms: 10,
                max_payload_bytes: 64 * 1024,
                fields: vec![],
            },
        );
        let publish = Publish {
            sinks: &sinks,
            source_id: "",
            source_name: None,
        };
        db.register(&snapshot, "credential", Some(&publish))
            .unwrap();
        for index in 0..(MAX_OUTBOX_RECORDS_PER_SINK + 128) {
            db.apply_event(
                &event(&snapshot, EventKind::Working, &format!("load-{index}")),
                Some(&publish),
            )
            .unwrap();
        }
        let conn = db.conn.lock().unwrap();
        let count: u64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sink_outbox WHERE sink_name='slow'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, MAX_OUTBOX_RECORDS_PER_SINK);
    }

    #[test]
    fn registration_binding_exit_and_reconciliation_are_hub_visible() {
        let db = Storage::memory().unwrap();
        let sinks = hub_sinks();
        let publish = hub_publish(&sinks);
        let mut s = snapshot();
        s.lifecycle = Lifecycle::Starting;
        s.status = derive_status(Lifecycle::Starting, Activity::Idle);
        db.register(&s, "secret", Some(&publish)).unwrap();
        db.bind_child(&s.invocation_id, "secret", 42, None, Some(&publish))
            .unwrap();
        db.mark_exit(&s.invocation_id, "secret", Some(0), None, Some(&publish))
            .unwrap();
        let updates = hub_updates(&db);
        assert_eq!(updates.len(), 3);
        let kinds: Vec<EventKind> = updates
            .iter()
            .map(|u| match u {
                HubEnvelope::Update { event, .. } => event.kind.clone(),
                _ => panic!("expected update"),
            })
            .collect();
        assert_eq!(
            kinds,
            vec![
                EventKind::Enrichment,
                EventKind::Enrichment,
                EventKind::SessionEnded
            ]
        );
        for (index, update) in updates.iter().enumerate() {
            match update {
                HubEnvelope::Update {
                    source_id,
                    snapshot,
                    revision,
                    ..
                } => {
                    assert_eq!(source_id, "host");
                    assert_eq!(*revision, snapshot.revision);
                    assert_eq!(snapshot.revision, (index + 1) as u64);
                }
                _ => panic!("expected update"),
            }
        }
    }

    #[test]
    fn reconciliation_lost_transition_reaches_hub_sinks() {
        let db = Storage::memory().unwrap();
        let sinks = hub_sinks();
        let publish = hub_publish(&sinks);
        let mut s = snapshot();
        s.process.child_pid = Some(424_242);
        db.register(&s, "secret", Some(&publish)).unwrap();
        assert_eq!(db.reconcile(|_, _| false, 7, Some(&publish)).unwrap(), 1);
        let updates = hub_updates(&db);
        let last = updates.last().unwrap();
        match last {
            HubEnvelope::Update {
                event, snapshot, ..
            } => {
                assert_eq!(event.kind, EventKind::SessionEnded);
                assert_eq!(snapshot.lifecycle, Lifecycle::Lost);
            }
            _ => panic!("expected update"),
        }
    }

    #[test]
    fn hub_update_carries_attention_then_explicit_null_when_cleared() {
        let db = Storage::memory().unwrap();
        let sinks = hub_sinks();
        let publish = hub_publish(&sinks);
        let s = snapshot();
        db.register(&s, "secret", Some(&publish)).unwrap();
        let attention = AttentionContext {
            summary: "Approve tests".into(),
            source: sessiontap_core::domain::AttentionSource::ToolSummary,
        };
        db.apply_event_with_context(
            &event(&s, EventKind::WaitingApproval, "wait-1"),
            Some(&attention),
            None,
            Some(&publish),
        )
        .unwrap();
        db.apply_event(&event(&s, EventKind::Working, "resume"), Some(&publish))
            .unwrap();
        let updates = hub_updates(&db);
        match &updates[1] {
            HubEnvelope::Update {
                attention, event, ..
            } => {
                assert_eq!(event.kind, EventKind::WaitingApproval);
                let active = attention.as_ref().unwrap();
                assert_eq!(active.kind, EventKind::WaitingApproval);
                assert_eq!(active.context.summary, "Approve tests");
            }
            _ => panic!("expected update"),
        }
        match &updates[2] {
            HubEnvelope::Update { attention, .. } => assert!(attention.is_none()),
            _ => panic!("expected update"),
        }
        let raw: Vec<u8> = db
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT payload FROM sink_outbox WHERE sink_name='hub' AND event_id='resume'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        assert!(value["attention"].is_null());
        assert!(value.as_object().unwrap().contains_key("attention"));
    }

    #[test]
    fn hub_update_carries_failure_category_without_raw_context() {
        let db = Storage::memory().unwrap();
        let sinks = hub_sinks();
        let publish = hub_publish(&sinks);
        let s = snapshot();
        db.register(&s, "secret", Some(&publish)).unwrap();
        db.apply_event_with_context(
            &event(&s, EventKind::Failed, "failed-1"),
            None,
            Some(FailureContext::RateLimited),
            Some(&publish),
        )
        .unwrap();
        match hub_updates(&db).pop().unwrap() {
            HubEnvelope::Update { event, .. } => {
                assert_eq!(event.kind, EventKind::Failed);
                assert_eq!(event.failure, Some(FailureContext::RateLimited));
            }
            _ => panic!("expected update"),
        }
    }

    #[test]
    fn semantically_suppressed_duplicates_are_not_delivered() {
        let db = Storage::memory().unwrap();
        let sinks = hub_sinks();
        let publish = hub_publish(&sinks);
        let s = snapshot();
        db.register(&s, "secret", Some(&publish)).unwrap();
        db.apply_event(&event(&s, EventKind::NewTurn, "turn"), Some(&publish))
            .unwrap();
        assert!(
            db.apply_event(&event(&s, EventKind::NewTurn, "turn"), Some(&publish))
                .unwrap()
                .is_none()
        );
        let updates = hub_updates(&db);
        // registration plus exactly one accepted turn; the duplicate added none
        assert_eq!(updates.len(), 2);
        assert_eq!(
            updates
                .iter()
                .filter(|u| matches!(u, HubEnvelope::Update { event_id, .. } if event_id == "turn"))
                .count(),
            1
        );
    }

    #[test]
    fn repeated_waiting_attention_is_enrichment_but_still_delivered_with_state() {
        let db = Storage::memory().unwrap();
        let sinks = hub_sinks();
        let publish = hub_publish(&sinks);
        let s = snapshot();
        db.register(&s, "secret", Some(&publish)).unwrap();
        let attention = AttentionContext {
            summary: "First".into(),
            source: sessiontap_core::domain::AttentionSource::Question,
        };
        db.apply_event_with_context(
            &event(&s, EventKind::WaitingInput, "w1"),
            Some(&attention),
            None,
            Some(&publish),
        )
        .unwrap();
        let second = AttentionContext {
            summary: "Second".into(),
            source: sessiontap_core::domain::AttentionSource::Question,
        };
        db.apply_event_with_context(
            &event(&s, EventKind::WaitingInput, "w2"),
            Some(&second),
            None,
            Some(&publish),
        )
        .unwrap();
        let updates = hub_updates(&db);
        assert_eq!(updates.len(), 3);
        match &updates[2] {
            HubEnvelope::Update {
                event, attention, ..
            } => {
                assert_eq!(event.kind, EventKind::Enrichment);
                assert_eq!(attention.as_ref().unwrap().context.summary, "Second");
            }
            _ => panic!("expected update"),
        }
    }

    #[test]
    fn retry_identity_is_stable_across_outbox_redelivery() {
        let db = Storage::memory().unwrap();
        let sinks = hub_sinks();
        let publish = hub_publish(&sinks);
        let s = snapshot();
        db.register(&s, "secret", Some(&publish)).unwrap();
        let before = hub_updates(&db);
        db.retry(
            "hub",
            match &before[0] {
                HubEnvelope::Update { event_id, .. } => event_id,
                _ => panic!("expected update"),
            },
            0,
        )
        .unwrap();
        let after = hub_updates(&db);
        assert_eq!(before, after);
    }

    #[test]
    fn snapshot_delivery_subsumes_earlier_updates_and_keeps_later_ones() {
        let db = Storage::memory().unwrap();
        let sinks = hub_sinks();
        let publish = hub_publish(&sinks);
        let s = snapshot();
        db.register(&s, "secret", Some(&publish)).unwrap();
        db.apply_event(&event(&s, EventKind::NewTurn, "early"), Some(&publish))
            .unwrap();
        let (revision, payload) = db.hub_source_snapshot("host", Some("Host")).unwrap();
        let envelope: HubEnvelope = serde_json::from_slice(&payload).unwrap();
        match envelope {
            HubEnvelope::Snapshot {
                source,
                revision: snapshot_revision,
                invocations,
                ..
            } => {
                assert_eq!(source.id, "host");
                assert_eq!(source.display_name.as_deref(), Some("Host"));
                assert_eq!(snapshot_revision, revision);
                assert_eq!(invocations.len(), 1);
                assert_eq!(invocations[0].revision, 2);
            }
            _ => panic!("expected snapshot"),
        }
        assert!(db.hub_snapshot_due("hub").unwrap());
        db.hub_snapshot_delivered("hub", revision).unwrap();
        assert!(!db.hub_snapshot_due("hub").unwrap());
        assert!(hub_updates(&db).is_empty());
        db.apply_event(&event(&s, EventKind::Working, "later"), Some(&publish))
            .unwrap();
        let updates = hub_updates(&db);
        assert_eq!(updates.len(), 1);
        match &updates[0] {
            HubEnvelope::Update {
                revision: later, ..
            } => assert!(*later > revision),
            _ => panic!("expected update"),
        }
    }

    #[test]
    fn snapshot_reset_allows_repair_after_receiver_state_loss() {
        let db = Storage::memory().unwrap();
        let sinks = hub_sinks();
        let publish = hub_publish(&sinks);
        let s = snapshot();
        db.register(&s, "secret", Some(&publish)).unwrap();
        let (revision, _) = db.hub_source_snapshot("host", None).unwrap();
        db.hub_snapshot_delivered("hub", revision).unwrap();
        assert!(!db.hub_snapshot_due("hub").unwrap());
        db.hub_reset_snapshot("hub").unwrap();
        assert!(db.hub_snapshot_due("hub").unwrap());
    }

    #[test]
    fn hub_payload_limit_drops_oversized_updates() {
        let db = Storage::memory().unwrap();
        let mut sinks = BTreeMap::new();
        sinks.insert(
            "hub".into(),
            SinkConfig::Hub {
                enabled: true,
                url: "http://127.0.0.1:8931/ingest".into(),
                token_env: None,
                token_file: None,
                timeout_ms: 100,
                max_payload_bytes: 64,
                trusted_addresses: vec![],
            },
        );
        let publish = hub_publish(&sinks);
        let s = snapshot();
        db.register(&s, "secret", Some(&publish)).unwrap();
        assert!(hub_updates(&db).is_empty());
    }

    #[test]
    fn hub_envelopes_are_skipped_without_source_identity() {
        let db = Storage::memory().unwrap();
        let sinks = hub_sinks();
        let publish = Publish {
            sinks: &sinks,
            source_id: "",
            source_name: None,
        };
        let s = snapshot();
        db.register(&s, "secret", Some(&publish)).unwrap();
        assert!(hub_updates(&db).is_empty());
    }
}
