use anyhow::Result;
use chrono::{Duration, Utc};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use sessiontap_core::{
    domain::{
        InvocationId, PublicAgentView, PublicField, PublicReasonKind, PublicStatus,
        STATUS_REASON_MAX_BYTES, STATUS_REASON_MAX_CHARS, changed_public_fields,
    },
    protocol::{HUB_SCHEMA_VERSION, SourceEnvelope},
};
use std::{
    collections::{BTreeSet, HashSet},
    path::Path,
    sync::Mutex,
};

const MIGRATION: &str = r#"
CREATE TABLE IF NOT EXISTS hub_meta (key TEXT PRIMARY KEY, value INTEGER NOT NULL);
INSERT OR IGNORE INTO hub_meta(key,value) VALUES ('revision',0);
CREATE TABLE IF NOT EXISTS sources (
 source_id TEXT PRIMARY KEY, display_name TEXT, source_revision INTEGER NOT NULL DEFAULT 0,
 updated_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS public_agents (
 source_id TEXT NOT NULL REFERENCES sources(source_id) ON DELETE CASCADE,
 invocation_id TEXT NOT NULL, view_json TEXT NOT NULL, updated_at TEXT NOT NULL,
 stopped_at TEXT, PRIMARY KEY (source_id, invocation_id)
);
CREATE TABLE IF NOT EXISTS accepted_deliveries (
 source_id TEXT NOT NULL, delivery_id TEXT NOT NULL, accepted_at TEXT NOT NULL,
 PRIMARY KEY (source_id, delivery_id)
);
CREATE TABLE IF NOT EXISTS devices (
 device_id TEXT PRIMARY KEY, spki_sha256 TEXT UNIQUE NOT NULL, name TEXT NOT NULL,
 scopes TEXT NOT NULL, paired_at TEXT NOT NULL, last_seen_at TEXT
);
CREATE TABLE IF NOT EXISTS forgotten_agents (
 source_id TEXT NOT NULL, invocation_id TEXT NOT NULL, forgotten_at TEXT NOT NULL,
 PRIMARY KEY (source_id, invocation_id)
);
"#;

pub struct HubStore {
    conn: Mutex<Connection>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceView {
    pub source_id: String,
    pub display_name: Option<String>,
    pub revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergedAgent {
    pub source_id: String,
    pub view: PublicAgentView,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reject {
    UnsupportedVersion(u32),
    Malformed(String),
    SnapshotRequired,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapshotAccept {
    Applied { hub_revision: u64 },
    Stale,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateAccept {
    Applied {
        hub_revision: u64,
        changed: BTreeSet<PublicField>,
        first_seen: bool,
    },
    Duplicate,
    Stale,
    /// The invocation is tombstoned: the delivery is acknowledged and its
    /// source revision recorded, but nothing is persisted or published.
    Suppressed,
}

/// A paired remote device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Device {
    pub device_id: String,
    pub spki_sha256: String,
    pub name: String,
    pub scopes: Vec<String>,
    pub paired_at: String,
    pub last_seen_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceLookup {
    Found(Device),
    NotFound,
    Ambiguous(Vec<Device>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForgetOutcome {
    Forgotten { hub_revision: u64 },
    NotFound,
    NotStopped,
}

impl HubStore {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = sessiontap_infra::sqlite::open_private_sqlite(path)?;
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
        Ok(self
            .conn
            .lock()
            .expect("hub store mutex poisoned")
            .query_row("SELECT value FROM hub_meta WHERE key='revision'", [], |r| {
                r.get(0)
            })?)
    }

    pub fn merged(&self) -> Result<(u64, Vec<SourceView>, Vec<MergedAgent>)> {
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
            "SELECT source_id,view_json FROM public_agents ORDER BY source_id,invocation_id",
        )?;
        let agents = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .map(|row| {
                let (source_id, raw) = row?;
                Ok(MergedAgent {
                    source_id,
                    view: serde_json::from_str(&raw)?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok((revision, sources, agents))
    }

    fn prior(
        tx: &Transaction<'_>,
        source_id: &str,
        id: &InvocationId,
    ) -> Result<Option<PublicAgentView>> {
        let raw: Option<String> = tx
            .query_row(
                "SELECT view_json FROM public_agents WHERE source_id=?1 AND invocation_id=?2",
                params![source_id, id.to_string()],
                |r| r.get(0),
            )
            .optional()?;
        raw.map(|raw| serde_json::from_str(&raw).map_err(Into::into))
            .transpose()
    }

    pub fn ingest_snapshot(
        &self,
        envelope: &SourceEnvelope,
    ) -> std::result::Result<SnapshotAccept, Reject> {
        let SourceEnvelope::Snapshot {
            schema_version,
            source,
            revision,
            views,
        } = envelope
        else {
            return Err(Reject::Malformed("expected snapshot envelope".into()));
        };
        validate_version(*schema_version)?;
        if source.id.is_empty() {
            return Err(Reject::Malformed("empty source identity".into()));
        }
        if views.iter().any(|view| !valid_public_reason(view)) {
            return Err(Reject::Malformed(
                "invalid or status-incompatible public reason".into(),
            ));
        }
        let mut ids = HashSet::new();
        if views.iter().any(|v| !ids.insert(v.invocation_id.clone())) {
            return Err(Reject::Malformed("duplicate invocation in snapshot".into()));
        }
        let mut conn = self.conn.lock().expect("hub store mutex poisoned");
        let tx = conn.transaction().map_err(malformed)?;
        let known: Option<u64> = tx
            .query_row(
                "SELECT source_revision FROM sources WHERE source_id=?1",
                [&source.id],
                |r| r.get(0),
            )
            .optional()
            .map_err(malformed)?;
        if known.is_some_and(|current| *revision <= current) {
            return Ok(SnapshotAccept::Stale);
        }
        let now = Utc::now().to_rfc3339();
        tx.execute("INSERT INTO sources(source_id,display_name,source_revision,updated_at) VALUES (?1,?2,?3,?4) ON CONFLICT(source_id) DO UPDATE SET display_name=excluded.display_name,source_revision=excluded.source_revision,updated_at=excluded.updated_at", params![source.id, source.display_name, revision, now]).map_err(malformed)?;
        tx.execute("DELETE FROM public_agents WHERE source_id=?1", [&source.id])
            .map_err(malformed)?;
        for view in views {
            if !is_forgotten(&tx, &source.id, &view.invocation_id).map_err(malformed)? {
                persist_view(&tx, &source.id, view, &now).map_err(malformed)?;
            }
        }
        let hub_revision = bump_revision(&tx).map_err(malformed)?;
        tx.commit().map_err(malformed)?;
        Ok(SnapshotAccept::Applied { hub_revision })
    }

    pub fn ingest_update(
        &self,
        envelope: &SourceEnvelope,
    ) -> std::result::Result<UpdateAccept, Reject> {
        let SourceEnvelope::Update {
            schema_version,
            source_id,
            delivery_id,
            revision,
            changed,
            view,
        } = envelope
        else {
            return Err(Reject::Malformed("expected update envelope".into()));
        };
        validate_version(*schema_version)?;
        if source_id.is_empty() || delivery_id.is_empty() || changed.is_empty() {
            return Err(Reject::Malformed(
                "missing delivery identity or changed fields".into(),
            ));
        }
        if !valid_public_reason(view) {
            return Err(Reject::Malformed(
                "invalid or status-incompatible public reason".into(),
            ));
        }
        let mut conn = self.conn.lock().expect("hub store mutex poisoned");
        let tx = conn.transaction().map_err(malformed)?;
        let source_revision: Option<u64> = tx
            .query_row(
                "SELECT source_revision FROM sources WHERE source_id=?1",
                [source_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(malformed)?;
        let Some(source_revision) = source_revision else {
            return Err(Reject::SnapshotRequired);
        };
        if tx
            .query_row(
                "SELECT 1 FROM accepted_deliveries WHERE source_id=?1 AND delivery_id=?2",
                params![source_id, delivery_id],
                |_| Ok(()),
            )
            .optional()
            .map_err(malformed)?
            .is_some()
        {
            return Ok(UpdateAccept::Duplicate);
        }
        if *revision <= source_revision {
            return Ok(UpdateAccept::Stale);
        }
        let now = Utc::now().to_rfc3339();
        if is_forgotten(&tx, source_id, &view.invocation_id).map_err(malformed)? {
            record_delivery(&tx, source_id, delivery_id, *revision, &now).map_err(malformed)?;
            tx.commit().map_err(malformed)?;
            return Ok(UpdateAccept::Suppressed);
        }
        let prior = Self::prior(&tx, source_id, &view.invocation_id).map_err(malformed)?;
        let actual = changed_public_fields(prior.as_ref(), view);
        if actual.is_empty() {
            return Err(Reject::Malformed(
                "update does not change the public view".into(),
            ));
        }
        persist_view(&tx, source_id, view, &now).map_err(malformed)?;
        record_delivery(&tx, source_id, delivery_id, *revision, &now).map_err(malformed)?;
        let hub_revision = bump_revision(&tx).map_err(malformed)?;
        tx.commit().map_err(malformed)?;
        Ok(UpdateAccept::Applied {
            hub_revision,
            changed: actual,
            first_seen: prior.is_none(),
        })
    }

    pub fn prune_retained(&self, retention_days: u64) -> Result<usize> {
        let cutoff = (Utc::now()
            - Duration::days(i64::try_from(retention_days).unwrap_or(i64::MAX)))
        .to_rfc3339();
        let conn = self.conn.lock().expect("hub store mutex poisoned");
        let agents = conn.execute(
            "DELETE FROM public_agents WHERE stopped_at IS NOT NULL AND stopped_at < ?1",
            [&cutoff],
        )?;
        let deliveries = conn.execute(
            "DELETE FROM accepted_deliveries WHERE accepted_at < ?1",
            [&cutoff],
        )?;
        conn.execute("DELETE FROM sources WHERE source_id NOT IN (SELECT DISTINCT source_id FROM public_agents)", [])?;
        let tombstone_cutoff = (Utc::now()
            - Duration::days(i64::try_from(retention_days.saturating_mul(2)).unwrap_or(i64::MAX)))
        .to_rfc3339();
        let tombstones = conn.execute(
            "DELETE FROM forgotten_agents WHERE forgotten_at < ?1",
            [&tombstone_cutoff],
        )?;
        Ok(agents + deliveries + tombstones)
    }

    /// Deletes a stopped agent and tombstones its invocation so later
    /// deliveries for it are suppressed. Bumps the hub revision.
    pub fn forget(&self, source_id: &str, invocation_id: &str) -> Result<ForgetOutcome> {
        let mut conn = self.conn.lock().expect("hub store mutex poisoned");
        let tx = conn.transaction()?;
        let raw: Option<String> = tx
            .query_row(
                "SELECT view_json FROM public_agents WHERE source_id=?1 AND invocation_id=?2",
                params![source_id, invocation_id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(raw) = raw else {
            return Ok(ForgetOutcome::NotFound);
        };
        let view: PublicAgentView = serde_json::from_str(&raw)?;
        if view.status != PublicStatus::Stopped {
            return Ok(ForgetOutcome::NotStopped);
        }
        tx.execute(
            "DELETE FROM public_agents WHERE source_id=?1 AND invocation_id=?2",
            params![source_id, invocation_id],
        )?;
        tx.execute(
            "INSERT OR REPLACE INTO forgotten_agents(source_id,invocation_id,forgotten_at) VALUES (?1,?2,?3)",
            params![source_id, invocation_id, Utc::now().to_rfc3339()],
        )?;
        let hub_revision = bump_revision(&tx)?;
        tx.commit()?;
        Ok(ForgetOutcome::Forgotten { hub_revision })
    }

    /// Inserts a device, or refreshes name and scopes when its SPKI is
    /// already paired. Returns the stored device.
    pub fn upsert_device(
        &self,
        device_id: &str,
        spki_sha256: &str,
        name: &str,
        scopes: &[String],
    ) -> Result<Device> {
        let conn = self.conn.lock().expect("hub store mutex poisoned");
        conn.execute(
            "INSERT INTO devices(device_id,spki_sha256,name,scopes,paired_at) VALUES (?1,?2,?3,?4,?5) ON CONFLICT(spki_sha256) DO UPDATE SET name=excluded.name,scopes=excluded.scopes,paired_at=excluded.paired_at",
            params![device_id, spki_sha256, name, scopes.join(","), Utc::now().to_rfc3339()],
        )?;
        Ok(conn.query_row(
            &format!("{DEVICE_COLUMNS} WHERE spki_sha256=?1"),
            [spki_sha256],
            device_row,
        )?)
    }

    pub fn devices(&self) -> Result<Vec<Device>> {
        let conn = self.conn.lock().expect("hub store mutex poisoned");
        let mut stmt = conn.prepare(&format!("{DEVICE_COLUMNS} ORDER BY paired_at,device_id"))?;
        Ok(stmt
            .query_map([], device_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn device_by_spki(&self, spki_sha256: &str) -> Result<Option<Device>> {
        let conn = self.conn.lock().expect("hub store mutex poisoned");
        Ok(conn
            .query_row(
                &format!("{DEVICE_COLUMNS} WHERE spki_sha256=?1"),
                [spki_sha256],
                device_row,
            )
            .optional()?)
    }

    /// Resolves a device ID or unique ID prefix.
    pub fn find_device(&self, prefix: &str) -> Result<DeviceLookup> {
        if prefix.is_empty() {
            return Ok(DeviceLookup::NotFound);
        }
        let mut matches: Vec<Device> = self
            .devices()?
            .into_iter()
            .filter(|device| device.device_id.starts_with(prefix))
            .collect();
        if let Some(exact) = matches.iter().position(|d| d.device_id == prefix) {
            return Ok(DeviceLookup::Found(matches.swap_remove(exact)));
        }
        Ok(match matches.len() {
            0 => DeviceLookup::NotFound,
            1 => DeviceLookup::Found(matches.remove(0)),
            _ => DeviceLookup::Ambiguous(matches),
        })
    }

    /// Deletes the device matching an ID or unique prefix.
    pub fn delete_device(&self, prefix: &str) -> Result<DeviceLookup> {
        let lookup = self.find_device(prefix)?;
        if let DeviceLookup::Found(device) = &lookup {
            self.conn
                .lock()
                .expect("hub store mutex poisoned")
                .execute(
                    "DELETE FROM devices WHERE device_id=?1",
                    [&device.device_id],
                )?;
        }
        Ok(lookup)
    }

    pub fn touch_device(&self, device_id: &str) -> Result<()> {
        self.conn
            .lock()
            .expect("hub store mutex poisoned")
            .execute(
                "UPDATE devices SET last_seen_at=?2 WHERE device_id=?1",
                params![device_id, Utc::now().to_rfc3339()],
            )?;
        Ok(())
    }

    pub fn has_source(&self, source_id: &str) -> Result<bool> {
        Ok(self
            .conn
            .lock()
            .expect("hub store mutex poisoned")
            .query_row(
                "SELECT 1 FROM sources WHERE source_id=?1",
                [source_id],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }
}

const DEVICE_COLUMNS: &str =
    "SELECT device_id,spki_sha256,name,scopes,paired_at,last_seen_at FROM devices";

fn device_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Device> {
    let scopes: String = r.get(3)?;
    Ok(Device {
        device_id: r.get(0)?,
        spki_sha256: r.get(1)?,
        name: r.get(2)?,
        scopes: scopes
            .split(',')
            .filter(|scope| !scope.is_empty())
            .map(str::to_owned)
            .collect(),
        paired_at: r.get(4)?,
        last_seen_at: r.get(5)?,
    })
}

fn is_forgotten(tx: &Transaction<'_>, source_id: &str, id: &InvocationId) -> Result<bool> {
    Ok(tx
        .query_row(
            "SELECT 1 FROM forgotten_agents WHERE source_id=?1 AND invocation_id=?2",
            params![source_id, id.to_string()],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

fn record_delivery(
    tx: &Transaction<'_>,
    source_id: &str,
    delivery_id: &str,
    revision: u64,
    now: &str,
) -> Result<()> {
    tx.execute(
        "UPDATE sources SET source_revision=?2,updated_at=?3 WHERE source_id=?1",
        params![source_id, revision, now],
    )?;
    tx.execute(
        "INSERT INTO accepted_deliveries(source_id,delivery_id,accepted_at) VALUES (?1,?2,?3)",
        params![source_id, delivery_id, now],
    )?;
    Ok(())
}

fn validate_version(version: u32) -> std::result::Result<(), Reject> {
    if version == HUB_SCHEMA_VERSION {
        Ok(())
    } else {
        Err(Reject::UnsupportedVersion(version))
    }
}

fn valid_public_reason(view: &PublicAgentView) -> bool {
    let Some(reason) = &view.reason else {
        return true;
    };
    if reason.summary.is_empty()
        || reason.summary.len() > STATUS_REASON_MAX_BYTES
        || reason.summary.chars().count() > STATUS_REASON_MAX_CHARS
        || reason.summary.chars().any(char::is_control)
    {
        return false;
    }
    matches!(
        (view.status, reason.kind),
        (
            PublicStatus::Blocked,
            PublicReasonKind::Input | PublicReasonKind::Approval
        ) | (
            PublicStatus::Stopped,
            PublicReasonKind::Completed | PublicReasonKind::Failed
        )
    )
}
fn malformed(error: impl std::fmt::Display) -> Reject {
    Reject::Malformed(error.to_string())
}
fn bump_revision(tx: &Transaction<'_>) -> Result<u64> {
    tx.execute("UPDATE hub_meta SET value=value+1 WHERE key='revision'", [])?;
    Ok(
        tx.query_row("SELECT value FROM hub_meta WHERE key='revision'", [], |r| {
            r.get(0)
        })?,
    )
}
fn persist_view(
    tx: &Transaction<'_>,
    source_id: &str,
    view: &PublicAgentView,
    now: &str,
) -> Result<()> {
    let stopped = (view.status == PublicStatus::Stopped).then(|| view.updated_at.to_rfc3339());
    tx.execute("INSERT INTO public_agents(source_id,invocation_id,view_json,updated_at,stopped_at) VALUES (?1,?2,?3,?4,?5) ON CONFLICT(source_id,invocation_id) DO UPDATE SET view_json=excluded.view_json,updated_at=excluded.updated_at,stopped_at=excluded.stopped_at", params![source_id, view.invocation_id.to_string(), serde_json::to_string(view)?, now, stopped])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use sessiontap_core::{domain::PublicStatus, protocol::SourceIdentity};

    fn view(id: &str) -> PublicAgentView {
        PublicAgentView {
            invocation_id: id.parse().unwrap(),
            provider: "codex".into(),
            status: PublicStatus::Idle,
            reason: None,
            cwd: "/tmp".into(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            session: None,
            metadata: None,
            usage: None,
            repository: None,
            children: None,
        }
    }
    fn snapshot(source: &str, revision: u64, views: Vec<PublicAgentView>) -> SourceEnvelope {
        SourceEnvelope::Snapshot {
            schema_version: 1,
            source: SourceIdentity {
                id: source.into(),
                display_name: None,
            },
            revision,
            views,
        }
    }
    #[test]
    fn source_snapshot_repair_is_scoped() {
        let store = HubStore::memory().unwrap();
        store
            .ingest_snapshot(&snapshot(
                "a",
                1,
                vec![view("00000000-0000-4000-8000-000000000001")],
            ))
            .unwrap();
        store
            .ingest_snapshot(&snapshot(
                "b",
                1,
                vec![view("00000000-0000-4000-8000-000000000001")],
            ))
            .unwrap();
        store.ingest_snapshot(&snapshot("a", 2, vec![])).unwrap();
        let (_, _, agents) = store.merged().unwrap();
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].source_id, "b");
    }

    #[test]
    fn stale_updates_do_not_replace_public_reason() {
        use sessiontap_core::domain::{PublicReasonKind, PublicStatusReason};
        let store = HubStore::memory().unwrap();
        let mut blocked = view("00000000-0000-4000-8000-000000000001");
        blocked.status = PublicStatus::Blocked;
        blocked.reason = Some(PublicStatusReason {
            kind: PublicReasonKind::Approval,
            summary: "Approve".into(),
        });
        store
            .ingest_snapshot(&snapshot("a", 5, vec![blocked.clone()]))
            .unwrap();
        let mut input = blocked;
        input.reason = Some(PublicStatusReason {
            kind: PublicReasonKind::Input,
            summary: "Choose".into(),
        });
        let stale = SourceEnvelope::Update {
            schema_version: 1,
            source_id: "a".into(),
            delivery_id: "stale".into(),
            revision: 4,
            changed: BTreeSet::from([PublicField::Reason]),
            view: Box::new(input),
        };
        assert_eq!(store.ingest_update(&stale).unwrap(), UpdateAccept::Stale);
        let (_, _, agents) = store.merged().unwrap();
        assert_eq!(
            agents[0].view.reason.as_ref().unwrap().kind,
            PublicReasonKind::Approval
        );
    }

    #[test]
    fn complete_views_replace_and_clear_reasons() {
        use sessiontap_core::domain::{PublicReasonKind, PublicStatusReason};
        let store = HubStore::memory().unwrap();
        let mut approval = view("00000000-0000-4000-8000-000000000001");
        approval.status = PublicStatus::Blocked;
        approval.reason = Some(PublicStatusReason {
            kind: PublicReasonKind::Approval,
            summary: "Approve".into(),
        });
        store
            .ingest_snapshot(&snapshot("a", 1, vec![approval.clone()]))
            .unwrap();
        let mut input = approval;
        input.reason = Some(PublicStatusReason {
            kind: PublicReasonKind::Input,
            summary: "Choose".into(),
        });
        input.updated_at = Utc::now();
        let replace = SourceEnvelope::Update {
            schema_version: 1,
            source_id: "a".into(),
            delivery_id: "replace".into(),
            revision: 2,
            changed: BTreeSet::from([PublicField::Reason]),
            view: Box::new(input.clone()),
        };
        assert!(matches!(
            store.ingest_update(&replace).unwrap(),
            UpdateAccept::Applied { .. }
        ));
        input.status = PublicStatus::Running;
        input.reason = None;
        input.updated_at = Utc::now();
        let clear = SourceEnvelope::Update {
            schema_version: 1,
            source_id: "a".into(),
            delivery_id: "clear".into(),
            revision: 3,
            changed: BTreeSet::from([PublicField::Status, PublicField::Reason]),
            view: Box::new(input),
        };
        assert!(matches!(
            store.ingest_update(&clear).unwrap(),
            UpdateAccept::Applied { .. }
        ));
        let (_, _, agents) = store.merged().unwrap();
        assert_eq!(agents[0].view.status, PublicStatus::Running);
        assert!(agents[0].view.reason.is_none());
    }

    #[test]
    fn stopped_completed_and_failed_reasons_round_trip_in_current_views() {
        use sessiontap_core::domain::{PublicReasonKind, PublicStatusReason};
        let store = HubStore::memory().unwrap();
        let mut completed = view("00000000-0000-4000-8000-000000000001");
        completed.status = PublicStatus::Stopped;
        completed.reason = Some(PublicStatusReason {
            kind: PublicReasonKind::Completed,
            summary: "Done".into(),
        });
        store
            .ingest_snapshot(&snapshot("a", 1, vec![completed.clone()]))
            .unwrap();
        assert_eq!(
            store.merged().unwrap().2[0]
                .view
                .reason
                .as_ref()
                .unwrap()
                .kind,
            PublicReasonKind::Completed
        );

        completed.reason = Some(PublicStatusReason {
            kind: PublicReasonKind::Failed,
            summary: "Timed out".into(),
        });
        completed.updated_at = Utc::now();
        let update = SourceEnvelope::Update {
            schema_version: 1,
            source_id: "a".into(),
            delivery_id: "failed".into(),
            revision: 2,
            changed: BTreeSet::from([PublicField::Reason]),
            view: Box::new(completed),
        };
        assert!(matches!(
            store.ingest_update(&update).unwrap(),
            UpdateAccept::Applied { .. }
        ));
        assert_eq!(
            store.merged().unwrap().2[0]
                .view
                .reason
                .as_ref()
                .unwrap()
                .kind,
            PublicReasonKind::Failed
        );
    }

    #[test]
    fn incompatible_or_unbounded_public_reasons_are_rejected() {
        use sessiontap_core::domain::{PublicReasonKind, PublicStatusReason};
        let store = HubStore::memory().unwrap();
        let mut invalid = view("00000000-0000-4000-8000-000000000001");
        invalid.status = PublicStatus::Stopped;
        invalid.reason = Some(PublicStatusReason {
            kind: PublicReasonKind::Input,
            summary: "Choose".into(),
        });
        assert!(matches!(
            store.ingest_snapshot(&snapshot("a", 1, vec![invalid.clone()])),
            Err(Reject::Malformed(_))
        ));
        invalid.reason = Some(PublicStatusReason {
            kind: PublicReasonKind::Completed,
            summary: "x".repeat(STATUS_REASON_MAX_CHARS + 1),
        });
        assert!(matches!(
            store.ingest_snapshot(&snapshot("a", 1, vec![invalid])),
            Err(Reject::Malformed(_))
        ));
    }

    fn stopped(id: &str) -> PublicAgentView {
        let mut stopped = view(id);
        stopped.status = PublicStatus::Stopped;
        stopped
    }

    const ID: &str = "00000000-0000-4000-8000-000000000001";

    fn update(delivery: &str, revision: u64, view: PublicAgentView) -> SourceEnvelope {
        SourceEnvelope::Update {
            schema_version: 1,
            source_id: "a".into(),
            delivery_id: delivery.into(),
            revision,
            changed: BTreeSet::from([PublicField::UpdatedAt]),
            view: Box::new(view),
        }
    }

    #[test]
    fn devices_insert_list_lookup_touch_and_delete() {
        let store = HubStore::memory().unwrap();
        let scopes = vec!["read".to_owned(), "manage".to_owned()];
        let phone = store
            .upsert_device("ab12", "spki1", "Pixel", &scopes)
            .unwrap();
        assert_eq!(phone.scopes, scopes);
        store
            .upsert_device("ab34", "spki2", "Tablet", &scopes[..1])
            .unwrap();
        let again = store
            .upsert_device("ffff", "spki1", "Pixel 9", &scopes[..1])
            .unwrap();
        assert_eq!(again.device_id, "ab12");
        assert_eq!(again.name, "Pixel 9");
        assert_eq!(store.devices().unwrap().len(), 2);
        assert_eq!(
            store.device_by_spki("spki2").unwrap().unwrap().device_id,
            "ab34"
        );
        assert!(store.device_by_spki("nope").unwrap().is_none());
        store.touch_device("ab12").unwrap();
        assert!(
            store
                .device_by_spki("spki1")
                .unwrap()
                .unwrap()
                .last_seen_at
                .is_some()
        );
        assert!(matches!(
            store.delete_device("ab").unwrap(),
            DeviceLookup::Ambiguous(matches) if matches.len() == 2
        ));
        assert_eq!(store.delete_device("zz").unwrap(), DeviceLookup::NotFound);
        assert!(
            matches!(store.delete_device("ab3").unwrap(), DeviceLookup::Found(d) if d.name == "Tablet")
        );
        assert_eq!(store.devices().unwrap().len(), 1);
    }

    #[test]
    fn forget_requires_existing_stopped_agent() {
        let store = HubStore::memory().unwrap();
        let mut running = view(ID);
        running.status = PublicStatus::Running;
        store
            .ingest_snapshot(&snapshot("a", 1, vec![running]))
            .unwrap();
        assert_eq!(store.forget("a", "nope").unwrap(), ForgetOutcome::NotFound);
        assert_eq!(store.forget("b", ID).unwrap(), ForgetOutcome::NotFound);
        let before = store.revision().unwrap();
        assert_eq!(store.forget("a", ID).unwrap(), ForgetOutcome::NotStopped);
        assert_eq!(store.revision().unwrap(), before);
        store
            .ingest_snapshot(&snapshot("a", 2, vec![stopped(ID)]))
            .unwrap();
        let ForgetOutcome::Forgotten { hub_revision } = store.forget("a", ID).unwrap() else {
            panic!("expected forgotten");
        };
        assert_eq!(hub_revision, store.revision().unwrap());
        assert!(store.merged().unwrap().2.is_empty());
    }

    #[test]
    fn tombstoned_deliveries_are_suppressed() {
        let store = HubStore::memory().unwrap();
        store
            .ingest_snapshot(&snapshot("a", 1, vec![stopped(ID)]))
            .unwrap();
        store.forget("a", ID).unwrap();
        let revision = store.revision().unwrap();
        let mut again = stopped(ID);
        again.updated_at = Utc::now() + Duration::seconds(1);
        let redelivered = update("late", 2, again.clone());
        assert_eq!(
            store.ingest_update(&redelivered).unwrap(),
            UpdateAccept::Suppressed
        );
        assert_eq!(
            store.ingest_update(&redelivered).unwrap(),
            UpdateAccept::Duplicate
        );
        assert_eq!(store.revision().unwrap(), revision);
        assert!(store.merged().unwrap().2.is_empty());
        // the suppressed delivery advanced the source revision
        assert_eq!(
            store.ingest_update(&update("older", 2, again)).unwrap(),
            UpdateAccept::Stale
        );
        let other = "00000000-0000-4000-8000-000000000002";
        store
            .ingest_snapshot(&snapshot("a", 3, vec![stopped(ID), view(other)]))
            .unwrap();
        let (_, _, agents) = store.merged().unwrap();
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].view.invocation_id.to_string(), other);
    }

    #[test]
    fn old_tombstones_are_pruned() {
        let store = HubStore::memory().unwrap();
        {
            let conn = store.conn.lock().unwrap();
            let old = (Utc::now() - Duration::days(15)).to_rfc3339();
            let recent = (Utc::now() - Duration::days(13)).to_rfc3339();
            conn.execute(
                "INSERT INTO forgotten_agents VALUES ('a','old',?1),('a','recent',?2)",
                params![old, recent],
            )
            .unwrap();
        }
        store.prune_retained(7).unwrap();
        let conn = store.conn.lock().unwrap();
        let left: Vec<String> = conn
            .prepare("SELECT invocation_id FROM forgotten_agents")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(left, vec!["recent".to_owned()]);
    }
}
