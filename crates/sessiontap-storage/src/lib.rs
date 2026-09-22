use anyhow::{Context, Result, bail};
use chrono::{DateTime, Duration, Utc};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use sessiontap_core::{
    config::SinkConfig,
    domain::{
        Activity, ActivityConfirmation, CurrentStatusReason, InvocationId, InvocationSnapshot,
        Lifecycle, NormalizedEvent, PublicAgentView, PublicField, StatusReasonContext,
        changed_public_fields, project_public,
    },
    protocol::{HUB_SCHEMA_VERSION, SourceEnvelope, SourceIdentity},
    reducer::{
        self, Prior, ReasonEffect, Transition, expire_stale_working, finalize, is_stale_working,
        local_mutation, mark_lost, validate_event,
    },
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
pub use sessiontap_core::reducer::STALE_WORKING_MINUTES;

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
            "lifecycle_exit",
            publish,
        )
    }

    fn mutate_authenticated(
        &self,
        id: &InvocationId,
        credential: &str,
        clear_incompatible_reason: bool,
        f: impl FnOnce(&mut InvocationSnapshot),
        synthetic_label: &'static str,
        publish: Option<&Publish<'_>>,
    ) -> Result<Option<AppliedUpdate>> {
        Ok(self
            .transition(
                id,
                Some(credential),
                Delivery::Synthetic(synthetic_label),
                publish,
                Utc::now(),
                |prior| Ok(Some(local_mutation(prior, clear_incompatible_reason, f))),
            )?
            .and_then(|committed| committed.update))
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
        validate_event(event, status_reason)?;
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
        let committed = apply_transition(
            &tx,
            &event.invocation_id,
            None,
            Delivery::Event(&event.event_id),
            publish,
            Utc::now(),
            |prior| reducer::apply_event(prior, event, status_reason).map(Some),
        )?
        .expect("event transitions always produce a snapshot");
        tx.execute(
            "INSERT INTO event_dedup(event_id,committed_at) VALUES (?1,?2)",
            params![event.event_id, Utc::now().to_rfc3339()],
        )?;
        tx.execute("INSERT INTO normalized_events(event_id,invocation_id,revision,received_at,event_json) VALUES (?1,?2,?3,?4,?5)", params![event.event_id, event.invocation_id.to_string(), committed.revision, event.received_at.to_rfc3339(), serde_json::to_string(event)?])?;
        tx.commit()?;
        Ok(committed.update)
    }

    /// Runs one state transition for `id` in its own transaction; see
    /// [`apply_transition`].
    fn transition(
        &self,
        id: &InvocationId,
        expected_credential: Option<&str>,
        delivery: Delivery<'_>,
        publish: Option<&Publish<'_>>,
        now: DateTime<Utc>,
        f: impl FnOnce(Prior<'_>) -> Result<Option<Transition>>,
    ) -> Result<Option<Committed>> {
        let mut conn = self.conn.lock().expect("storage mutex poisoned");
        let tx = conn.transaction()?;
        let committed = apply_transition(&tx, id, expected_credential, delivery, publish, now, f)?;
        if committed.is_some() {
            tx.commit()?;
        }
        Ok(committed)
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
                let mut snapshot = decode_snapshot(&raw)?;
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
                let mut snapshot = decode_snapshot(&raw)?;
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
        let mut snapshot = decode_snapshot(&raw)?;
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
            let process_is_alive = snapshot
                .process
                .child_pid
                .is_some_and(|pid| is_alive(pid, snapshot.process.start_identity.as_deref()));
            if matches!(snapshot.lifecycle, Lifecycle::Alive | Lifecycle::Starting)
                && process_is_alive
                && matches!(
                    snapshot.activity,
                    Activity::Working | Activity::WaitingInput | Activity::WaitingApproval
                )
                && snapshot.activity_confirmation != ActivityConfirmation::RestoredUnconfirmed
            {
                let mut conn = self.conn.lock().expect("storage mutex poisoned");
                let tx = conn.transaction()?;
                let mut restored = snapshot;
                restored.activity_confirmation = ActivityConfirmation::RestoredUnconfirmed;
                restored.revision = next_revision(&tx)?;
                let credential: String = tx.query_row(
                    "SELECT credential FROM invocations WHERE invocation_id=?1",
                    [restored.invocation_id.to_string()],
                    |r| r.get(0),
                )?;
                persist_snapshot(&tx, &restored, &credential)?;
                tx.commit()?;
                changed += 1;
            } else if matches!(snapshot.lifecycle, Lifecycle::Alive | Lifecycle::Starting)
                && !process_is_alive
            {
                let lost = self.transition(
                    &snapshot.invocation_id,
                    None,
                    Delivery::Synthetic("reconcile_lost"),
                    publish,
                    Utc::now(),
                    |prior| {
                        let current = prior.snapshot;
                        let still_alive = current.process.child_pid.is_some_and(|pid| {
                            is_alive(pid, current.process.start_identity.as_deref())
                        });
                        Ok(
                            (matches!(current.lifecycle, Lifecycle::Alive | Lifecycle::Starting)
                                && !still_alive)
                                .then(|| mark_lost(prior)),
                        )
                    },
                )?;
                if lost.is_some() {
                    changed += 1;
                }
            }
        }
        changed += self.expire_stale_working_at(Utc::now(), publish)?.len();
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

    pub fn expire_stale_working_at(
        &self,
        now: chrono::DateTime<Utc>,
        publish: Option<&Publish<'_>>,
    ) -> Result<Vec<AppliedUpdate>> {
        let (_, snapshots) = self.snapshot()?;
        let mut updates = Vec::new();
        for snapshot in snapshots {
            if !is_stale_working(&snapshot, now) {
                continue;
            }
            if let Some(update) = self
                .transition(
                    &snapshot.invocation_id,
                    None,
                    Delivery::Synthetic("stale_working"),
                    publish,
                    now,
                    |prior| Ok(expire_stale_working(prior, now)),
                )?
                .and_then(|committed| committed.update)
            {
                updates.push(update);
            }
        }
        Ok(updates)
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

/// Outbox identity of a committed transition.
#[derive(Clone, Copy)]
enum Delivery<'a> {
    /// The provider event id, shared with dedup and event history.
    Event(&'a str),
    /// A stable id derived from the label, invocation, and assigned revision.
    Synthetic(&'static str),
}

struct Committed {
    revision: u64,
    /// Present only when the public view changed.
    update: Option<AppliedUpdate>,
}

/// Loads the committed row for `id`, lets `f` decide the transition, and
/// persists it inside `tx`: reason row, revision, `updated_at`, snapshot, and
/// outbox entries when the public view changed. When `f` returns `None`
/// nothing is written and no revision is consumed. The caller commits.
fn apply_transition(
    tx: &Transaction<'_>,
    id: &InvocationId,
    expected_credential: Option<&str>,
    delivery: Delivery<'_>,
    publish: Option<&Publish<'_>>,
    now: DateTime<Utc>,
    f: impl FnOnce(Prior<'_>) -> Result<Option<Transition>>,
) -> Result<Option<Committed>> {
    let (credential, raw, generation, completed): (String, String, u64, Option<u64>) = tx.query_row(
        "SELECT credential,snapshot_json,turn_generation,completed_generation FROM invocations WHERE invocation_id=?1", [id.to_string()],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    ).context("unknown invocation")?;
    if expected_credential
        .is_some_and(|expected| !constant_time_eq(credential.as_bytes(), expected.as_bytes()))
    {
        bail!("invalid invocation credential");
    }
    let mut prior = decode_snapshot(&raw)?;
    prior.turn_generation = generation;
    prior.completed_generation = completed;
    let prior_reason = current_status_reason(tx, id)?;
    let prior_view = project_public(&prior, prior_reason.as_ref());
    let Some(Transition {
        mut snapshot,
        reason,
        ..
    }) = f(Prior {
        snapshot: &prior,
        reason: prior_reason.as_ref(),
    })?
    else {
        return Ok(None);
    };
    match reason {
        ReasonEffect::Keep => {}
        ReasonEffect::Clear => clear_status_reason_row(tx, id)?,
        ReasonEffect::Set(current) => {
            tx.execute("INSERT INTO local_status_reasons(invocation_id,kind,reason_json,updated_at) VALUES (?1,?2,?3,?4) ON CONFLICT(invocation_id) DO UPDATE SET kind=excluded.kind,reason_json=excluded.reason_json,updated_at=excluded.updated_at", params![id.to_string(), serde_json::to_string(&current.kind)?, serde_json::to_string(&current)?, Utc::now().to_rfc3339()])?;
        }
    }
    let revision = next_revision(tx)?;
    snapshot.revision = revision;
    let current_reason = current_status_reason(tx, id)?;
    let (view, changed) = finalize(&prior_view, &mut snapshot, current_reason.as_ref(), now);
    persist_snapshot(tx, &snapshot, &credential)?;
    let update = if changed.is_empty() {
        None
    } else {
        let delivery_id = match delivery {
            Delivery::Event(event_id) => event_id.to_owned(),
            Delivery::Synthetic(label) => synthetic_event_id(label, id, revision),
        };
        enqueue_transition(tx, publish, &view, &delivery_id, revision, &changed)?;
        Some(AppliedUpdate {
            revision,
            delivery_id,
            view,
            changed,
        })
    };
    Ok(Some(Committed { revision, update }))
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

fn decode_snapshot(raw: &str) -> Result<InvocationSnapshot> {
    serde_json::from_str(raw)
        .context("incompatible retained invocation state; internal alpha schemas change in place")
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
    use chrono::Utc;
    use sessiontap_core::domain::{
        Capabilities, EventEvidence, EventKind, EvidenceChannel, ProcessMetadata, PublicReasonKind,
        PublicStatus, ToolActivityPhase,
    };

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
            state_started_at: now,
            last_state_asserted_at: Some(now),
            activity_confirmation: ActivityConfirmation::Live,
            last_evidence: None,
            source_ordering: vec![],
            current_tool_activity: None,
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
            evidence: EventEvidence::managed_hook(1),
            kind,
            provider_session_id: None,
            provider_session_name: None,
            provider_session_start_reason: None,
            provider_metadata: None,
            usage: None,
            turn_id: None,
            tool_activity: None,
        }
    }

    fn completed_reason(summary: &str) -> StatusReasonContext {
        StatusReasonContext {
            summary: summary.into(),
            source: sessiontap_core::domain::StatusReasonSource::AssistantMessage,
        }
    }

    #[test]
    fn provider_session_change_clears_prior_usage() {
        let db = Storage::memory().unwrap();
        let value = snapshot();
        db.register(&value, "secret", None).unwrap();
        let mut first = normalized_event(&value, EventKind::ProviderSessionStarted, "session-a");
        first.provider_session_id = Some("a".into());
        first.usage = Some(sessiontap_core::domain::Usage {
            input_tokens: Some(10),
            output_tokens: Some(2),
            context_tokens: Some(5),
            context_window_percent: Some(3),
        });
        db.apply_event(&first, None).unwrap();
        assert!(db.invocation(&value.invocation_id).unwrap().usage.is_some());

        let mut second = normalized_event(&value, EventKind::ProviderSessionStarted, "session-b");
        second.provider_session_id = Some("b".into());
        db.apply_event(&second, None).unwrap();
        assert!(db.invocation(&value.invocation_id).unwrap().usage.is_none());
    }

    #[test]
    fn provider_artifact_usage_replaces_atomically_and_unchanged_is_suppressed() {
        let db = Storage::memory().unwrap();
        let value = snapshot();
        db.register(&value, "secret", None).unwrap();
        let mut artifact = normalized_event(&value, EventKind::Enrichment, "usage-1");
        artifact.evidence = EventEvidence::local(EvidenceChannel::ProviderArtifact);
        artifact.usage = Some(sessiontap_core::domain::Usage {
            input_tokens: Some(100),
            output_tokens: Some(20),
            context_tokens: None,
            context_window_percent: None,
        });
        assert!(
            db.apply_event_with_context(&artifact, None, None)
                .unwrap()
                .is_some()
        );
        let mut unchanged = artifact.clone();
        unchanged.event_id = "usage-2".into();
        assert!(
            db.apply_event_with_context(&unchanged, None, None)
                .unwrap()
                .is_none()
        );
        let stored = db.invocation(&value.invocation_id).unwrap().usage.unwrap();
        assert_eq!(stored.input_tokens, Some(100));
        assert_eq!(stored.context_tokens, None);
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
            evidence: EventEvidence::managed_hook(1),
            kind: EventKind::WaitingInput,
            provider_session_id: None,
            provider_session_name: None,
            provider_session_start_reason: None,
            provider_metadata: None,
            usage: None,
            turn_id: None,
            tool_activity: None,
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
            evidence: EventEvidence::managed_hook(1),
            kind: EventKind::Enrichment,
            provider_session_id: None,
            provider_session_name: None,
            provider_session_start_reason: None,
            provider_metadata: None,
            usage: None,
            turn_id: None,
            tool_activity: None,
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
    fn interrupted_turn_is_reasonless_terminal_until_a_new_turn() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state.sqlite3");
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
        let db = Storage::open(&path).unwrap();
        db.register(&value, "credential", Some(&publish)).unwrap();
        db.apply_event(
            &normalized_event(&value, EventKind::NewTurn, "turn"),
            Some(&publish),
        )
        .unwrap();
        db.apply_event_with_context(
            &normalized_event(&value, EventKind::WaitingInput, "waiting"),
            Some(&StatusReasonContext {
                summary: "PRIVATE QUESTION".into(),
                source: sessiontap_core::domain::StatusReasonSource::Question,
            }),
            Some(&publish),
        )
        .unwrap();
        let interrupted = db
            .apply_event_with_context(
                &normalized_event(&value, EventKind::Interrupted, "interrupted"),
                None,
                Some(&publish),
            )
            .unwrap()
            .unwrap();
        assert_eq!(interrupted.view.status, PublicStatus::Stopped);
        assert!(interrupted.view.reason.is_none());
        assert_eq!(sessiontap_core::SCHEMA_VERSION, 1);
        assert_eq!(sessiontap_core::protocol::HUB_SCHEMA_VERSION, 1);

        for (kind, event_id) in [
            (EventKind::Working, "late-working"),
            (EventKind::WaitingApproval, "late-approval"),
            (EventKind::Completed, "duplicate-completed"),
            (EventKind::Failed, "duplicate-failed"),
            (EventKind::Interrupted, "duplicate-interrupted"),
        ] {
            assert!(
                db.apply_event(&normalized_event(&value, kind, event_id), Some(&publish))
                    .unwrap()
                    .is_none()
            );
        }
        drop(db);

        let db = Storage::open(&path).unwrap();
        let (_, views) = db.public_snapshot().unwrap();
        assert_eq!(views[0].status, PublicStatus::Stopped);
        assert!(views[0].reason.is_none());
        let persisted: String = db
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT event_json FROM normalized_events WHERE event_id='interrupted'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(persisted.contains("\"kind\":\"interrupted\""));
        assert!(!persisted.contains("PRIVATE QUESTION"));

        let next = db
            .apply_event(
                &normalized_event(&value, EventKind::NewTurn, "next-turn"),
                Some(&publish),
            )
            .unwrap()
            .unwrap();
        assert_eq!(next.activity, Activity::Working);
        let (_, views) = db.public_snapshot().unwrap();
        assert_eq!(views[0].status, PublicStatus::Running);
        assert!(views[0].reason.is_none());
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

    #[test]
    fn evidence_authority_ordering_and_assertion_timing_are_enforced() {
        use sessiontap_core::domain::{EvidenceTrust, ProviderMetadata};
        let db = Storage::memory().unwrap();
        let value = snapshot();
        db.register(&value, "credential", None).unwrap();
        let first_at = value.created_at + chrono::Duration::seconds(1);

        let mut process = normalized_event(&value, EventKind::Working, "process-cannot-work");
        process.evidence = EventEvidence::local(EvidenceChannel::ProcessObservation);
        process.received_at = first_at;
        process.provider_metadata = Some(ProviderMetadata {
            model: Some("forged-model".into()),
            ..Default::default()
        });
        db.apply_event(&process, None).unwrap();
        let unchanged = db.invocation(&value.invocation_id).unwrap();
        assert_eq!(unchanged.activity, Activity::Idle);
        assert!(unchanged.provider_metadata.is_none());

        let mut working = normalized_event(&value, EventKind::Working, "working-seq-1");
        working.received_at = first_at;
        working.evidence = EventEvidence {
            channel: EvidenceChannel::SideChannel,
            trust: EvidenceTrust::LocalObservation,
            collector_revision: Some(2),
            collector_instance_id: Some("collector-a".into()),
            source_sequence: Some(1),
        };
        db.apply_event(&working, None).unwrap();
        let started = db.invocation(&value.invocation_id).unwrap();
        assert_eq!(started.activity, Activity::Working);
        assert_eq!(started.state_started_at, first_at);
        assert_eq!(started.last_state_asserted_at, Some(first_at));
        assert_eq!(started.activity_confirmation, ActivityConfirmation::Live);

        let second_at = first_at + chrono::Duration::seconds(10);
        let mut repeated = working.clone();
        repeated.event_id = "working-seq-2".into();
        repeated.received_at = second_at;
        repeated.evidence.source_sequence = Some(2);
        db.apply_event(&repeated, None).unwrap();
        let refreshed = db.invocation(&value.invocation_id).unwrap();
        assert_eq!(refreshed.state_started_at, first_at);
        assert_eq!(refreshed.last_state_asserted_at, Some(second_at));

        let mut interleaved = normalized_event(&value, EventKind::Enrichment, "interleaved");
        interleaved.evidence = EventEvidence::local(EvidenceChannel::ProviderArtifact);
        db.apply_event(&interleaved, None).unwrap();

        let mut late = repeated.clone();
        late.event_id = "late-idle-seq-1".into();
        late.kind = EventKind::Idle;
        late.received_at = second_at + chrono::Duration::seconds(10);
        late.evidence.source_sequence = Some(1);
        db.apply_event(&late, None).unwrap();
        let ordered = db.invocation(&value.invocation_id).unwrap();
        assert_eq!(ordered.activity, Activity::Working);
        assert_eq!(ordered.last_state_asserted_at, Some(second_at));

        let mut artifact = normalized_event(&value, EventKind::SessionEnded, "artifact");
        artifact.evidence = EventEvidence::local(EvidenceChannel::ProviderArtifact);
        artifact.provider_metadata = Some(ProviderMetadata {
            model: Some("artifact-model".into()),
            ..Default::default()
        });
        db.apply_event(&artifact, None).unwrap();
        let enriched = db.invocation(&value.invocation_id).unwrap();
        assert_eq!(enriched.lifecycle, Lifecycle::Alive);
        assert_eq!(
            enriched
                .provider_metadata
                .as_ref()
                .and_then(|metadata| metadata.model.as_deref()),
            Some("artifact-model")
        );
        assert_eq!(enriched.last_state_asserted_at, Some(second_at));
    }

    #[test]
    fn tool_activity_matches_identity_and_private_progress_is_not_published() {
        use sessiontap_core::domain::{ToolActivityPhase, ToolActivityUpdate};
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

        let tool_update = |id: &str, event_id: &str, phase: ToolActivityPhase| {
            let mut event = normalized_event(&value, EventKind::Working, event_id);
            event.tool_activity = Some(ToolActivityUpdate {
                phase,
                label: "shell".into(),
                correlation_id: Some(id.into()),
                detail: Some("Run tests".into()),
            });
            event
        };
        let start = tool_update("a", "start-a", ToolActivityPhase::Start);
        let started_at = start.received_at;
        db.apply_event(&start, Some(&publish)).unwrap();
        let public_count = db.due_outbox(20).unwrap().len();
        let mut progress = tool_update("a", "progress-a", ToolActivityPhase::Progress);
        progress.received_at = started_at + chrono::Duration::seconds(5);
        db.apply_event(&progress, Some(&publish)).unwrap();
        assert_eq!(db.due_outbox(20).unwrap().len(), public_count);
        let progressed = db
            .invocation(&value.invocation_id)
            .unwrap()
            .current_tool_activity
            .unwrap();
        assert_eq!(progressed.started_at, started_at);
        assert_eq!(progressed.last_observed_at, progress.received_at);

        db.apply_event(&tool_update("b", "start-b", ToolActivityPhase::Start), None)
            .unwrap();
        let mut attention = normalized_event(&value, EventKind::WaitingApproval, "attention-b");
        attention.tool_activity = Some(ToolActivityUpdate {
            phase: ToolActivityPhase::Attention,
            label: "shell".into(),
            correlation_id: None,
            detail: Some("Approve tests".into()),
        });
        db.apply_event(&attention, None).unwrap();
        assert_eq!(
            db.invocation(&value.invocation_id).unwrap().activity,
            Activity::WaitingApproval
        );
        db.apply_event(
            &tool_update("a", "finish-a", ToolActivityPhase::Finish),
            None,
        )
        .unwrap();
        assert_eq!(
            db.invocation(&value.invocation_id)
                .unwrap()
                .current_tool_activity
                .unwrap()
                .correlation_id
                .as_deref(),
            Some("b")
        );
        db.apply_event(
            &tool_update("b", "finish-b", ToolActivityPhase::Failure),
            None,
        )
        .unwrap();
        assert!(
            db.invocation(&value.invocation_id)
                .unwrap()
                .current_tool_activity
                .is_none()
        );

        let lost_db = Storage::memory().unwrap();
        let mut lost_value = snapshot();
        lost_value.process.child_pid = Some(424_242);
        lost_db.register(&lost_value, "credential", None).unwrap();
        let mut lost_start = normalized_event(&lost_value, EventKind::Working, "lost-tool");
        lost_start.tool_activity = Some(ToolActivityUpdate {
            phase: ToolActivityPhase::Start,
            label: "shell".into(),
            correlation_id: Some("lost".into()),
            detail: None,
        });
        lost_db.apply_event(&lost_start, None).unwrap();
        lost_db.reconcile(|_, _| false, 7, None).unwrap();
        let lost = lost_db.invocation(&lost_value.invocation_id).unwrap();
        assert_eq!(lost.lifecycle, Lifecycle::Lost);
        assert!(lost.current_tool_activity.is_none());
        assert_eq!(
            lost.last_evidence.as_ref().unwrap().channel,
            EvidenceChannel::ProcessObservation
        );
    }

    #[test]
    fn tool_activity_clears_at_state_session_and_lifecycle_boundaries() {
        use sessiontap_core::domain::{ToolActivityPhase, ToolActivityUpdate};
        for (index, boundary) in [
            EventKind::NewTurn,
            EventKind::Idle,
            EventKind::Completed,
            EventKind::Failed,
            EventKind::Interrupted,
            EventKind::ProviderSessionStarted,
            EventKind::ProviderSessionEnded,
            EventKind::SessionEnded,
        ]
        .into_iter()
        .enumerate()
        {
            let db = Storage::memory().unwrap();
            let value = snapshot();
            db.register(&value, "credential", None).unwrap();
            let mut start = normalized_event(&value, EventKind::Working, &format!("start-{index}"));
            start.tool_activity = Some(ToolActivityUpdate {
                phase: ToolActivityPhase::Start,
                label: "read_file".into(),
                correlation_id: Some(format!("tool-{index}")),
                detail: None,
            });
            db.apply_event(&start, None).unwrap();
            assert!(
                db.invocation(&value.invocation_id)
                    .unwrap()
                    .current_tool_activity
                    .is_some()
            );
            let mut event = normalized_event(&value, boundary, &format!("boundary-{index}"));
            if event.kind == EventKind::ProviderSessionStarted {
                event.provider_session_id = Some("new-session".into());
            }
            db.apply_event(&event, None).unwrap();
            assert!(
                db.invocation(&value.invocation_id)
                    .unwrap()
                    .current_tool_activity
                    .is_none(),
                "boundary {:?} retained tool activity",
                event.kind
            );
        }

        let db = Storage::memory().unwrap();
        let mut value = snapshot();
        value.process.child_pid = Some(42);
        db.register(&value, "credential", None).unwrap();
        let mut start = normalized_event(&value, EventKind::Working, "lifecycle-tool");
        start.tool_activity = Some(ToolActivityUpdate {
            phase: ToolActivityPhase::Start,
            label: "shell".into(),
            correlation_id: Some("lifecycle".into()),
            detail: None,
        });
        db.apply_event(&start, None).unwrap();
        db.mark_exit(&value.invocation_id, "credential", Some(0), None, None)
            .unwrap();
        assert!(
            db.invocation(&value.invocation_id)
                .unwrap()
                .current_tool_activity
                .is_none()
        );
    }

    #[test]
    fn stale_working_expires_but_silent_waiting_survives_and_restores_unconfirmed() {
        let db = Storage::memory().unwrap();
        let mut value = snapshot();
        value.process.child_pid = Some(42);
        db.register(&value, "credential", None).unwrap();
        let asserted_at = value.created_at + chrono::Duration::seconds(1);
        let mut working = normalized_event(&value, EventKind::Working, "working");
        working.received_at = asserted_at;
        working.tool_activity = Some(sessiontap_core::domain::ToolActivityUpdate {
            phase: ToolActivityPhase::Start,
            label: "shell".into(),
            correlation_id: Some("long-tool".into()),
            detail: None,
        });
        db.apply_event(&working, None).unwrap();
        assert!(
            db.invocation(&value.invocation_id)
                .unwrap()
                .current_tool_activity
                .is_some()
        );
        assert_eq!(
            db.expire_stale_working_at(
                asserted_at + chrono::Duration::minutes(STALE_WORKING_MINUTES),
                None,
            )
            .unwrap()
            .len(),
            1
        );
        let stale = db.invocation(&value.invocation_id).unwrap();
        assert_eq!(stale.lifecycle, Lifecycle::Alive);
        assert_eq!(stale.activity, Activity::Unknown);
        assert_eq!(stale.status, PublicStatus::Idle);
        assert!(stale.current_tool_activity.is_none());
        assert_ne!(stale.completed_generation, Some(stale.turn_generation));

        let mut waiting = normalized_event(&value, EventKind::WaitingInput, "waiting");
        waiting.received_at = asserted_at + chrono::Duration::hours(1);
        db.apply_event(&waiting, None).unwrap();
        assert!(
            db.expire_stale_working_at(waiting.received_at + chrono::Duration::hours(8), None,)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            db.invocation(&value.invocation_id).unwrap().activity,
            Activity::WaitingInput
        );

        db.reconcile(|pid, _| pid == 42, 7, None).unwrap();
        assert_eq!(
            db.invocation(&value.invocation_id)
                .unwrap()
                .activity_confirmation,
            ActivityConfirmation::RestoredUnconfirmed
        );
        let mut live = normalized_event(&value, EventKind::WaitingInput, "waiting-live");
        live.received_at = waiting.received_at + chrono::Duration::hours(9);
        db.apply_event(&live, None).unwrap();
        assert_eq!(
            db.invocation(&value.invocation_id)
                .unwrap()
                .activity_confirmation,
            ActivityConfirmation::Live
        );
    }
    fn hub_sinks(max_payload_bytes: usize) -> BTreeMap<String, SinkConfig> {
        BTreeMap::from([(
            "hub".into(),
            SinkConfig::Hub {
                enabled: true,
                url: "http://127.0.0.1:8931/ingest".into(),
                token_env: None,
                token_file: None,
                timeout_ms: 100,
                max_payload_bytes,
                trusted_addresses: vec![],
            },
        )])
    }

    fn hub_publish(sinks: &BTreeMap<String, SinkConfig>) -> Publish<'_> {
        Publish {
            sinks,
            source_id: "host",
            source_name: Some("Host"),
        }
    }

    fn hub_updates(db: &Storage) -> Vec<SourceEnvelope> {
        let conn = db.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT payload FROM sink_outbox WHERE sink_name='hub' ORDER BY revision")
            .unwrap();
        stmt.query_map([], |r| r.get::<_, Vec<u8>>(0))
            .unwrap()
            .map(|raw| serde_json::from_slice(&raw.unwrap()).unwrap())
            .collect()
    }

    fn update_parts(
        envelope: &SourceEnvelope,
    ) -> (&str, &str, u64, &BTreeSet<PublicField>, &PublicAgentView) {
        match envelope {
            SourceEnvelope::Update {
                source_id,
                delivery_id,
                revision,
                changed,
                view,
                ..
            } => (source_id, delivery_id, *revision, changed, view),
            SourceEnvelope::Snapshot { .. } => panic!("expected update"),
        }
    }

    fn question(summary: &str) -> StatusReasonContext {
        StatusReasonContext {
            summary: summary.into(),
            source: sessiontap_core::domain::StatusReasonSource::Question,
        }
    }

    #[test]
    fn transition_that_declines_writes_nothing_and_keeps_the_revision() {
        let db = Storage::memory().unwrap();
        let value = snapshot();
        db.register(&value, "credential", None).unwrap();
        let before = db.invocation(&value.invocation_id).unwrap();
        let revision = db.revision().unwrap();
        let committed = db
            .transition(
                &value.invocation_id,
                Some("credential"),
                Delivery::Synthetic("declined"),
                None,
                Utc::now(),
                |_| Ok(None),
            )
            .unwrap();
        assert!(committed.is_none());
        assert_eq!(db.revision().unwrap(), revision);
        assert_eq!(db.invocation(&value.invocation_id).unwrap(), before);
        let err = db
            .transition(
                &value.invocation_id,
                Some("wrong"),
                Delivery::Synthetic("declined"),
                None,
                Utc::now(),
                |prior| Ok(Some(reducer::mark_lost(prior))),
            )
            .err()
            .unwrap();
        assert_eq!(err.to_string(), "invalid invocation credential");
        assert_eq!(db.revision().unwrap(), revision);
    }

    #[test]
    fn process_exit_between_reconcile_scan_and_transition_uses_the_committed_row() {
        let db = Storage::memory().unwrap();
        let sinks = hub_sinks(64 * 1024);
        let publish = hub_publish(&sinks);
        let mut value = snapshot();
        value.process.child_pid = Some(4242);
        db.register(&value, "credential", Some(&publish)).unwrap();
        let exited = std::cell::Cell::new(false);
        // The first liveness probe runs after the candidate scan and before the
        // lost transition; the wrapper reports the exit in between.
        let changed = db
            .reconcile(
                |_, _| {
                    if !exited.replace(true) {
                        db.mark_exit(
                            &value.invocation_id,
                            "credential",
                            Some(3),
                            None,
                            Some(&publish),
                        )
                        .unwrap();
                    }
                    false
                },
                7,
                Some(&publish),
            )
            .unwrap();
        assert_eq!(changed, 0);
        let stored = db.invocation(&value.invocation_id).unwrap();
        assert_eq!(stored.lifecycle, Lifecycle::Exited);
        assert_eq!(stored.process.exit_code, Some(3));
        let deliveries = hub_updates(&db)
            .iter()
            .map(|update| update_parts(update).1.to_owned())
            .collect::<Vec<_>>();
        assert_eq!(deliveries.len(), 2);
        assert!(deliveries[1].starts_with("synthetic:lifecycle_exit:"));
    }

    #[test]
    fn duplicate_event_ids_are_committed_once() {
        let db = Storage::memory().unwrap();
        let value = snapshot();
        db.register(&value, "credential", None).unwrap();
        db.apply_event(&normalized_event(&value, EventKind::NewTurn, "turn"), None)
            .unwrap()
            .unwrap();
        let revision = db.revision().unwrap();
        let mut replay = normalized_event(&value, EventKind::Idle, "turn");
        replay.received_at += chrono::Duration::seconds(5);
        assert!(db.apply_event(&replay, None).unwrap().is_none());
        assert_eq!(db.revision().unwrap(), revision);
        assert_eq!(
            db.invocation(&value.invocation_id).unwrap().activity,
            Activity::Working
        );
    }

    #[test]
    fn credential_and_provider_must_match() {
        let db = Storage::memory().unwrap();
        let value = snapshot();
        db.register(&value, "secret", None).unwrap();
        assert!(
            db.credential_matches(&value.invocation_id, "company-claude", "secret")
                .unwrap()
        );
        assert!(
            !db.credential_matches(&value.invocation_id, "codex", "secret")
                .unwrap()
        );
        assert!(
            !db.credential_matches(&value.invocation_id, "company-claude", "bad")
                .unwrap()
        );
        assert!(
            db.bind_child(&value.invocation_id, "bad", 7, None, None)
                .is_err()
        );
    }

    #[test]
    fn outbox_survives_restart_and_acknowledges_once() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state.sqlite3");
        let value = snapshot();
        let sinks = BTreeMap::from([(
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
        )]);
        let publish = Publish {
            sinks: &sinks,
            source_id: "",
            source_name: None,
        };
        {
            let db = Storage::open(&path).unwrap();
            db.register(&value, "secret", Some(&publish)).unwrap();
            db.apply_event(
                &normalized_event(&value, EventKind::NewTurn, "stable-event"),
                Some(&publish),
            )
            .unwrap();
        }
        let db = Storage::open(&path).unwrap();
        let records = db
            .due_outbox(10)
            .unwrap()
            .into_iter()
            .filter(|r| r.event_id == "stable-event")
            .collect::<Vec<_>>();
        assert_eq!(records.len(), 1);
        db.acknowledge("remote", "stable-event").unwrap();
        db.acknowledge("remote", "stable-event").unwrap();
        assert!(
            db.due_outbox(10)
                .unwrap()
                .iter()
                .all(|r| r.event_id != "stable-event")
        );
        assert_eq!(db.due_outbox(10).unwrap().len(), 1);
    }

    #[test]
    fn retry_identity_is_stable_across_outbox_redelivery() {
        let db = Storage::memory().unwrap();
        let sinks = hub_sinks(64 * 1024);
        let publish = hub_publish(&sinks);
        let value = snapshot();
        db.register(&value, "secret", Some(&publish)).unwrap();
        let before = hub_updates(&db);
        let delivery_id = update_parts(&before[0]).1.to_owned();
        db.retry("hub", &delivery_id, 0).unwrap();
        assert_eq!(hub_updates(&db), before);
        let conn = db.conn.lock().unwrap();
        let attempts: u32 = conn
            .query_row(
                "SELECT attempts FROM sink_outbox WHERE sink_name='hub'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(attempts, 1);
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
        old.lifecycle = Lifecycle::Exited;
        old.updated_at = Utc::now() - Duration::days(8);
        old.status = sessiontap_core::domain::derive_status(old.lifecycle, old.activity);
        db.register(&old, "old-secret", None).unwrap();
        db.reconcile(|_, _| false, 7, None).unwrap();
        assert!(db.invocation(&old.invocation_id).is_err());
        assert!(db.invocation(&live.invocation_id).is_ok());
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
    fn concurrent_hook_burst_is_idempotent() {
        use std::sync::Arc;
        let temp = tempfile::tempdir().unwrap();
        let db = Arc::new(Storage::open(&temp.path().join("burst.sqlite3")).unwrap());
        let value = snapshot();
        db.register(&value, "s", None).unwrap();
        let joins = (0..40)
            .map(|n| {
                let db = Arc::clone(&db);
                let event = normalized_event(&value, EventKind::Working, &format!("{}", n % 20));
                std::thread::spawn(move || db.apply_event(&event, None).unwrap())
            })
            .collect::<Vec<_>>();
        for join in joins {
            join.join().unwrap();
        }
        assert_eq!(db.revision().unwrap(), 21);
        let events: u64 = db
            .conn
            .lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM normalized_events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(events, 20);
    }

    #[test]
    fn concurrent_session_load_keeps_unique_snapshots_and_revisions() {
        use std::sync::Arc;
        let temp = tempfile::tempdir().unwrap();
        let db = Arc::new(Storage::open(&temp.path().join("load.sqlite3")).unwrap());
        let joins = (0..128)
            .map(|_| {
                let db = Arc::clone(&db);
                std::thread::spawn(move || {
                    let value = snapshot();
                    let revision = db.register(&value, "credential", None).unwrap();
                    (value.invocation_id, revision)
                })
            })
            .collect::<Vec<_>>();
        let results = joins
            .into_iter()
            .map(|join| join.join().unwrap())
            .collect::<Vec<_>>();
        let ids = results.iter().map(|(id, _)| id).collect::<BTreeSet<_>>();
        let revisions = results
            .iter()
            .map(|(_, revision)| *revision)
            .collect::<BTreeSet<_>>();
        let (revision, snapshots) = db.snapshot().unwrap();
        assert_eq!(ids.len(), 128);
        assert_eq!(revisions, (1..=128).collect());
        assert_eq!(snapshots.len(), 128);
        assert_eq!(revision, 128);
    }

    #[test]
    fn sink_backlog_is_bounded_under_burst_load() {
        let db = Storage::memory().unwrap();
        let value = snapshot();
        let sinks = BTreeMap::from([(
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
        )]);
        let publish = Publish {
            sinks: &sinks,
            source_id: "",
            source_name: None,
        };
        db.register(&value, "credential", Some(&publish)).unwrap();
        // Alternate states so every event is a public change.
        for index in 0..(MAX_OUTBOX_RECORDS_PER_SINK + 128) {
            let kind = if index % 2 == 0 {
                EventKind::Working
            } else {
                EventKind::Idle
            };
            db.apply_event(
                &normalized_event(&value, kind, &format!("load-{index}")),
                Some(&publish),
            )
            .unwrap();
        }
        let count: u64 = db
            .conn
            .lock()
            .unwrap()
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
        let sinks = hub_sinks(64 * 1024);
        let publish = hub_publish(&sinks);
        let mut value = snapshot();
        value.lifecycle = Lifecycle::Starting;
        db.register(&value, "secret", Some(&publish)).unwrap();
        db.bind_child(&value.invocation_id, "secret", 42, None, Some(&publish))
            .unwrap();
        db.apply_event(
            &normalized_event(&value, EventKind::NewTurn, "turn"),
            Some(&publish),
        )
        .unwrap();
        db.mark_exit(
            &value.invocation_id,
            "secret",
            Some(0),
            None,
            Some(&publish),
        )
        .unwrap();

        let mut lost = snapshot();
        lost.process.child_pid = Some(424_242);
        db.register(&lost, "secret", Some(&publish)).unwrap();
        assert_eq!(db.reconcile(|_, _| false, 7, Some(&publish)).unwrap(), 1);

        let updates = hub_updates(&db);
        let summary = updates
            .iter()
            .map(|update| {
                let (source_id, delivery_id, revision, _, view) = update_parts(update);
                assert_eq!(source_id, "host");
                let label = delivery_id
                    .split(':')
                    .nth(1)
                    .unwrap_or(delivery_id)
                    .to_owned();
                (label, revision, view.status)
            })
            .collect::<Vec<_>>();
        assert_eq!(
            summary,
            vec![
                ("register".into(), 1, PublicStatus::Idle),
                ("turn".into(), 3, PublicStatus::Running),
                ("lifecycle_exit".into(), 4, PublicStatus::Stopped),
                ("register".into(), 5, PublicStatus::Idle),
                ("reconcile_lost".into(), 6, PublicStatus::Stopped),
            ]
        );
    }

    #[test]
    fn hub_update_carries_reason_then_omits_it_when_cleared() {
        let db = Storage::memory().unwrap();
        let sinks = hub_sinks(64 * 1024);
        let publish = hub_publish(&sinks);
        let value = snapshot();
        db.register(&value, "secret", Some(&publish)).unwrap();
        db.apply_event_with_context(
            &normalized_event(&value, EventKind::WaitingApproval, "wait-1"),
            Some(&StatusReasonContext {
                summary: "Approve tests".into(),
                source: sessiontap_core::domain::StatusReasonSource::ToolSummary,
            }),
            Some(&publish),
        )
        .unwrap();
        db.apply_event(
            &normalized_event(&value, EventKind::Working, "resume"),
            Some(&publish),
        )
        .unwrap();
        db.apply_event_with_context(
            &normalized_event(&value, EventKind::Failed, "failed"),
            Some(&StatusReasonContext {
                summary: "Rate limited".into(),
                source: sessiontap_core::domain::StatusReasonSource::FailureCategory,
            }),
            Some(&publish),
        )
        .unwrap();
        let updates = hub_updates(&db);
        assert_eq!(updates.len(), 4);
        let (_, _, _, changed, view) = update_parts(&updates[1]);
        assert_eq!(
            view.reason.as_ref().unwrap().kind,
            PublicReasonKind::Approval
        );
        assert_eq!(view.reason.as_ref().unwrap().summary, "Approve tests");
        assert!(changed.contains(&PublicField::Reason));
        let (_, _, _, changed, view) = update_parts(&updates[2]);
        assert!(view.reason.is_none());
        assert!(changed.contains(&PublicField::Reason));
        let (_, _, _, _, view) = update_parts(&updates[3]);
        assert_eq!(view.reason.as_ref().unwrap().kind, PublicReasonKind::Failed);
    }

    #[test]
    fn semantically_suppressed_duplicates_are_not_delivered() {
        let db = Storage::memory().unwrap();
        let sinks = hub_sinks(64 * 1024);
        let publish = hub_publish(&sinks);
        let value = snapshot();
        db.register(&value, "secret", Some(&publish)).unwrap();
        db.apply_event(
            &normalized_event(&value, EventKind::NewTurn, "turn"),
            Some(&publish),
        )
        .unwrap();
        assert!(
            db.apply_event(
                &normalized_event(&value, EventKind::NewTurn, "turn"),
                Some(&publish)
            )
            .unwrap()
            .is_none()
        );
        assert!(
            db.apply_event(
                &normalized_event(&value, EventKind::Working, "same"),
                Some(&publish)
            )
            .unwrap()
            .is_none()
        );
        let updates = hub_updates(&db);
        assert_eq!(updates.len(), 2);
        assert_eq!(update_parts(&updates[1]).1, "turn");
    }

    #[test]
    fn repeated_waiting_reason_is_delivered_as_a_reason_change() {
        let db = Storage::memory().unwrap();
        let sinks = hub_sinks(64 * 1024);
        let publish = hub_publish(&sinks);
        let value = snapshot();
        db.register(&value, "secret", Some(&publish)).unwrap();
        db.apply_event_with_context(
            &normalized_event(&value, EventKind::WaitingInput, "w1"),
            Some(&question("First")),
            Some(&publish),
        )
        .unwrap();
        db.apply_event_with_context(
            &normalized_event(&value, EventKind::WaitingInput, "w2"),
            Some(&question("Second")),
            Some(&publish),
        )
        .unwrap();
        let updates = hub_updates(&db);
        assert_eq!(updates.len(), 3);
        let (_, _, _, changed, view) = update_parts(&updates[2]);
        assert_eq!(view.reason.as_ref().unwrap().summary, "Second");
        assert_eq!(
            *changed,
            BTreeSet::from([PublicField::Reason, PublicField::UpdatedAt])
        );
    }

    #[test]
    fn snapshot_delivery_subsumes_earlier_updates_and_keeps_later_ones() {
        let db = Storage::memory().unwrap();
        let sinks = hub_sinks(64 * 1024);
        let publish = hub_publish(&sinks);
        let value = snapshot();
        db.register(&value, "secret", Some(&publish)).unwrap();
        db.apply_event(
            &normalized_event(&value, EventKind::NewTurn, "early"),
            Some(&publish),
        )
        .unwrap();
        let (revision, payload) = db.hub_source_snapshot("host", Some("Host")).unwrap();
        match serde_json::from_slice(&payload).unwrap() {
            SourceEnvelope::Snapshot {
                source,
                revision: snapshot_revision,
                views,
                ..
            } => {
                assert_eq!(source.id, "host");
                assert_eq!(source.display_name.as_deref(), Some("Host"));
                assert_eq!(snapshot_revision, revision);
                assert_eq!(revision, 2);
                assert_eq!(views.len(), 1);
                assert_eq!(views[0].status, PublicStatus::Running);
            }
            SourceEnvelope::Update { .. } => panic!("expected snapshot"),
        }
        assert!(db.hub_snapshot_due("hub").unwrap());
        db.hub_snapshot_delivered("hub", revision).unwrap();
        assert!(!db.hub_snapshot_due("hub").unwrap());
        assert!(hub_updates(&db).is_empty());
        db.apply_event(
            &normalized_event(&value, EventKind::Idle, "later"),
            Some(&publish),
        )
        .unwrap();
        let updates = hub_updates(&db);
        assert_eq!(updates.len(), 1);
        assert!(update_parts(&updates[0]).2 > revision);
    }

    #[test]
    fn snapshot_reset_allows_repair_after_receiver_state_loss() {
        let db = Storage::memory().unwrap();
        let sinks = hub_sinks(64 * 1024);
        let publish = hub_publish(&sinks);
        db.register(&snapshot(), "secret", Some(&publish)).unwrap();
        let (revision, _) = db.hub_source_snapshot("host", None).unwrap();
        db.hub_snapshot_delivered("hub", revision).unwrap();
        assert!(!db.hub_snapshot_due("hub").unwrap());
        db.hub_reset_snapshot("hub").unwrap();
        assert!(db.hub_snapshot_due("hub").unwrap());
    }

    #[test]
    fn hub_payload_limit_drops_oversized_updates() {
        let db = Storage::memory().unwrap();
        let sinks = hub_sinks(64);
        let publish = hub_publish(&sinks);
        db.register(&snapshot(), "secret", Some(&publish)).unwrap();
        assert!(hub_updates(&db).is_empty());
    }

    #[test]
    fn hub_envelopes_are_skipped_without_source_identity() {
        let db = Storage::memory().unwrap();
        let sinks = hub_sinks(64 * 1024);
        let publish = Publish {
            sinks: &sinks,
            source_id: "",
            source_name: None,
        };
        db.register(&snapshot(), "secret", Some(&publish)).unwrap();
        assert!(hub_updates(&db).is_empty());
    }

    #[test]
    fn local_context_never_enters_event_history_or_sink_outbox() {
        let db = Storage::memory().unwrap();
        let value = snapshot();
        let sinks = BTreeMap::from([(
            "debug".into(),
            SinkConfig::Stdout {
                enabled: true,
                fields: vec![],
            },
        )]);
        let publish = Publish {
            sinks: &sinks,
            source_id: "",
            source_name: None,
        };
        db.register(&value, "secret", Some(&publish)).unwrap();
        db.apply_event_with_context(
            &normalized_event(&value, EventKind::WaitingApproval, "private-boundary"),
            Some(&StatusReasonContext {
                summary: "PRIVATE-CONTEXT".into(),
                source: sessiontap_core::domain::StatusReasonSource::Description,
            }),
            Some(&publish),
        )
        .unwrap();
        // The selected summary may be projected as the bounded public reason
        // while blocked, but never enters history, the snapshot row, or later
        // deliveries.
        db.apply_event(
            &normalized_event(&value, EventKind::Working, "after"),
            Some(&publish),
        )
        .unwrap();
        let conn = db.conn.lock().unwrap();
        let history: String = conn
            .query_row(
                "SELECT group_concat(event_json) FROM normalized_events",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let persisted: String = conn
            .query_row("SELECT snapshot_json FROM invocations", [], |r| r.get(0))
            .unwrap();
        let payloads: Vec<Vec<u8>> = conn
            .prepare("SELECT payload FROM sink_outbox WHERE event_id='after'")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(payloads.len(), 1);
        for value in [
            history.as_bytes(),
            persisted.as_bytes(),
            payloads[0].as_slice(),
        ] {
            assert!(!String::from_utf8_lossy(value).contains("PRIVATE-CONTEXT"));
        }
    }
}

#[cfg(test)]
mod replay_tests;
