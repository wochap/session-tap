use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use sessiontap_core::domain::{PublicAgentView, PublicField};
use sessiontap_infra::json::write_json_line;
use std::collections::BTreeSet;
use std::sync::Arc;
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncWrite, BufReader, Lines},
    net::UnixStream,
    sync::broadcast,
};
use tokio_util::sync::CancellationToken;

use crate::ingest::HubPublication;
use crate::store::{Device, HubStore, MergedAgent, SourceView};

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

/// One request per unix connection. `Listen` streams; `Pair` is a short
/// conversation; every other request gets one `HubResponse` line.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HubRequest {
    Listen,
    Pair {
        scopes: Vec<String>,
    },
    Accept {
        accept: bool,
    },
    Devices,
    Revoke {
        device: String,
    },
    Forget {
        source_id: String,
        invocation_id: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HubResponse {
    Devices {
        devices: Vec<Device>,
        /// Whether `remote.control` makes terminal scopes effective.
        #[serde(default)]
        control: bool,
    },
    Revoked {
        device: Device,
    },
    Forgotten {
        hub_revision: u64,
    },
    PairWindow {
        payload: String,
        expires_at: i64,
    },
    PairConfirm {
        name: String,
        fingerprint: String,
    },
    PairDone {
        device_id: String,
        name: String,
    },
    PairFailed {
        reason: String,
    },
    Error {
        code: String,
        message: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        matches: Vec<Device>,
    },
}

impl HubResponse {
    pub fn error(code: &str, message: impl Into<String>) -> Self {
        Self::Error {
            code: code.into(),
            message: message.into(),
            matches: Vec::new(),
        }
    }
}

/// Destination for merged stream envelopes.
pub trait StreamSink {
    fn send(
        &mut self,
        envelope: &HubStreamEnvelope,
    ) -> impl std::future::Future<Output = Result<()>> + Send;
}

/// Writes each envelope as one JSON line.
pub struct JsonLinesSink<W>(pub W);

impl<W: AsyncWrite + Unpin + Send> StreamSink for JsonLinesSink<W> {
    async fn send(&mut self, envelope: &HubStreamEnvelope) -> Result<()> {
        write_json_line(&mut self.0, envelope).await?;
        Ok(())
    }
}

/// Serves one merged live consumer on a unix socket after reading its
/// `Listen` request.
pub async fn serve_listener(
    stream: UnixStream,
    store: Arc<HubStore>,
    receiver: broadcast::Receiver<HubPublication>,
) -> Result<()> {
    let (read, write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    let Some(line) = lines.next_line().await? else {
        return Ok(());
    };
    let request: HubRequest = serde_json::from_str(&line)?;
    if !matches!(request, HubRequest::Listen) {
        bail!("hub listener accepts only listen requests");
    }
    serve_unix_stream(lines, write, store, receiver).await
}

/// Streams to a unix consumer whose `Listen` request was already read. The
/// consumer closing its side ends the stream; any further input is an error.
pub async fn serve_unix_stream<R, W>(
    mut lines: Lines<BufReader<R>>,
    write: W,
    store: Arc<HubStore>,
    receiver: broadcast::Receiver<HubPublication>,
) -> Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin + Send,
{
    let cancel = CancellationToken::new();
    let watch = async {
        match lines.next_line().await {
            Ok(None) => Ok(()),
            Ok(Some(_)) => Err(anyhow::anyhow!(
                "hub listener connection accepts only one request"
            )),
            Err(error) => Err(error.into()),
        }
    };
    tokio::select! {
        result = stream_merged(JsonLinesSink(write), store, receiver, cancel.clone()) => result,
        result = watch => result,
    }
}

/// Emits a persisted merged baseline, then gap-free publications after the
/// baseline revision. The receiver must be subscribed before calling so
/// updates cannot be lost across the snapshot boundary. Source snapshot
/// applications, forgets, and receiver lag re-baseline the consumer from the
/// persisted merged view. Returns when `cancel` fires or the channel closes.
pub async fn stream_merged<S: StreamSink>(
    mut sink: S,
    store: Arc<HubStore>,
    mut receiver: broadcast::Receiver<HubPublication>,
    cancel: CancellationToken,
) -> Result<()> {
    let mut since = send_baseline(&mut sink, &store).await?;
    loop {
        tokio::select! {
            () = cancel.cancelled() => break,
            received = receiver.recv() => match received {
                Ok(HubPublication::Update(update)) if update.hub_revision > since => {
                    since = update.hub_revision;
                    sink.send(&HubStreamEnvelope::Update {
                        hub_revision: update.hub_revision,
                        source_id: update.source_id,
                        delivery_id: update.delivery_id,
                        source_revision: update.source_revision,
                        changed: update.changed,
                        view: Box::new(update.view),
                    })
                    .await?;
                }
                Ok(HubPublication::SnapshotApplied { hub_revision }) if hub_revision > since => {
                    since = send_baseline(&mut sink, &store).await?;
                }
                Ok(_) => {}
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    since = send_baseline(&mut sink, &store).await?;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    }
    Ok(())
}

async fn send_baseline<S: StreamSink>(sink: &mut S, store: &HubStore) -> Result<u64> {
    let (hub_revision, sources, agents) = store.merged()?;
    sink.send(&HubStreamEnvelope::Snapshot {
        hub_revision,
        sources,
        agents,
    })
    .await?;
    Ok(hub_revision)
}
