use anyhow::{Context, Result, bail};
use chrono::{Duration, Utc};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use sessiontap_core::{
    config::SinkConfig,
    domain::{
        Activity, CurrentStatusReason, EventKind, InvocationId, InvocationSnapshot, Lifecycle,
        NormalizedEvent, PublicAgentView, PublicField, STATUS_REASON_MAX_BYTES,
        STATUS_REASON_MAX_CHARS, StatusReasonContext, changed_public_fields, derive_status,
        project_public,
    },
    protocol::{HUB_SCHEMA_VERSION, SourceEnvelope, SourceIdentity},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    sync::Mutex,
};

const MIGRATION: &str = r#"
CREATE TABLE IF NOT EXISTS schema_migrations (version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL);
INSERT OR IGNORE INTO schema_migrations(version,applied_at) VALUES (1,CURRENT_TIMESTAMP);
INSERT OR IGNORE INTO schema_migrations(version,applied_at) VALUES (2,CURRENT_TIMESTAMP);
INSERT OR IGNORE INTO schema_migrations(version,applied_at) VALUES (3,CURRENT_TIMESTAMP);
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
CREATE TABLE IF NOT EXISTS local_status_reasons (
 invocation_id TEXT PRIMARY KEY REFERENCES invocations(invocation_id) ON DELETE CASCADE,
 kind TEXT NOT NULL, reason_json TEXT NOT NULL CHECK(length(reason_json) <= 2048),
 updated_at TEXT NOT NULL
);
INSERT OR IGNORE INTO local_status_reasons(invocation_id,kind,reason_json,updated_at)
 SELECT invocation_id,kind,attention_json,updated_at FROM local_active_attention;
DELETE FROM local_active_attention;
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
        let view = project_public(&value, None);
        let changed = changed_public_fields(None, &view);
        enqueue_transition(&tx, publish, &view, &event_id, revision, &changed)?;
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
    ) -> Result<Option<AppliedUpdate>> {
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
    ) -> Result<Option<AppliedUpdate>> {
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
        clear_incompatible_reason: bool,
        f: impl FnOnce(&mut InvocationSnapshot),
        kind: EventKind,
        synthetic_label: &str,
        publish: Option<&Publish<'_>>,
    ) -> Result<Option<AppliedUpdate>> {
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
        let prior_reason = current_status_reason(&tx, id)?;
        let prior_view = project_public(&snapshot, prior_reason.as_ref());
        f(&mut snapshot);
        snapshot.revision = next_revision(&tx)?;
        snapshot.status = derive_status(snapshot.lifecycle, snapshot.activity);
        if clear_incompatible_reason
            && prior_reason.as_ref().is_some_and(|reason| {
                !matches!(reason.kind, EventKind::Completed | EventKind::Failed)
            })
        {
            clear_status_reason_row(&tx, id)?;
        }
        let reason = current_status_reason(&tx, id)?;
        let provisional = project_public(&snapshot, reason.as_ref());
        let changed = changed_public_fields(Some(&prior_view), &provisional);
        if !changed.is_empty() {
            snapshot.updated_at = Utc::now();
        }
        persist_snapshot(&tx, &snapshot, credential)?;
        let view = project_public(&snapshot, reason.as_ref());
        let changed = changed_public_fields(Some(&prior_view), &view);
        let event_id = synthetic_event_id(synthetic_label, id, snapshot.revision);
        let _ = kind;
        if !changed.is_empty() {
            enqueue_transition(&tx, publish, &view, &event_id, snapshot.revision, &changed)?;
        }
        tx.commit()?;
        Ok((!changed.is_empty()).then_some(AppliedUpdate {
            revision: snapshot.revision,
            delivery_id: event_id,
            view,
            changed,
        }))
    }

    pub fn apply_event(
        &self,
        event: &NormalizedEvent,
        publish: Option<&Publish<'_>>,
    ) -> Result<Option<InvocationSnapshot>> {
        self.apply_event_with_context(event, None, publish)?
            .map(|_| self.invocation(&event.invocation_id))
            .transpose()
    }

    pub fn apply_event_with_context(
        &self,
        event: &NormalizedEvent,
        status_reason: Option<&StatusReasonContext>,
        publish: Option<&Publish<'_>>,
    ) -> Result<Option<AppliedUpdate>> {
        if status_reason.is_some_and(|context| {
            context.summary.is_empty()
                || context.summary.len() > STATUS_REASON_MAX_BYTES
                || context.summary.chars().count() > STATUS_REASON_MAX_CHARS
                || context.summary.chars().any(char::is_control)
        }) {
            bail!("status reason context is not bounded normalized text");
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
        let prior_reason = current_status_reason(&tx, &event.invocation_id)?;
        let prior_public = project_public(&snapshot, prior_reason.as_ref());
        let stale_session = event.provider_session_id.as_ref().is_some_and(|id| {
            snapshot
                .provider_session
                .as_ref()
                .is_some_and(|current| current.id != *id)
                && event.kind != EventKind::ProviderSessionStarted
        });
        let stale_turn = event.turn_id.as_ref().is_some_and(|id| {
            snapshot
                .provider_metadata
                .as_ref()
                .and_then(|m| m.current_turn_id.as_ref())
                .is_some_and(|current| current != id)
                && event.kind != EventKind::NewTurn
        });
        let terminal_for_turn = snapshot.completed_generation == Some(snapshot.turn_generation);
        let suppressed_terminal_event = terminal_for_turn
            && matches!(
                event.kind,
                EventKind::Working
                    | EventKind::WaitingInput
                    | EventKind::WaitingApproval
                    | EventKind::Completed
                    | EventKind::Failed
            );
        let suppressed = stale_session || stale_turn || suppressed_terminal_event;
        if !suppressed {
            reduce(&mut snapshot, event);
        }
        let revision = next_revision(&tx)?;
        snapshot.revision = revision;
        snapshot.status = derive_status(snapshot.lifecycle, snapshot.activity);
        match if suppressed {
            &EventKind::Enrichment
        } else {
            &event.kind
        } {
            EventKind::WaitingApproval
            | EventKind::WaitingInput
            | EventKind::Completed
            | EventKind::Failed => {
                if let Some(context) = status_reason {
                    let current = CurrentStatusReason {
                        kind: event.kind.clone(),
                        context: context.clone(),
                    };
                    tx.execute("INSERT INTO local_status_reasons(invocation_id,kind,reason_json,updated_at) VALUES (?1,?2,?3,?4) ON CONFLICT(invocation_id) DO UPDATE SET kind=excluded.kind,reason_json=excluded.reason_json,updated_at=excluded.updated_at", params![event.invocation_id.to_string(), serde_json::to_string(&event.kind)?, serde_json::to_string(&current)?, Utc::now().to_rfc3339()])?;
                } else {
                    clear_status_reason_row(&tx, &event.invocation_id)?;
                }
            }
            EventKind::NewTurn
            | EventKind::Working
            | EventKind::Idle
            | EventKind::ProviderSessionStarted => {
                clear_status_reason_row(&tx, &event.invocation_id)?;
            }
            EventKind::SessionEnded => {
                let current = current_status_reason(&tx, &event.invocation_id)?;
                if current.as_ref().is_some_and(|reason| {
                    !matches!(reason.kind, EventKind::Completed | EventKind::Failed)
                }) {
                    clear_status_reason_row(&tx, &event.invocation_id)?;
                }
            }
            EventKind::ProviderSessionEnded | EventKind::Enrichment => {}
        }
        tx.execute(
            "INSERT INTO event_dedup(event_id,committed_at) VALUES (?1,?2)",
            params![event.event_id, Utc::now().to_rfc3339()],
        )?;
        tx.execute("INSERT INTO normalized_events(event_id,invocation_id,revision,received_at,event_json) VALUES (?1,?2,?3,?4,?5)", params![event.event_id, event.invocation_id.to_string(), revision, event.received_at.to_rfc3339(), serde_json::to_string(event)?])?;
        let current_reason = current_status_reason(&tx, &event.invocation_id)?;
        let provisional = project_public(&snapshot, current_reason.as_ref());
        let substantive = changed_public_fields(Some(&prior_public), &provisional);
        let materially_changed = !substantive.is_empty();
        if materially_changed {
            snapshot.updated_at = Utc::now();
        }
        persist_snapshot(&tx, &snapshot, &credential)?;
        let view = project_public(&snapshot, current_reason.as_ref());
        let changed = changed_public_fields(Some(&prior_public), &view);
        if materially_changed {
            enqueue_transition(&tx, publish, &view, &event.event_id, revision, &changed)?;
        }
        tx.commit()?;
        if !materially_changed {
            return Ok(None);
        }
        Ok(Some(AppliedUpdate {
            revision,
            delivery_id: event.event_id.clone(),
            view,
            changed,
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

    pub fn snapshot_with_reasons(
        &self,
    ) -> Result<(
        u64,
        Vec<InvocationSnapshot>,
        BTreeMap<InvocationId, CurrentStatusReason>,
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
            conn.prepare("SELECT invocation_id,reason_json FROM local_status_reasons")?;
        let reasons = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .map(|row| {
                let (id, raw) = row?;
                Ok((id.parse()?, serde_json::from_str(&raw)?))
            })
            .collect::<Result<BTreeMap<_, _>>>()?;
        Ok((revision, snapshots, reasons))
    }

    /// Projects internal snapshots and current reasons from one consistent read.
    pub fn public_snapshot(&self) -> Result<(u64, Vec<PublicAgentView>)> {
        let (revision, snapshots, reasons) = self.snapshot_with_reasons()?;
        let views = snapshots
            .iter()
            .map(|snapshot| project_public(snapshot, reasons.get(&snapshot.invocation_id)))
            .collect();
        Ok((revision, views))
    }

    /// Builds a canonical versioned source snapshot envelope at the current
    /// consistent revision. The mutex guarantees the snapshot and revision are
    /// captured atomically with respect to committed transitions.
    pub fn hub_source_snapshot(
        &self,
        source_id: &str,
        source_name: Option<&str>,
    ) -> Result<(u64, Vec<u8>)> {
        let (revision, views) = self.public_snapshot()?;
        let envelope = SourceEnvelope::Snapshot {
            schema_version: HUB_SCHEMA_VERSION,
            source: SourceIdentity {
                id: source_id.to_owned(),
                display_name: source_name.map(str::to_owned),
            },
            revision,
            views,
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
                let prior_reason = current_status_reason(&tx, &lost.invocation_id)?;
                let prior_view = project_public(&lost, prior_reason.as_ref());
                lost.lifecycle = Lifecycle::Lost;
                lost.status = derive_status(lost.lifecycle, lost.activity);
                lost.revision = next_revision(&tx)?;
                let credential: String = tx.query_row(
                    "SELECT credential FROM invocations WHERE invocation_id=?1",
                    [lost.invocation_id.to_string()],
                    |r| r.get(0),
                )?;
                if prior_reason.as_ref().is_some_and(|reason| {
                    !matches!(reason.kind, EventKind::Completed | EventKind::Failed)
                }) {
                    clear_status_reason_row(&tx, &lost.invocation_id)?;
                }
                let event_id =
                    synthetic_event_id("reconcile_lost", &lost.invocation_id, lost.revision);
                let reason = current_status_reason(&tx, &lost.invocation_id)?;
                let provisional = project_public(&lost, reason.as_ref());
                if !changed_public_fields(Some(&prior_view), &provisional).is_empty() {
                    lost.updated_at = Utc::now();
                }
                persist_snapshot(&tx, &lost, &credential)?;
                let view = project_public(&lost, reason.as_ref());
                let fields = changed_public_fields(Some(&prior_view), &view);
                if !fields.is_empty() {
                    enqueue_transition(&tx, publish, &view, &event_id, lost.revision, &fields)?;
                }
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
    pub revision: u64,
    pub delivery_id: String,
    pub view: PublicAgentView,
    pub changed: BTreeSet<PublicField>,
}

/// Stable source-scoped identity for transitions that have no provider event.
fn synthetic_event_id(label: &str, invocation_id: &InvocationId, revision: u64) -> String {
    format!("synthetic:{label}:{invocation_id}:{revision}")
}

fn current_status_reason(
    tx: &Transaction<'_>,
    id: &InvocationId,
) -> Result<Option<CurrentStatusReason>> {
    let raw: Option<String> = tx
        .query_row(
            "SELECT reason_json FROM local_status_reasons WHERE invocation_id=?1",
            [id.to_string()],
            |r| r.get(0),
        )
        .optional()?;
    raw.map(|raw| serde_json::from_str(&raw).map_err(Into::into))
        .transpose()
}

/// Enqueues one delivery per enabled sink in the same transaction as the
/// committed transition. Every sink receives the same canonical public
/// source envelope; field selection can only remove explicitly public view
/// fields for non-hub archival sinks.
fn enqueue_transition(
    tx: &Transaction<'_>,
    publish: Option<&Publish<'_>>,
    view: &PublicAgentView,
    delivery_id: &str,
    revision: u64,
    changed: &BTreeSet<PublicField>,
) -> Result<()> {
    let Some(publish) = publish else {
        return Ok(());
    };
    for (name, config) in publish.sinks.iter().filter(|(_, c)| c.enabled()) {
        if publish.source_id.is_empty() && config.is_hub() {
            continue;
        }
        let source_id = if publish.source_id.is_empty() {
            "local"
        } else {
            publish.source_id
        };
        let envelope = SourceEnvelope::Update {
            schema_version: HUB_SCHEMA_VERSION,
            source_id: source_id.to_owned(),
            delivery_id: delivery_id.to_owned(),
            revision,
            changed: changed.clone(),
            view: Box::new(view.clone()),
        };
        let payload = serde_json::to_vec(&envelope)?;
        if payload.len() <= config.max_payload_bytes() {
            tx.execute(
                "INSERT OR IGNORE INTO sink_outbox(sink_name,event_id,revision,payload,next_attempt_at) SELECT ?1,?2,?3,?4,?5 WHERE (SELECT COUNT(*) FROM sink_outbox WHERE sink_name=?1) < ?6",
                params![name, delivery_id, revision, payload, Utc::now().to_rfc3339(), MAX_OUTBOX_RECORDS_PER_SINK],
            )?;
        }
    }
    Ok(())
}

fn clear_status_reason_row(tx: &Transaction<'_>, id: &InvocationId) -> Result<()> {
    tx.execute(
        "DELETE FROM local_status_reasons WHERE invocation_id=?1",
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
        EventKind::Idle => snapshot.activity = Activity::Idle,
        EventKind::WaitingInput
            if snapshot.completed_generation != Some(snapshot.turn_generation) =>
        {
            snapshot.activity = Activity::WaitingInput;
        }
        EventKind::WaitingApproval
            if snapshot.completed_generation != Some(snapshot.turn_generation) =>
        {
            snapshot.activity = Activity::WaitingApproval;
        }
        EventKind::Completed | EventKind::Failed => {
            snapshot.activity = Activity::Stopped;
            snapshot.completed_generation = Some(snapshot.turn_generation);
        }
        EventKind::ProviderSessionStarted => {
            snapshot.activity = Activity::Idle;
        }
        EventKind::ProviderSessionEnded => {}
        EventKind::SessionEnded => snapshot.lifecycle = Lifecycle::Exited,
        EventKind::Enrichment
        | EventKind::Working
        | EventKind::WaitingInput
        | EventKind::WaitingApproval => {}
    }
    if let Some(id) = &event.provider_session_id {
        let prior = snapshot.provider_session.as_ref();
        let is_new = prior.is_none_or(|session| session.id != *id);
        snapshot.provider_session = Some(sessiontap_core::domain::ProviderSession {
            id: id.clone(),
            name: event.provider_session_name.clone().or_else(|| {
                snapshot
                    .provider_session
                    .as_ref()
                    .filter(|session| session.id == *id)
                    .and_then(|session| session.name.clone())
            }),
            generation: if is_new {
                prior.map_or(1, |session| session.generation.saturating_add(1))
            } else {
                prior.map_or(1, |session| session.generation)
            },
            start_reason: event.provider_session_start_reason.clone().or_else(|| {
                prior
                    .filter(|session| session.id == *id)
                    .and_then(|session| session.start_reason.clone())
            }),
        });
    }
    if let Some(metadata) = &event.provider_metadata {
        let current = snapshot.provider_metadata.get_or_insert_default();
        if metadata.model.is_some() {
            current.model.clone_from(&metadata.model);
        }
        if metadata.effort.is_some() {
            current.effort.clone_from(&metadata.effort);
        }
        if metadata.permission_mode.is_some() {
            current
                .permission_mode
                .clone_from(&metadata.permission_mode);
        }
        if metadata.current_turn_id.is_some() {
            current
                .current_turn_id
                .clone_from(&metadata.current_turn_id);
        }
    }
    if let Some(turn_id) = &event.turn_id {
        snapshot
            .provider_metadata
            .get_or_insert_default()
            .current_turn_id = Some(turn_id.clone());
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

#[cfg(all(test, any()))]
mod legacy_tests {
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
            provider_metadata: None,
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
            provider_session_start_reason: None,
            provider_metadata: None,
            usage: None,
            turn_id: None,
        }
    }
    #[test]
    fn provider_sessions_are_ordered_and_do_not_end_the_wrapper() {
        let mut state = snapshot();
        let mut first = event(&state, EventKind::ProviderSessionStarted, "start-a");
        first.provider_session_id = Some("a".into());
        first.provider_session_start_reason = Some("startup".into());
        reduce(&mut state, &first);
        assert_eq!(state.provider_session.as_ref().unwrap().generation, 1);

        let mut second = event(&state, EventKind::ProviderSessionStarted, "start-b");
        second.provider_session_id = Some("b".into());
        second.provider_session_start_reason = Some("clear".into());
        reduce(&mut state, &second);
        assert_eq!(state.provider_session.as_ref().unwrap().generation, 2);

        let mut ended = event(&state, EventKind::ProviderSessionEnded, "end-b");
        ended.provider_session_id = Some("b".into());
        reduce(&mut state, &ended);
        assert_eq!(state.lifecycle, Lifecycle::Alive);
        assert_eq!(state.activity, Activity::Idle);
    }

    #[test]
    fn stale_session_and_turn_events_cannot_regress_state() {
        let db = Storage::memory().unwrap();
        let state = snapshot();
        db.register(&state, "secret", None).unwrap();
        let mut start = event(&state, EventKind::ProviderSessionStarted, "start-b");
        start.provider_session_id = Some("b".into());
        start.turn_id = Some("turn-2".into());
        db.apply_event(&start, None).unwrap();

        let mut stale = event(&state, EventKind::WaitingApproval, "late-a");
        stale.provider_session_id = Some("a".into());
        stale.turn_id = Some("turn-1".into());
        stale.provider_metadata = Some(sessiontap_core::domain::ProviderMetadata {
            permission_mode: Some("auto".into()),
            ..Default::default()
        });
        let update = db.apply_event(&stale, None).unwrap().unwrap();
        assert_eq!(update.activity, Activity::Idle);
        assert_eq!(update.provider_session.as_ref().unwrap().id, "b");
        assert_ne!(
            update
                .provider_metadata
                .as_ref()
                .and_then(|m| m.permission_mode.as_deref()),
            Some("auto")
        );
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

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use sessiontap_core::domain::{Capabilities, ProcessMetadata, PublicReasonKind, PublicStatus};

    fn snapshot() -> InvocationSnapshot {
        let now = Utc::now();
        InvocationSnapshot {
            schema_version: sessiontap_core::SCHEMA_VERSION,
            revision: 0,
            invocation_id: InvocationId::new(),
            provider: "company-claude".into(),
            executable: "private-executable".into(),
            args: vec!["PRIVATE_ARGUMENT".into()],
            cwd: "/work/project".into(),
            process: ProcessMetadata {
                wrapper_pid: 42,
                ..Default::default()
            },
            created_at: now,
            updated_at: now,
            lifecycle: Lifecycle::Alive,
            activity: Activity::Idle,
            status: PublicStatus::Idle,
            provider_session: None,
            provider_metadata: None,
            usage: None,
            repository: None,
            multiplexer: None,
            capabilities: Capabilities::default(),
            turn_generation: 0,
            completed_generation: None,
        }
    }

    fn normalized_event(value: &InvocationSnapshot, kind: EventKind, id: &str) -> NormalizedEvent {
        NormalizedEvent {
            schema_version: 1,
            event_id: id.into(),
            invocation_id: value.invocation_id.clone(),
            provider_event_id: None,
            provider: value.provider.clone(),
            observed_at: Utc::now(),
            received_at: Utc::now(),
            source: "hook".into(),
            kind,
            provider_session_id: None,
            provider_session_name: None,
            provider_session_start_reason: None,
            provider_metadata: None,
            usage: None,
            turn_id: None,
        }
    }

    fn completed_reason(summary: &str) -> StatusReasonContext {
        StatusReasonContext {
            summary: summary.into(),
            source: sessiontap_core::domain::StatusReasonSource::AssistantMessage,
        }
    }

    #[test]
    fn public_projection_and_outbox_exclude_private_state() {
        let db = Storage::memory().unwrap();
        let sinks = BTreeMap::from([(
            "stdout".into(),
            SinkConfig::Stdout {
                enabled: true,
                fields: vec![],
            },
        )]);
        let publish = Publish {
            sinks: &sinks,
            source_id: "sandbox",
            source_name: None,
        };
        let value = snapshot();
        db.register(&value, "PRIVATE_CREDENTIAL", Some(&publish))
            .unwrap();
        let (_, views) = db.public_snapshot().unwrap();
        assert_eq!(views[0].provider, "company-claude");
        let payload = String::from_utf8(db.due_outbox(1).unwrap()[0].payload.clone()).unwrap();
        for private in [
            "PRIVATE_ARGUMENT",
            "PRIVATE_CREDENTIAL",
            "process",
            "multiplexer",
            "lifecycle",
            "activity",
        ] {
            assert!(!payload.contains(private));
        }
    }

    #[test]
    fn waiting_input_projects_bounded_public_reason() {
        let db = Storage::memory().unwrap();
        let value = snapshot();
        db.register(&value, "credential", None).unwrap();
        let event = NormalizedEvent {
            schema_version: 1,
            event_id: "wait".into(),
            invocation_id: value.invocation_id.clone(),
            provider_event_id: None,
            provider: value.provider.clone(),
            observed_at: Utc::now(),
            received_at: Utc::now(),
            source: "hook".into(),
            kind: EventKind::WaitingInput,
            provider_session_id: None,
            provider_session_name: None,
            provider_session_start_reason: None,
            provider_metadata: None,
            usage: None,
            turn_id: None,
        };
        let update = db
            .apply_event_with_context(
                &event,
                Some(&StatusReasonContext {
                    summary: "Choose an option".into(),
                    source: sessiontap_core::domain::StatusReasonSource::Question,
                }),
                None,
            )
            .unwrap()
            .unwrap();
        assert_eq!(update.view.status, PublicStatus::Blocked);
        assert_eq!(update.view.reason.unwrap().kind, PublicReasonKind::Input);
        assert!(update.changed.contains(&PublicField::Status));
        assert!(update.changed.contains(&PublicField::Reason));

        let replacement = db
            .apply_event_with_context(
                &normalized_event(&value, EventKind::WaitingInput, "wait-replacement"),
                Some(&StatusReasonContext {
                    summary: "Choose the newer option".into(),
                    source: sessiontap_core::domain::StatusReasonSource::Question,
                }),
                None,
            )
            .unwrap()
            .unwrap();
        assert_eq!(
            replacement.view.reason.unwrap().summary,
            "Choose the newer option"
        );
        assert_eq!(db.snapshot_with_reasons().unwrap().2.len(), 1);
    }

    #[test]
    fn repeated_internal_only_enrichment_is_not_published() {
        let db = Storage::memory().unwrap();
        let value = snapshot();
        db.register(&value, "credential", None).unwrap();
        let event = NormalizedEvent {
            schema_version: 1,
            event_id: "internal-only".into(),
            invocation_id: value.invocation_id,
            provider_event_id: None,
            provider: value.provider,
            observed_at: Utc::now(),
            received_at: Utc::now(),
            source: "hook".into(),
            kind: EventKind::Enrichment,
            provider_session_id: None,
            provider_session_name: None,
            provider_session_start_reason: None,
            provider_metadata: None,
            usage: None,
            turn_id: None,
        };
        assert!(
            db.apply_event_with_context(&event, None, None)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn stopped_outcomes_persist_clear_and_resist_late_work() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state.sqlite3");
        let value = snapshot();
        let db = Storage::open(&path).unwrap();
        db.register(&value, "credential", None).unwrap();
        db.apply_event(&normalized_event(&value, EventKind::NewTurn, "turn"), None)
            .unwrap();
        let completed = db
            .apply_event_with_context(
                &normalized_event(&value, EventKind::Completed, "completed"),
                Some(&completed_reason("All tests pass")),
                None,
            )
            .unwrap()
            .unwrap();
        assert_eq!(completed.view.status, PublicStatus::Stopped);
        assert_eq!(
            completed.view.reason.as_ref().unwrap().kind,
            PublicReasonKind::Completed
        );
        assert!(
            db.apply_event(
                &normalized_event(&value, EventKind::Working, "late-work"),
                None
            )
            .unwrap()
            .is_none()
        );
        drop(db);

        let db = Storage::open(&path).unwrap();
        let (_, views) = db.public_snapshot().unwrap();
        assert_eq!(views[0].status, PublicStatus::Stopped);
        assert_eq!(views[0].reason.as_ref().unwrap().summary, "All tests pass");
        let idle = db
            .apply_event_with_context(
                &normalized_event(&value, EventKind::Idle, "idle"),
                None,
                None,
            )
            .unwrap()
            .unwrap();
        assert_eq!(idle.view.status, PublicStatus::Idle);
        assert!(idle.view.reason.is_none());
    }

    #[test]
    fn provider_end_and_lifecycle_exit_preserve_outcomes_without_duplicates() {
        let db = Storage::memory().unwrap();
        let value = snapshot();
        db.register(&value, "credential", None).unwrap();
        db.apply_event(&normalized_event(&value, EventKind::NewTurn, "turn"), None)
            .unwrap();
        db.apply_event_with_context(
            &normalized_event(&value, EventKind::Failed, "failed"),
            Some(&StatusReasonContext {
                summary: "Rate limited".into(),
                source: sessiontap_core::domain::StatusReasonSource::FailureCategory,
            }),
            None,
        )
        .unwrap();
        assert!(
            db.apply_event(
                &normalized_event(&value, EventKind::ProviderSessionEnded, "provider-end"),
                None,
            )
            .unwrap()
            .is_none()
        );
        assert!(
            db.mark_exit(&value.invocation_id, "credential", Some(1), None, None)
                .unwrap()
                .is_none()
        );
        let (_, views) = db.public_snapshot().unwrap();
        assert_eq!(
            views[0].reason.as_ref().unwrap().kind,
            PublicReasonKind::Failed
        );
    }

    #[test]
    fn lifecycle_only_exit_clears_blocked_reason() {
        let db = Storage::memory().unwrap();
        let value = snapshot();
        db.register(&value, "credential", None).unwrap();
        db.apply_event_with_context(
            &normalized_event(&value, EventKind::WaitingInput, "waiting"),
            Some(&StatusReasonContext {
                summary: "Choose".into(),
                source: sessiontap_core::domain::StatusReasonSource::Question,
            }),
            None,
        )
        .unwrap();
        let exit = db
            .mark_exit(&value.invocation_id, "credential", Some(0), None, None)
            .unwrap()
            .unwrap();
        assert_eq!(exit.view.status, PublicStatus::Stopped);
        assert!(exit.view.reason.is_none());
        assert!(exit.changed.contains(&PublicField::Status));
        assert!(exit.changed.contains(&PublicField::Reason));
    }

    #[test]
    fn legacy_attention_rows_migrate_to_current_status_reasons() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("legacy.sqlite3");
        let mut value = snapshot();
        value.activity = Activity::WaitingApproval;
        value.status = PublicStatus::Blocked;
        let db = Storage::open(&path).unwrap();
        db.register(&value, "credential", None).unwrap();
        let legacy = CurrentStatusReason {
            kind: EventKind::WaitingApproval,
            context: StatusReasonContext {
                summary: "Approve legacy".into(),
                source: sessiontap_core::domain::StatusReasonSource::Description,
            },
        };
        {
            let conn = db.conn.lock().unwrap();
            conn.execute("DELETE FROM local_status_reasons", [])
                .unwrap();
            conn.execute(
                "INSERT INTO local_active_attention(invocation_id,kind,attention_json,updated_at) VALUES (?1,?2,?3,?4)",
                params![
                    value.invocation_id.to_string(),
                    serde_json::to_string(&EventKind::WaitingApproval).unwrap(),
                    serde_json::to_string(&legacy).unwrap(),
                    Utc::now().to_rfc3339()
                ],
            )
            .unwrap();
        }
        drop(db);
        let db = Storage::open(&path).unwrap();
        let (_, views) = db.public_snapshot().unwrap();
        assert_eq!(views[0].reason.as_ref().unwrap().summary, "Approve legacy");
    }

    #[test]
    fn selected_summaries_stay_out_of_normalized_event_history() {
        let db = Storage::memory().unwrap();
        let value = snapshot();
        db.register(&value, "credential", None).unwrap();
        db.apply_event_with_context(
            &normalized_event(&value, EventKind::Completed, "private-event"),
            Some(&completed_reason("SELECTED_CURRENT_ONLY")),
            None,
        )
        .unwrap();
        let conn = db.conn.lock().unwrap();
        let raw: String = conn
            .query_row(
                "SELECT event_json FROM normalized_events WHERE event_id='private-event'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!raw.contains("SELECTED_CURRENT_ONLY"));
        let current: String = conn
            .query_row(
                "SELECT reason_json FROM local_status_reasons WHERE invocation_id=?1",
                [value.invocation_id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert!(current.contains("SELECTED_CURRENT_ONLY"));
    }

    #[test]
    fn stopped_reason_is_delivered_once_without_internal_or_raw_fields() {
        let db = Storage::memory().unwrap();
        let value = snapshot();
        let sinks = BTreeMap::from([(
            "observer".into(),
            SinkConfig::Stdout {
                enabled: true,
                fields: vec![],
            },
        )]);
        let publish = Publish {
            sinks: &sinks,
            source_id: "sandbox",
            source_name: None,
        };
        db.register(&value, "credential", Some(&publish)).unwrap();
        db.apply_event(
            &normalized_event(&value, EventKind::NewTurn, "sink-turn"),
            Some(&publish),
        )
        .unwrap();
        db.apply_event_with_context(
            &normalized_event(&value, EventKind::Completed, "sink-completed"),
            Some(&completed_reason("Bounded final response")),
            Some(&publish),
        )
        .unwrap();
        let before_exit = db.due_outbox(10).unwrap();
        assert_eq!(before_exit.len(), 3);
        let completion = before_exit
            .iter()
            .find(|record| record.event_id == "sink-completed")
            .unwrap();
        let payload = String::from_utf8(completion.payload.clone()).unwrap();
        assert!(payload.contains("Bounded final response"));
        assert!(payload.contains("\"kind\":\"completed\""));
        for private in [
            "last_assistant_message",
            "normalized_events",
            "lifecycle",
            "activity",
            "process",
            "multiplexer",
        ] {
            assert!(!payload.contains(private));
        }
        assert!(
            db.mark_exit(
                &value.invocation_id,
                "credential",
                Some(0),
                None,
                Some(&publish),
            )
            .unwrap()
            .is_none()
        );
        assert_eq!(db.due_outbox(10).unwrap().len(), 3);
    }

    #[test]
    fn lost_reconciliation_does_not_republish_an_unchanged_stopped_view() {
        let db = Storage::memory().unwrap();
        let mut value = snapshot();
        value.process.child_pid = Some(424_242);
        let sinks = BTreeMap::from([(
            "observer".into(),
            SinkConfig::Stdout {
                enabled: true,
                fields: vec![],
            },
        )]);
        let publish = Publish {
            sinks: &sinks,
            source_id: "sandbox",
            source_name: None,
        };
        db.register(&value, "credential", Some(&publish)).unwrap();
        db.apply_event(
            &normalized_event(&value, EventKind::NewTurn, "lost-turn"),
            Some(&publish),
        )
        .unwrap();
        db.apply_event_with_context(
            &normalized_event(&value, EventKind::Completed, "lost-completed"),
            Some(&completed_reason("Done before process loss")),
            Some(&publish),
        )
        .unwrap();
        assert_eq!(db.due_outbox(10).unwrap().len(), 3);
        assert_eq!(db.reconcile(|_, _| false, 7, Some(&publish)).unwrap(), 1);
        assert_eq!(db.due_outbox(10).unwrap().len(), 3);
        let (_, views) = db.public_snapshot().unwrap();
        assert_eq!(
            views[0].reason.as_ref().unwrap().summary,
            "Done before process loss"
        );
    }
}
