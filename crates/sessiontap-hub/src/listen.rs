use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use sessiontap_core::domain::{PublicAgentView, PublicField};
use std::collections::BTreeSet;
use std::sync::Arc;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    sync::broadcast,
};

use crate::ingest::HubPublication;
use crate::store::{HubStore, MergedAgent, SourceView};

/// One merged live envelope per accepted update, after the initial baseline.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HubStreamEnvelope {
    Snapshot {
        hub_revision: u64,
        sources: Vec<SourceView>,
        agents: Vec<MergedAgent>,
    },
    Update {
        hub_revision: u64,
        source_id: String,
        delivery_id: String,
        source_revision: u64,
        changed: BTreeSet<PublicField>,
        view: Box<PublicAgentView>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HubRequest {
    Listen,
}

/// Serves one merged live consumer: subscribes before reading the persisted
/// baseline so updates cannot be lost across the snapshot boundary, then
/// emits gap-free publications after the baseline revision. Source snapshot
/// applications re-baseline the consumer from the persisted merged view.
pub async fn serve_listener(
    stream: UnixStream,
    store: Arc<HubStore>,
    mut receiver: broadcast::Receiver<HubPublication>,
) -> Result<()> {
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    let Some(line) = lines.next_line().await? else {
        return Ok(());
    };
    let request: HubRequest = serde_json::from_str(&line)?;
    if !matches!(request, HubRequest::Listen) {
        bail!("hub listener accepts only listen requests");
    }
    let (mut since, sources, agents) = store.merged()?;
    write_json(
        &mut write,
        &HubStreamEnvelope::Snapshot {
            hub_revision: since,
            sources,
            agents,
        },
    )
    .await?;
    loop {
        tokio::select! {
            incoming = lines.next_line() => match incoming {
                Ok(None) => break,
                Ok(Some(_)) => bail!("hub listener connection accepts only one request"),
                Err(error) => return Err(error.into()),
            },
            received = receiver.recv() => match received {
                Ok(HubPublication::Update(update)) if update.hub_revision > since => {
                    since = update.hub_revision;
                    write_json(
                        &mut write,
                        &HubStreamEnvelope::Update {
                            hub_revision: update.hub_revision,
                            source_id: update.source_id,
                            delivery_id: update.delivery_id,
                            source_revision: update.source_revision,
                            changed: update.changed,
                            view: Box::new(update.view),
                        },
                    )
                    .await?;
                }
                Ok(HubPublication::SnapshotApplied { hub_revision }) if hub_revision > since => {
                    let (hub_revision, sources, agents) = store.merged()?;
                    since = hub_revision;
                    write_json(
                        &mut write,
                        &HubStreamEnvelope::Snapshot {
                            hub_revision,
                            sources,
                            agents,
                        },
                    )
                    .await?;
                }
                Ok(_) => {}
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    let (hub_revision, sources, agents) = store.merged()?;
                    since = hub_revision;
                    write_json(
                        &mut write,
                        &HubStreamEnvelope::Snapshot {
                            hub_revision,
                            sources,
                            agents,
                        },
                    )
                    .await?;
                }
                Err(_) => break,
            }
        }
    }
    Ok(())
}

async fn write_json<T: serde::Serialize>(
    write: &mut tokio::net::unix::OwnedWriteHalf,
    value: &T,
) -> Result<()> {
    write.write_all(&serde_json::to_vec(value)?).await?;
    write.write_all(b"\n").await?;
    Ok(())
}
