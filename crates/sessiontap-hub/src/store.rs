use anyhow::{Context, Result, bail};
use chrono::{Duration, Utc};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use sessiontap_core::{
    domain::{InvocationId, PublicAgentView, PublicField, PublicStatus, changed_public_fields},
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
"#;

pub const CANONICAL_FIELDS: &[&str] = &[
    "invocation_id",
    "provider",
    "status",
    "reason",
    "cwd",
    "created_at",
    "updated_at",
    "session",
    "metadata",
    "usage",
    "repository",
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
}

impl HubStore {
    pub fn open(path: &Path) -> Result<Self> {
        if path
            .symlink_metadata()
            .is_ok_and(|m| m.file_type().is_symlink())
        {
            bail!("database path must not be a symlink");
        }
        let conn = Connection::open(path).with_context(|| format!("open {}", path.display()))?;
        use std::{fs, os::unix::fs::PermissionsExt};
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
            persist_view(&tx, &source.id, view, &now).map_err(malformed)?;
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
        let prior = Self::prior(&tx, source_id, &view.invocation_id).map_err(malformed)?;
        let actual = changed_public_fields(prior.as_ref(), view);
        if actual.is_empty() {
            return Err(Reject::Malformed(
                "update does not change the public view".into(),
            ));
        }
        let now = Utc::now().to_rfc3339();
        persist_view(&tx, source_id, view, &now).map_err(malformed)?;
        tx.execute(
            "UPDATE sources SET source_revision=?2,updated_at=?3 WHERE source_id=?1",
            params![source_id, revision, now],
        )
        .map_err(malformed)?;
        tx.execute(
            "INSERT INTO accepted_deliveries(source_id,delivery_id,accepted_at) VALUES (?1,?2,?3)",
            params![source_id, delivery_id, now],
        )
        .map_err(malformed)?;
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
        Ok(agents + deliveries)
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

fn validate_version(version: u32) -> std::result::Result<(), Reject> {
    if version == HUB_SCHEMA_VERSION {
        Ok(())
    } else {
        Err(Reject::UnsupportedVersion(version))
    }
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
}
