#![allow(dead_code)]

use anyhow::{Result, bail};
use chrono::Utc;
use sessiontap_adapters::AdapterRegistry;
use sessiontap_core::{
    config::{Config, DaemonConfig},
    domain::{
        Activity, ActivityConfirmation, Capabilities, EventEvidence, EventKind, InvocationId,
        InvocationSnapshot, Lifecycle, MultiplexerMetadata, NormalizedEvent, ProcessMetadata,
        derive_status,
    },
    multiplexer::MultiplexerAdapter,
    protocol::{Request, StreamEnvelope},
};
use sessiontap_storage::Storage;
use sessiontapd::app::{App, Collection, PublishConfig};
use std::sync::Arc;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
};

pub struct NoMultiplexer;

impl MultiplexerAdapter for NoMultiplexer {
    fn inspect(&self) -> Result<Option<MultiplexerMetadata>> {
        Ok(None)
    }
    fn capture(&self, _: &MultiplexerMetadata, _: u32) -> Result<String> {
        bail!("no multiplexer")
    }
    fn send_input(&self, _: &MultiplexerMetadata, _: u32, _: &[u8]) -> Result<()> {
        bail!("no multiplexer")
    }
}

pub fn app_with(storage: Storage, daemon: &DaemonConfig) -> App {
    App::new(
        Arc::new(storage),
        PublishConfig::default(),
        daemon,
        Arc::new(NoMultiplexer),
        Collection {
            home: "/nonexistent".into(),
            registry: Arc::new(AdapterRegistry::new(&Config::default())),
        },
    )
}

pub fn app(storage: Storage) -> App {
    app_with(storage, &DaemonConfig::default())
}

pub fn snapshot() -> InvocationSnapshot {
    let now = Utc::now();
    InvocationSnapshot {
        schema_version: sessiontap_core::SCHEMA_VERSION,
        revision: 0,
        invocation_id: InvocationId::new(),
        provider: "claude".into(),
        executable: "claude".into(),
        args: vec![],
        cwd: "/tmp".into(),
        process: ProcessMetadata::default(),
        created_at: now,
        updated_at: now,
        lifecycle: Lifecycle::Starting,
        activity: Activity::Idle,
        state_started_at: now,
        last_state_asserted_at: None,
        activity_confirmation: ActivityConfirmation::Live,
        last_evidence: None,
        source_ordering: vec![],
        current_tool_activity: None,
        status: derive_status(Lifecycle::Starting, Activity::Idle),
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

pub fn event(initial: &InvocationSnapshot, id: &str, kind: EventKind) -> NormalizedEvent {
    NormalizedEvent {
        schema_version: 1,
        event_id: id.into(),
        invocation_id: initial.invocation_id.clone(),
        provider_event_id: None,
        provider: initial.provider.clone(),
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

/// Connects a client to a spawned `handle` over a socket pair.
pub fn connected_handle(app: App) -> (UnixStream, tokio::task::JoinHandle<Result<()>>) {
    let (client, server) = UnixStream::pair().unwrap();
    let task = tokio::spawn(sessiontapd::server::handle(server, app));
    (client, task)
}

pub async fn send_request(stream: &mut UnixStream, request: &Request) {
    stream
        .write_all(&serde_json::to_vec(request).unwrap())
        .await
        .unwrap();
    stream.write_all(b"\n").await.unwrap();
}

pub async fn read_line<T: serde::de::DeserializeOwned>(reader: &mut BufReader<UnixStream>) -> T {
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    serde_json::from_str(&line).unwrap()
}

pub async fn read_stream_line(reader: &mut BufReader<UnixStream>) -> StreamEnvelope {
    read_line(reader).await
}
