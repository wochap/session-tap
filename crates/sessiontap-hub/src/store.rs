use anyhow::{Context, Result, bail};
use chrono::{Duration, Utc};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use sessiontap_core::{
    domain::{ActiveAttention, InvocationId, InvocationSnapshot},
    protocol::{HUB_SCHEMA_VERSION, HubEnvelope},
};
use std::{collections::BTreeSet, path::Path, sync::Mutex};

const MIGRATION: &str = r#"
CREATE TABLE IF NOT EXISTS hub_meta (key TEXT PRIMARY KEY, value INTEGER NOT NULL);
INSERT OR IGNORE INTO hub_meta(key,value) VALUES ('revision',0);
CREATE TABLE IF NOT EXISTS sources (
 source_id TEXT PRIMARY KEY,
 display_name TEXT,
 source_revision INTEGER NOT NULL DEFAULT 0,
 updated_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS invocations (
 source_id TEXT NOT NULL REFERENCES sources(source_id) ON DELETE CASCADE,
 invocation_id TEXT NOT NULL,
 snapshot_json TEXT NOT NULL,
 attention_json TEXT,
 updated_at TEXT NOT NULL,
 stopped_at TEXT,
 PRIMARY KEY (source_id, invocation_id)
);
CREATE TABLE IF NOT EXISTS accepted_events (
 source_id TEXT NOT NULL,
 event_id TEXT NOT NULL,
 accepted_at TEXT NOT NULL,
 PRIMARY KEY (source_id, event_id)
);
"#;

/// Canonical fields compared when computing material changes between the
/// previously persisted state and an accepted resulting state.
pub const CANONICAL_FIELDS: &[&str] = &[
    "status",
    "lifecycle",
    "activity",
    "usage",
    "provider_session",
    "provider_metadata",
    "repository",
    "multiplexer",
    "process",
    "attention",
];

pub struct HubStore {
    conn: Mutex<Connection>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceView {
    pub source_id: String,
    pub display_name: Option<String>,
    pub revision: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergedInvocation {
    pub source_id: String,
    pub snapshot: InvocationSnapshot,
    pub attention: Option<ActiveAttention>,
}

/// Reason an ingestion request is rejected before any state change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reject {
    UnsupportedVersion(u32),
    Malformed(String),
    /// An update arrived for a source with no established snapshot baseline.
    SnapshotRequired,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapshotAccept {
    Applied {
        hub_revision: u64,
    },
    /// The snapshot is not newer than the materialized source revision.
    Stale,
}

#[derive(Debug, Clone, PartialEq)]
pub enum UpdateAccept {
    Applied {
        hub_revision: u64,
        changed: Vec<String>,
        first_seen: bool,
    },
    /// The `(source_id, event_id)` pair was already accepted.
    Duplicate,
    /// The revision is not newer than the materialized source revision.
    Stale,
}

impl HubStore {
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
        let conn = self.conn.lock().expect("hub store mutex poisoned");
        Ok(
            conn.query_row("SELECT value FROM hub_meta WHERE key='revision'", [], |r| {
                r.get(0)
            })?,
        )
    }

    /// Persisted merged view: hub revision, known sources, and every retained
    /// invocation with its current attention.
    pub fn merged(&self) -> Result<(u64, Vec<SourceView>, Vec<MergedInvocation>)> {
        let conn = self.conn.lock().expect("hub store mutex poisoned");
        let revision =
            conn.query_row("SELECT value FROM hub_meta WHERE key='revision'", [], |r| {
                r.get(0)
            })?;
        let mut stmt = conn.prepare(
            "SELECT source_id,display_name,source_revision FROM sources ORDER BY source_id",
        )?;
        let sources = stmt
            .query_map([], |r| {
                Ok(SourceView {
                    source_id: r.get(0)?,
                    display_name: r.get(1)?,
                    revision: r.get(2)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut stmt = conn.prepare(
            "SELECT source_id,snapshot_json,attention_json FROM invocations ORDER BY source_id,invocation_id",
        )?;
        let invocations = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                ))
            })?
            .map(|row| {
                let (source_id, snapshot_raw, attention_raw) = row?;
                Ok(MergedInvocation {
                    source_id,
                    snapshot: serde_json::from_str(&snapshot_raw)?,
                    attention: attention_raw
                        .map(|raw| serde_json::from_str(&raw))
                        .transpose()?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok((revision, sources, invocations))
    }

    pub fn prior_invocation(
        conn: &Transaction<'_>,
        source_id: &str,
        invocation_id: &InvocationId,
    ) -> Result<Option<MergedInvocation>> {
        let row: Option<(String, Option<String>)> = conn
            .query_row(
                "SELECT snapshot_json,attention_json FROM invocations WHERE source_id=?1 AND invocation_id=?2",
                params![source_id, invocation_id.to_string()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        row.map(|(snapshot_raw, attention_raw)| {
            Ok(MergedInvocation {
                source_id: source_id.to_owned(),
                snapshot: serde_json::from_str(&snapshot_raw)?,
                attention: attention_raw
                    .map(|raw| serde_json::from_str(&raw))
                    .transpose()?,
            })
        })
        .transpose()
    }

    /// Transactionally replaces the materialized invocation set for one source
    /// from a complete snapshot while preserving other sources.
    pub fn ingest_snapshot(
        &self,
        envelope: &HubEnvelope,
    ) -> std::result::Result<SnapshotAccept, Reject> {
        let HubEnvelope::Snapshot {
            schema_version,
            source,
            revision,
            invocations,
            active_attention,
        } = envelope
        else {
            return Err(Reject::Malformed("expected snapshot envelope".into()));
        };
        if *schema_version != HUB_SCHEMA_VERSION {
            return Err(Reject::UnsupportedVersion(*schema_version));
        }
        if source.id.is_empty() {
            return Err(Reject::Malformed("empty source identity".into()));
        }
        let mut conn = self.conn.lock().expect("hub store mutex poisoned");
        let tx = conn
            .transaction()
            .map_err(|e| Reject::Malformed(e.to_string()))?;
        let known: Option<u64> = tx
            .query_row(
                "SELECT source_revision FROM sources WHERE source_id=?1",
                [&source.id],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| Reject::Malformed(e.to_string()))?;
        if known.is_some_and(|current| *revision <= current) {
            return Ok(SnapshotAccept::Stale);
        }
        tx.execute(
            "INSERT INTO sources(source_id,display_name,source_revision,updated_at) VALUES (?1,?2,?3,?4) ON CONFLICT(source_id) DO UPDATE SET display_name=excluded.display_name,source_revision=excluded.source_revision,updated_at=excluded.updated_at",
            params![source.id, source.display_name, revision, Utc::now().to_rfc3339()],
        )
        .map_err(|e| Reject::Malformed(e.to_string()))?;
        let retained: BTreeSet<String> = invocations
            .iter()
            .map(|s| s.invocation_id.to_string())
            .collect();
        let existing: Vec<String> = {
            let mut stale = tx
                .prepare("SELECT invocation_id FROM invocations WHERE source_id=?1")
                .map_err(|e| Reject::Malformed(e.to_string()))?;
            stale
                .query_map([&source.id], |r| r.get(0))
                .map_err(|e| Reject::Malformed(e.to_string()))?
                .collect::<rusqlite::Result<_>>()
                .map_err(|e| Reject::Malformed(e.to_string()))?
        };
        for invocation_id in existing {
            if !retained.contains(&invocation_id) {
                tx.execute(
                    "DELETE FROM invocations WHERE source_id=?1 AND invocation_id=?2",
                    params![source.id, invocation_id],
                )
                .map_err(|e| Reject::Malformed(e.to_string()))?;
            }
        }
        let now = Utc::now().to_rfc3339();
        for snapshot in invocations {
            let stopped = matches!(
                snapshot.lifecycle,
                sessiontap_core::domain::Lifecycle::Exited
                    | sessiontap_core::domain::Lifecycle::Lost
            )
            .then(|| snapshot.updated_at.to_rfc3339());
            let attention = active_attention
                .get(&snapshot.invocation_id)
                .map(serde_json::to_string)
                .transpose()
                .map_err(|e| Reject::Malformed(e.to_string()))?;
            tx.execute(
                "INSERT INTO invocations(source_id,invocation_id,snapshot_json,attention_json,updated_at,stopped_at) VALUES (?1,?2,?3,?4,?5,?6) ON CONFLICT(source_id,invocation_id) DO UPDATE SET snapshot_json=excluded.snapshot_json,attention_json=excluded.attention_json,updated_at=excluded.updated_at,stopped_at=excluded.stopped_at",
                params![source.id, snapshot.invocation_id.to_string(), serde_json::to_string(snapshot).map_err(|e| Reject::Malformed(e.to_string()))?, attention, now, stopped],
            )
            .map_err(|e| Reject::Malformed(e.to_string()))?;
        }
        let hub_revision = bump_revision(&tx).map_err(|e| Reject::Malformed(e.to_string()))?;
        tx.commit().map_err(|e| Reject::Malformed(e.to_string()))?;
        Ok(SnapshotAccept::Applied { hub_revision })
    }

    /// Idempotently applies one canonical update keyed by
    /// `(source_id, event_id)` without provider-specific interpretation.
    pub fn ingest_update(
        &self,
        envelope: &HubEnvelope,
    ) -> std::result::Result<UpdateAccept, Reject> {
        let HubEnvelope::Update {
            schema_version,
            source_id,
            event_id,
            revision,
            event: _,
            snapshot,
            attention,
        } = envelope
        else {
            return Err(Reject::Malformed("expected update envelope".into()));
        };
        if *schema_version != HUB_SCHEMA_VERSION {
            return Err(Reject::UnsupportedVersion(*schema_version));
        }
        if source_id.is_empty() || event_id.is_empty() {
            return Err(Reject::Malformed("missing delivery identity".into()));
        }
        if snapshot.invocation_id.to_string().is_empty() {
            return Err(Reject::Malformed("missing invocation identity".into()));
        }
        let mut conn = self.conn.lock().expect("hub store mutex poisoned");
        let tx = conn
            .transaction()
            .map_err(|e| Reject::Malformed(e.to_string()))?;
        let source_revision: Option<u64> = tx
            .query_row(
                "SELECT source_revision FROM sources WHERE source_id=?1",
                [source_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| Reject::Malformed(e.to_string()))?;
        let Some(source_revision) = source_revision else {
            return Err(Reject::SnapshotRequired);
        };
        if tx
            .query_row(
                "SELECT 1 FROM accepted_events WHERE source_id=?1 AND event_id=?2",
                params![source_id, event_id],
                |_| Ok(()),
            )
            .optional()
            .map_err(|e| Reject::Malformed(e.to_string()))?
            .is_some()
        {
            return Ok(UpdateAccept::Duplicate);
        }
        if *revision <= source_revision {
            return Ok(UpdateAccept::Stale);
        }
        let prior = Self::prior_invocation(&tx, source_id, &snapshot.invocation_id)
            .map_err(|e| Reject::Malformed(e.to_string()))?;
        let stopped = matches!(
            snapshot.lifecycle,
            sessiontap_core::domain::Lifecycle::Exited | sessiontap_core::domain::Lifecycle::Lost
        )
        .then(|| snapshot.updated_at.to_rfc3339());
        let now = Utc::now().to_rfc3339();
        tx.execute(
            "INSERT INTO invocations(source_id,invocation_id,snapshot_json,attention_json,updated_at,stopped_at) VALUES (?1,?2,?3,?4,?5,?6) ON CONFLICT(source_id,invocation_id) DO UPDATE SET snapshot_json=excluded.snapshot_json,attention_json=excluded.attention_json,updated_at=excluded.updated_at,stopped_at=excluded.stopped_at",
            params![
                source_id,
                snapshot.invocation_id.to_string(),
                serde_json::to_string(snapshot).map_err(|e| Reject::Malformed(e.to_string()))?,
                attention
                    .as_ref()
                    .map(serde_json::to_string)
                    .transpose()
                    .map_err(|e| Reject::Malformed(e.to_string()))?,
                now,
                stopped
            ],
        )
        .map_err(|e| Reject::Malformed(e.to_string()))?;
        tx.execute(
            "UPDATE sources SET source_revision=?2, updated_at=?3 WHERE source_id=?1",
            params![source_id, revision, now],
        )
        .map_err(|e| Reject::Malformed(e.to_string()))?;
        tx.execute(
            "INSERT INTO accepted_events(source_id,event_id,accepted_at) VALUES (?1,?2,?3)",
            params![source_id, event_id, now],
        )
        .map_err(|e| Reject::Malformed(e.to_string()))?;
        let hub_revision = bump_revision(&tx).map_err(|e| Reject::Malformed(e.to_string()))?;
        tx.commit().map_err(|e| Reject::Malformed(e.to_string()))?;
        let changed = changed_fields(prior.as_ref(), snapshot, attention.as_ref());
        Ok(UpdateAccept::Applied {
            hub_revision,
            changed,
            first_seen: prior.is_none(),
        })
    }

    /// Retention: removes stopped invocations and accepted-event identities
    /// older than the configured horizon.
    pub fn prune_retained(&self, retention_days: u64) -> Result<usize> {
        let cutoff = (Utc::now()
            - Duration::days(i64::try_from(retention_days).unwrap_or(i64::MAX)))
        .to_rfc3339();
        let conn = self.conn.lock().expect("hub store mutex poisoned");
        let invocations = conn.execute(
            "DELETE FROM invocations WHERE stopped_at IS NOT NULL AND stopped_at < ?1",
            [&cutoff],
        )?;
        let events = conn.execute(
            "DELETE FROM accepted_events WHERE accepted_at < ?1",
            [&cutoff],
        )?;
        conn.execute("DELETE FROM sources WHERE source_id NOT IN (SELECT DISTINCT source_id FROM invocations)", [])?;
        Ok(invocations + events)
    }

    pub fn has_source(&self, source_id: &str) -> Result<bool> {
        let conn = self.conn.lock().expect("hub store mutex poisoned");
        Ok(conn
            .query_row(
                "SELECT 1 FROM sources WHERE source_id=?1",
                [source_id],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }
}

fn bump_revision(tx: &Transaction<'_>) -> Result<u64> {
    tx.execute("UPDATE hub_meta SET value=value+1 WHERE key='revision'", [])?;
    Ok(
        tx.query_row("SELECT value FROM hub_meta WHERE key='revision'", [], |r| {
            r.get(0)
        })?,
    )
}

/// Canonical changed-field computation against the previously persisted state.
/// A previously unknown invocation reports every canonical field as changed.
pub fn changed_fields(
    prior: Option<&MergedInvocation>,
    snapshot: &InvocationSnapshot,
    attention: Option<&ActiveAttention>,
) -> Vec<String> {
    let Some(prior) = prior else {
        return CANONICAL_FIELDS.iter().map(|s| (*s).to_owned()).collect();
    };
    let before = serde_json::to_value(&prior.snapshot).unwrap_or_default();
    let after = serde_json::to_value(snapshot).unwrap_or_default();
    let mut changed: Vec<String> = CANONICAL_FIELDS
        .iter()
        .filter(|field| **field != "attention")
        .filter(|field| before.get(**field) != after.get(**field))
        .map(|field| (*field).to_owned())
        .collect();
    let attention_before = prior
        .attention
        .as_ref()
        .and_then(|a| serde_json::to_value(a).ok());
    let attention_after = attention.and_then(|a| serde_json::to_value(a).ok());
    if attention_before != attention_after {
        changed.push("attention".into());
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use sessiontap_core::{
        domain::{
            Activity, AttentionContext, AttentionSource, Capabilities, EventKind, Lifecycle,
            ProcessMetadata, PublicStatus,
        },
        protocol::{HubEventMetadata, SourceCapabilities, SourceIdentity},
    };

    fn test_snapshot(provider: &str) -> InvocationSnapshot {
        let now = Utc::now();
        InvocationSnapshot {
            schema_version: 1,
            revision: 1,
            invocation_id: InvocationId::new(),
            provider: provider.into(),
            executable: provider.into(),
            args: vec![],
            cwd: "/tmp".into(),
            process: ProcessMetadata::default(),
            created_at: now,
            updated_at: now,
            lifecycle: Lifecycle::Alive,
            activity: Activity::Working,
            status: PublicStatus::Running,
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

    fn snapshot_envelope(
        source: &str,
        revision: u64,
        invocations: Vec<InvocationSnapshot>,
    ) -> HubEnvelope {
        HubEnvelope::Snapshot {
            schema_version: HUB_SCHEMA_VERSION,
            source: SourceIdentity {
                id: source.into(),
                display_name: None,
                capabilities: SourceCapabilities::default(),
            },
            revision,
            invocations,
            active_attention: Default::default(),
        }
    }

    fn update_envelope(
        source: &str,
        event_id: &str,
        revision: u64,
        snapshot: InvocationSnapshot,
        attention: Option<ActiveAttention>,
    ) -> HubEnvelope {
        let now = Utc::now();
        HubEnvelope::Update {
            schema_version: HUB_SCHEMA_VERSION,
            source_id: source.into(),
            event_id: event_id.into(),
            revision,
            event: HubEventMetadata {
                kind: EventKind::Working,
                observed_at: now,
                received_at: now,
                failure: None,
                turn_id: None,
            },
            snapshot: Box::new(snapshot),
            attention,
        }
    }

    #[test]
    fn snapshot_materializes_source_state() {
        let store = HubStore::memory().unwrap();
        let invocation = test_snapshot("claude");
        let envelope = snapshot_envelope("host", 5, vec![invocation.clone()]);
        let accept = store.ingest_snapshot(&envelope).unwrap();
        assert!(matches!(
            accept,
            SnapshotAccept::Applied { hub_revision: 1 }
        ));
        let (hub_revision, sources, invocations) = store.merged().unwrap();
        assert_eq!(hub_revision, 1);
        assert_eq!(sources[0].source_id, "host");
        assert_eq!(sources[0].revision, 5);
        assert_eq!(invocations.len(), 1);
        assert_eq!(invocations[0].source_id, "host");
        assert_eq!(
            invocations[0].snapshot.invocation_id,
            invocation.invocation_id
        );
    }

    #[test]
    fn stale_snapshot_is_acknowledged_without_replacement() {
        let store = HubStore::memory().unwrap();
        store
            .ingest_snapshot(&snapshot_envelope(
                "host",
                10,
                vec![test_snapshot("claude")],
            ))
            .unwrap();
        let stale = store
            .ingest_snapshot(&snapshot_envelope("host", 4, vec![]))
            .unwrap();
        assert_eq!(stale, SnapshotAccept::Stale);
        let (_, _, invocations) = store.merged().unwrap();
        assert_eq!(invocations.len(), 1);
    }

    #[test]
    fn snapshot_replaces_only_the_source_it_describes() {
        let store = HubStore::memory().unwrap();
        let host_agent = test_snapshot("claude");
        let sandbox_agent = test_snapshot("codex");
        store
            .ingest_snapshot(&snapshot_envelope("host", 1, vec![host_agent.clone()]))
            .unwrap();
        store
            .ingest_snapshot(&snapshot_envelope(
                "sandbox",
                1,
                vec![sandbox_agent.clone()],
            ))
            .unwrap();
        let removed = test_snapshot("claude");
        let mut kept = test_snapshot("claude");
        kept.invocation_id = host_agent.invocation_id.clone();
        store
            .ingest_snapshot(&snapshot_envelope("host", 2, vec![kept.clone()]))
            .unwrap();
        let (_, sources, invocations) = store.merged().unwrap();
        assert_eq!(sources.len(), 2);
        assert_eq!(invocations.len(), 2);
        let host_ids: Vec<_> = invocations
            .iter()
            .filter(|i| i.source_id == "host")
            .map(|i| i.snapshot.invocation_id.clone())
            .collect();
        assert_eq!(host_ids, vec![host_agent.invocation_id]);
        assert!(
            invocations.iter().any(|i| i.source_id == "sandbox"
                && i.snapshot.invocation_id == sandbox_agent.invocation_id)
        );
        assert!(
            !invocations
                .iter()
                .any(|i| i.snapshot.invocation_id == removed.invocation_id)
        );
    }

    #[test]
    fn same_invocation_id_from_two_sources_stays_distinct() {
        let store = HubStore::memory().unwrap();
        let shared = test_snapshot("claude");
        store
            .ingest_snapshot(&snapshot_envelope("host", 1, vec![shared.clone()]))
            .unwrap();
        store
            .ingest_snapshot(&snapshot_envelope("sandbox", 1, vec![shared.clone()]))
            .unwrap();
        let (_, _, invocations) = store.merged().unwrap();
        assert_eq!(invocations.len(), 2);
    }

    #[test]
    fn update_without_baseline_requires_snapshot() {
        let store = HubStore::memory().unwrap();
        let invocation = test_snapshot("claude");
        let update = update_envelope("host", "event-1", 1, invocation, None);
        assert_eq!(
            store.ingest_update(&update).unwrap_err(),
            Reject::SnapshotRequired
        );
    }

    #[test]
    fn update_applies_stores_attention_and_bumps_revision() {
        let store = HubStore::memory().unwrap();
        let mut invocation = test_snapshot("claude");
        invocation.activity = Activity::WaitingInput;
        invocation.status = PublicStatus::Blocked;
        store
            .ingest_snapshot(&snapshot_envelope("host", 1, vec![invocation.clone()]))
            .unwrap();
        let attention = ActiveAttention {
            kind: EventKind::WaitingInput,
            context: AttentionContext {
                summary: "Choose option".into(),
                source: AttentionSource::Question,
            },
        };
        let update = update_envelope(
            "host",
            "event-1",
            2,
            invocation.clone(),
            Some(attention.clone()),
        );
        let accept = store.ingest_update(&update).unwrap();
        match accept {
            UpdateAccept::Applied {
                hub_revision,
                changed,
                first_seen,
            } => {
                assert_eq!(hub_revision, 2);
                // the invocation was already materialized by the snapshot
                assert!(!first_seen);
                assert!(changed.contains(&"attention".to_owned()));
            }
            other => panic!("unexpected accept: {other:?}"),
        }
        let (_, _, invocations) = store.merged().unwrap();
        assert_eq!(invocations[0].attention, Some(attention));
    }

    #[test]
    fn duplicate_delivery_is_acknowledged_once() {
        let store = HubStore::memory().unwrap();
        let invocation = test_snapshot("claude");
        store
            .ingest_snapshot(&snapshot_envelope("host", 1, vec![invocation.clone()]))
            .unwrap();
        let update = update_envelope("host", "event-1", 2, invocation.clone(), None);
        assert!(matches!(
            store.ingest_update(&update).unwrap(),
            UpdateAccept::Applied {
                hub_revision: 2,
                ..
            }
        ));
        assert_eq!(
            store.ingest_update(&update).unwrap(),
            UpdateAccept::Duplicate
        );
        assert_eq!(store.revision().unwrap(), 2);
    }

    #[test]
    fn stale_revision_is_acknowledged_without_state_change() {
        let store = HubStore::memory().unwrap();
        let invocation = test_snapshot("claude");
        store
            .ingest_snapshot(&snapshot_envelope("host", 10, vec![invocation.clone()]))
            .unwrap();
        let mut older = invocation.clone();
        older.activity = Activity::Idle;
        let update = update_envelope("host", "old-event", 3, older, None);
        assert_eq!(store.ingest_update(&update).unwrap(), UpdateAccept::Stale);
        let (_, sources, _) = store.merged().unwrap();
        assert_eq!(sources[0].revision, 10);
    }

    #[test]
    fn attention_clearing_is_materialized_and_reported() {
        let store = HubStore::memory().unwrap();
        let mut invocation = test_snapshot("claude");
        invocation.activity = Activity::WaitingInput;
        invocation.status = PublicStatus::Blocked;
        let attention = ActiveAttention {
            kind: EventKind::WaitingInput,
            context: AttentionContext {
                summary: "Waiting".into(),
                source: AttentionSource::GenericInput,
            },
        };
        let mut envelope = snapshot_envelope("host", 1, vec![invocation.clone()]);
        if let HubEnvelope::Snapshot {
            active_attention, ..
        } = &mut envelope
        {
            active_attention.insert(invocation.invocation_id.clone(), attention);
        }
        store.ingest_snapshot(&envelope).unwrap();
        let mut resumed = invocation.clone();
        resumed.activity = Activity::Working;
        resumed.status = PublicStatus::Running;
        let update = update_envelope("host", "resume", 2, resumed, None);
        let accept = store.ingest_update(&update).unwrap();
        match accept {
            UpdateAccept::Applied { changed, .. } => {
                assert!(changed.contains(&"attention".to_owned()));
                assert!(changed.contains(&"status".to_owned()));
            }
            other => panic!("unexpected accept: {other:?}"),
        }
        let (_, _, invocations) = store.merged().unwrap();
        assert_eq!(invocations[0].attention, None);
    }

    #[test]
    fn unsupported_schema_version_is_rejected() {
        let store = HubStore::memory().unwrap();
        let mut envelope = snapshot_envelope("host", 1, vec![]);
        if let HubEnvelope::Snapshot { schema_version, .. } = &mut envelope {
            *schema_version = 99;
        }
        assert_eq!(
            store.ingest_snapshot(&envelope).unwrap_err(),
            Reject::UnsupportedVersion(99)
        );
    }

    #[test]
    fn restart_restores_merged_state() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("hub.sqlite3");
        let invocation = test_snapshot("claude");
        {
            let store = HubStore::open(&path).unwrap();
            store
                .ingest_snapshot(&snapshot_envelope("host", 1, vec![invocation.clone()]))
                .unwrap();
        }
        let store = HubStore::open(&path).unwrap();
        let (revision, sources, invocations) = store.merged().unwrap();
        assert_eq!(revision, 1);
        assert_eq!(sources[0].source_id, "host");
        assert_eq!(invocations.len(), 1);
    }

    #[test]
    fn retention_prunes_stopped_invocations_and_event_identities() {
        let store = HubStore::memory().unwrap();
        let mut stopped = test_snapshot("claude");
        stopped.lifecycle = Lifecycle::Exited;
        stopped.status = PublicStatus::Stopped;
        stopped.updated_at = Utc::now() - Duration::days(30);
        store
            .ingest_snapshot(&snapshot_envelope("host", 1, vec![stopped.clone()]))
            .unwrap();
        let active = test_snapshot("codex");
        store
            .ingest_snapshot(&snapshot_envelope("sandbox", 1, vec![active.clone()]))
            .unwrap();
        let update = update_envelope("host", "ancient", 2, stopped.clone(), None);
        // aged accepted event via direct SQL to simulate passage of time
        store.ingest_update(&update).unwrap();
        {
            let conn = store.conn.lock().unwrap();
            conn.execute(
                "UPDATE accepted_events SET accepted_at=?1",
                [(Utc::now() - Duration::days(30)).to_rfc3339()],
            )
            .unwrap();
        }
        let pruned = store.prune_retained(7).unwrap();
        assert!(pruned >= 2);
        let (_, _, invocations) = store.merged().unwrap();
        assert_eq!(invocations.len(), 1);
        assert_eq!(invocations[0].source_id, "sandbox");
        let conn = store.conn.lock().unwrap();
        let accepted: u64 = conn
            .query_row("SELECT COUNT(*) FROM accepted_events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(accepted, 0);
    }

    #[test]
    fn changed_fields_detects_status_and_ignore_enrichment() {
        let base = test_snapshot("claude");
        let prior = MergedInvocation {
            source_id: "host".into(),
            snapshot: base.clone(),
            attention: None,
        };
        let mut next = base.clone();
        next.usage = Some(sessiontap_core::domain::Usage {
            input_tokens: Some(10),
            output_tokens: None,
            context_tokens: None,
            context_window_percent: None,
        });
        // usage enrichment with no status change
        let changed = changed_fields(Some(&prior), &next, None);
        assert_eq!(changed, vec!["usage".to_owned()]);
        next.status = PublicStatus::Idle;
        next.activity = Activity::Idle;
        let changed = changed_fields(Some(&prior), &next, None);
        assert!(changed.contains(&"status".to_owned()));
        assert!(changed.contains(&"activity".to_owned()));
        // identical state reports nothing
        let same = changed_fields(Some(&prior), &base, None);
        assert!(same.is_empty());
        // unknown prior reports every canonical field
        let fresh = changed_fields(None, &base, None);
        assert_eq!(fresh.len(), CANONICAL_FIELDS.len());
    }
}
