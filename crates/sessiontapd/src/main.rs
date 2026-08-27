use anyhow::{Context, Result};
use fs2::FileExt;
use sessiontap_core::{
    SCHEMA_VERSION,
    config::{Config, SinkConfig},
    multiplexer::{MultiplexerAdapter, TmuxAdapter},
    paths::AppPaths,
    protocol::{ErrorEnvelope, Request, Response, StreamEnvelope},
};
use sessiontap_storage::{AppliedUpdate, Storage};
use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    os::unix::fs::PermissionsExt,
    path::Path,
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::broadcast,
};

#[derive(Clone)]
struct Broker {
    storage: Arc<Storage>,
    updates: broadcast::Sender<AppliedUpdate>,
    sinks: Arc<BTreeMap<String, SinkConfig>>,
    source_id: Arc<str>,
    source_name: Arc<Option<String>>,
}

impl Broker {
    fn publish(&self) -> sessiontap_storage::Publish<'_> {
        sessiontap_storage::Publish {
            sinks: &self.sinks,
            source_id: &self.source_id,
            source_name: self.source_name.as_deref(),
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let paths = AppPaths::discover()?;
    AppPaths::prepare_private(&paths.runtime_dir)?;
    AppPaths::prepare_private(&paths.state_dir)?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(paths.lock())?;
    lock.try_lock_exclusive()
        .context("sessiontapd is already running")?;
    let socket = paths.socket();
    if socket.exists() {
        if UnixStream::connect(&socket).await.is_ok() {
            anyhow::bail!("sessiontapd is already listening");
        }
        fs::remove_file(&socket).context("remove stale socket")?;
    }
    let listener = UnixListener::bind(&socket)?;
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
    let config = Config::load(&paths.config_file()).unwrap_or_else(|e| {
        eprintln!("sessiontapd: configuration disabled: {e}");
        Config::default()
    });
    if let Err(e) = config.validate() {
        anyhow::bail!("sessiontapd: invalid configuration: {e}");
    }
    let storage = Arc::new(Storage::open(&paths.database())?);
    let (updates, _) = broadcast::channel(1024);
    let broker = Broker {
        storage: storage.clone(),
        updates,
        sinks: Arc::new(config.sinks),
        source_id: Arc::from(config.source_id.unwrap_or_default()),
        source_name: Arc::new(config.source_name),
    };
    storage.reconcile(
        process_alive,
        config.retention_days,
        Some(&broker.publish()),
    )?;
    tokio::spawn(sink_worker(
        storage,
        broker.sinks.clone(),
        broker.source_id.clone(),
        broker.source_name.clone(),
    ));
    let shutdown = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    tokio::pin!(shutdown);
    loop {
        tokio::select! { biased; ()=&mut shutdown=>break, accepted=listener.accept()=> { let (stream,_)=accepted?; let b=broker.clone(); tokio::spawn(async move { let _=handle(stream,b).await; }); } }
    }
    let _ = fs::remove_file(&socket);
    drop(lock);
    Ok(())
}

async fn handle(stream: UnixStream, broker: Broker) -> Result<()> {
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    let Some(line) = lines.next_line().await? else {
        return Ok(());
    };
    let request: Request = serde_json::from_str(&line)?;
    if matches!(request, Request::Listen) {
        let mut rx = broker.updates.subscribe();
        let (revision, invocations, active_attention) = broker.storage.snapshot_with_attention()?;
        write_json(
            &mut write,
            &StreamEnvelope::Snapshot {
                schema_version: SCHEMA_VERSION,
                revision,
                invocations,
                active_attention,
            },
        )
        .await?;
        loop {
            tokio::select! {
                incoming = lines.next_line() => match incoming {
                    Ok(None) => break,
                    Ok(Some(_)) => anyhow::bail!("listener connection accepts only one request"),
                    Err(error) => return Err(error.into()),
                },
                received = rx.recv() => match received {
                    Ok(update) if update.snapshot.revision > revision => {
                        write_json(
                            &mut write,
                            &StreamEnvelope::Update {
                                schema_version: SCHEMA_VERSION,
                                revision: update.snapshot.revision,
                                snapshot: Box::new(update.snapshot),
                                event: Some(update.event),
                            },
                        )
                        .await?
                    }
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        let (r, s, active_attention) = broker.storage.snapshot_with_attention()?;
                        write_json(
                            &mut write,
                            &StreamEnvelope::Snapshot {
                                schema_version: SCHEMA_VERSION,
                                revision: r,
                                invocations: s,
                                active_attention,
                            },
                        )
                        .await?;
                    }
                    Err(_) => break,
                }
            }
        }
        return Ok(());
    }
    let response = match process(request, &broker) {
        Ok(r) => r,
        Err(e) => Response::Error(ErrorEnvelope {
            code: "request_failed".into(),
            message: e.to_string(),
        }),
    };
    write_json(&mut write, &response).await
}

fn process(request: Request, broker: &Broker) -> Result<Response> {
    match request {
        Request::Health => Ok(Response::Health {
            version: SCHEMA_VERSION,
        }),
        Request::Status => {
            let (revision, invocations) = broker.storage.snapshot()?;
            Ok(Response::Status {
                revision,
                invocations,
            })
        }
        Request::Register {
            snapshot,
            credential,
        } => {
            let publish = broker.publish();
            let revision = broker
                .storage
                .register(&snapshot, &credential, Some(&publish))?;
            let mut registered = *snapshot;
            registered.revision = revision;
            let _ = broker.updates.send(AppliedUpdate {
                snapshot: registered,
                event: sessiontap_core::domain::LiveEventMetadata {
                    kind: sessiontap_core::domain::EventKind::Enrichment,
                    attention: None,
                    failure: None,
                    turn_id: None,
                },
            });
            Ok(Response::Ok)
        }
        Request::BindChild {
            invocation_id,
            credential,
            child_pid,
            start_identity,
        } => {
            let publish = broker.publish();
            let s = broker.storage.bind_child(
                &invocation_id,
                &credential,
                child_pid,
                start_identity,
                Some(&publish),
            )?;
            let _ = broker.updates.send(AppliedUpdate {
                snapshot: s,
                event: sessiontap_core::domain::LiveEventMetadata {
                    kind: sessiontap_core::domain::EventKind::Enrichment,
                    attention: None,
                    failure: None,
                    turn_id: None,
                },
            });
            Ok(Response::Ok)
        }
        Request::LifecycleExit {
            invocation_id,
            credential,
            exit_code,
            signal,
        } => {
            let publish = broker.publish();
            let s = broker.storage.mark_exit(
                &invocation_id,
                &credential,
                exit_code,
                signal,
                Some(&publish),
            )?;
            let _ = broker.updates.send(AppliedUpdate {
                snapshot: s,
                event: sessiontap_core::domain::LiveEventMetadata {
                    kind: sessiontap_core::domain::EventKind::SessionEnded,
                    attention: None,
                    failure: None,
                    turn_id: None,
                },
            });
            Ok(Response::Ok)
        }
        Request::HookIngest {
            provider,
            invocation_id,
            credential,
            event,
            attention,
            failure,
        } => {
            if event.invocation_id != invocation_id
                || event.provider != provider
                || !broker
                    .storage
                    .credential_matches(&invocation_id, &provider, &credential)?
            {
                anyhow::bail!("unknown or invalid hook context");
            }
            let publish = broker.publish();
            if let Some(update) = broker.storage.apply_event_with_context(
                &event,
                attention.as_ref(),
                failure,
                Some(&publish),
            )? {
                let _ = broker.updates.send(update);
            }
            Ok(Response::Ok)
        }
        Request::Capture { invocation_id } => {
            let snapshot = broker.storage.invocation(&invocation_id)?;
            let metadata = snapshot.multiplexer.context("invocation is not in tmux")?;
            let pid = snapshot
                .process
                .child_pid
                .context("invocation has no child PID")?;
            let text = TmuxAdapter.capture(&metadata, pid)?;
            Ok(Response::Captured { text })
        }
        Request::SendInput {
            invocation_id,
            text,
        } => {
            let snapshot = broker.storage.invocation(&invocation_id)?;
            let metadata = snapshot.multiplexer.context("invocation is not in tmux")?;
            let pid = snapshot
                .process
                .child_pid
                .context("invocation has no child PID")?;
            TmuxAdapter.send_input(&metadata, pid, text.as_bytes())?;
            Ok(Response::Ok)
        }
        Request::Listen => unreachable!(),
    }
}

async fn write_json<T: serde::Serialize>(
    write: &mut tokio::net::unix::OwnedWriteHalf,
    value: &T,
) -> Result<()> {
    write.write_all(&serde_json::to_vec(value)?).await?;
    write.write_all(b"\n").await?;
    Ok(())
}

fn process_alive(pid: u32, identity: Option<&str>) -> bool {
    let path = format!("/proc/{pid}");
    if Path::new(&path).exists() {
        return identity.is_none_or(|expected| {
            fs::read_to_string(format!("/proc/{pid}/stat"))
                .ok()
                .and_then(|stat| {
                    stat.rsplit_once(')')?
                        .1
                        .split_whitespace()
                        .nth(19)
                        .map(str::to_owned)
                })
                .as_deref()
                == Some(expected)
        });
    }
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None).is_ok()
}

/// Drops a permanently failing hub delivery after this many attempts so a
/// malformed envelope cannot occupy the bounded outbox forever.
const MAX_HUB_DELIVERY_ATTEMPTS: u32 = 16;

async fn sink_worker(
    storage: Arc<Storage>,
    sinks: Arc<BTreeMap<String, SinkConfig>>,
    source_id: Arc<str>,
    source_name: Arc<Option<String>>,
) {
    let client = reqwest::Client::new();
    let mut snapshot_backoff: std::collections::HashMap<String, (tokio::time::Instant, Duration)> =
        std::collections::HashMap::new();
    loop {
        let _ = deliver_hub_snapshots(
            &storage,
            &sinks,
            &client,
            &source_id,
            source_name.as_deref(),
            &mut snapshot_backoff,
        )
        .await;
        let _ = process_outbox_once(&storage, &sinks, &client).await;
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// Delivers the baseline source snapshot for every hub sink that has not yet
/// established one, before any incremental updates for that sink are sent.
async fn deliver_hub_snapshots(
    storage: &Storage,
    sinks: &BTreeMap<String, SinkConfig>,
    client: &reqwest::Client,
    source_id: &str,
    source_name: Option<&str>,
    backoff: &mut std::collections::HashMap<String, (tokio::time::Instant, Duration)>,
) -> Result<()> {
    if source_id.is_empty() {
        return Ok(());
    }
    for (name, config) in sinks.iter().filter(|(_, c)| c.enabled() && c.is_hub()) {
        if !storage.hub_snapshot_due(name)? {
            backoff.remove(name);
            continue;
        }
        if let Some((next, _)) = backoff.get(name) {
            if tokio::time::Instant::now() < *next {
                continue;
            }
        }
        let (revision, payload) = storage.hub_source_snapshot(source_id, source_name)?;
        let outcome = deliver_hub_payload(client, config, &payload).await;
        match outcome {
            HubOutcome::Accepted => {
                storage.hub_snapshot_delivered(name, revision)?;
                backoff.remove(name);
            }
            HubOutcome::SnapshotRequired | HubOutcome::Rejected | HubOutcome::Transient => {
                let delay = backoff
                    .get(name)
                    .map(|(_, delay)| (*delay * 2).min(Duration::from_secs(60)))
                    .unwrap_or(Duration::from_secs(1));
                backoff.insert(name.clone(), (tokio::time::Instant::now() + delay, delay));
                eprintln!(
                    "sessiontapd: hub sink '{name}' snapshot delivery pending (retry in {delay:?})"
                );
            }
        }
    }
    Ok(())
}

async fn process_outbox_once(
    storage: &Storage,
    sinks: &BTreeMap<String, SinkConfig>,
    client: &reqwest::Client,
) -> Result<usize> {
    let records = storage.due_outbox(100)?;
    let count = records.len();
    for record in records {
        let Some(config) = sinks.get(&record.sink_name) else {
            continue;
        };
        match config {
            SinkConfig::Stdout { .. } => {
                println!("{}", String::from_utf8_lossy(&record.payload));
                storage.acknowledge(&record.sink_name, &record.event_id)?;
            }
            SinkConfig::Http {
                url,
                token_env,
                token_file,
                timeout_ms,
                ..
            } => {
                let delivered = deliver_http(
                    client,
                    url,
                    token_env.as_deref(),
                    token_file.as_deref(),
                    &[],
                    *timeout_ms,
                    &record.payload,
                )
                .await
                .unwrap_or(false);
                if delivered {
                    storage.acknowledge(&record.sink_name, &record.event_id)?;
                } else {
                    storage.retry(&record.sink_name, &record.event_id, record.attempts)?;
                }
            }
            SinkConfig::Hub { .. } => {
                if storage.hub_snapshot_due(&record.sink_name)? {
                    // Hold incremental updates until the baseline snapshot is
                    // delivered so the receiver never sees an update gap.
                    continue;
                }
                match deliver_hub_payload(client, config, &record.payload).await {
                    HubOutcome::Accepted => {
                        storage.acknowledge(&record.sink_name, &record.event_id)?;
                    }
                    HubOutcome::SnapshotRequired => {
                        storage.hub_reset_snapshot(&record.sink_name)?;
                        storage.retry(&record.sink_name, &record.event_id, record.attempts)?;
                    }
                    HubOutcome::Rejected => {
                        if record.attempts + 1 >= MAX_HUB_DELIVERY_ATTEMPTS {
                            eprintln!(
                                "sessiontapd: dropping undeliverable hub event '{}' for sink '{}'",
                                record.event_id, record.sink_name
                            );
                            storage.acknowledge(&record.sink_name, &record.event_id)?;
                        } else {
                            storage.retry(&record.sink_name, &record.event_id, record.attempts)?;
                        }
                    }
                    HubOutcome::Transient => {
                        storage.retry(&record.sink_name, &record.event_id, record.attempts)?;
                    }
                }
            }
        }
    }
    Ok(count)
}

enum HubOutcome {
    Accepted,
    /// The receiver has no baseline state for this source and requested a
    /// source snapshot before further updates.
    SnapshotRequired,
    /// Permanent receiver-side rejection (malformed or stale envelope).
    Rejected,
    /// Network or server-side failure eligible for backoff retry.
    Transient,
}

async fn deliver_hub_payload(
    client: &reqwest::Client,
    config: &SinkConfig,
    payload: &[u8],
) -> HubOutcome {
    let SinkConfig::Hub {
        url,
        token_env,
        token_file,
        timeout_ms,
        trusted_addresses,
        ..
    } = config
    else {
        return HubOutcome::Rejected;
    };
    if let Err(error) = sessiontap_core::config::validate_sink_url(url, trusted_addresses) {
        eprintln!("sessiontapd: hub sink URL rejected: {error}");
        return HubOutcome::Rejected;
    }
    let mut req = client
        .post(url)
        .timeout(Duration::from_millis(*timeout_ms))
        .header("content-type", "application/json")
        .body(payload.to_vec());
    if let Some(name) = token_env {
        if let Ok(token) = std::env::var(name) {
            req = req.bearer_auth(token);
        }
    }
    if let Some(path) = token_file {
        match hub_token(path) {
            Ok(token) => req = req.bearer_auth(token),
            Err(error) => {
                eprintln!("sessiontapd: hub token unavailable: {error}");
                return HubOutcome::Transient;
            }
        }
    }
    let response = match req.send().await {
        Ok(response) => response,
        Err(_) => return HubOutcome::Transient,
    };
    let status = response.status();
    if status.is_success() {
        return HubOutcome::Accepted;
    }
    if status == reqwest::StatusCode::CONFLICT {
        let body = response.text().await.unwrap_or_default();
        if body.contains("snapshot_required") {
            return HubOutcome::SnapshotRequired;
        }
        return HubOutcome::Rejected;
    }
    if status.is_client_error() {
        return HubOutcome::Rejected;
    }
    HubOutcome::Transient
}

fn hub_token(path: &str) -> Result<String> {
    let meta = fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() || meta.permissions().mode() & 0o077 != 0 {
        anyhow::bail!("token file must be private and not a symlink");
    }
    Ok(fs::read_to_string(path)?.trim().to_owned())
}

async fn deliver_http(
    client: &reqwest::Client,
    url: &str,
    token_env: Option<&str>,
    token_file: Option<&str>,
    trusted_addresses: &[String],
    timeout_ms: u64,
    payload: &[u8],
) -> Result<bool> {
    sessiontap_core::config::validate_sink_url(url, trusted_addresses)
        .map_err(anyhow::Error::msg)?;
    let mut req = client
        .post(url)
        .timeout(Duration::from_millis(timeout_ms))
        .header("content-type", "application/json")
        .body(payload.to_vec());
    if let Some(name) = token_env {
        if let Ok(token) = std::env::var(name) {
            req = req.bearer_auth(token);
        }
    }
    if let Some(path) = token_file {
        let meta = fs::symlink_metadata(path)?;
        if meta.file_type().is_symlink() || meta.permissions().mode() & 0o077 != 0 {
            anyhow::bail!("token file must be private and not a symlink");
        }
        req = req.bearer_auth(fs::read_to_string(path)?.trim());
    }
    Ok(req.send().await?.status().is_success())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use sessiontap_core::{
        domain::{
            Activity, Capabilities, EventKind, InvocationId, InvocationSnapshot, Lifecycle,
            NormalizedEvent, ProcessMetadata, derive_status,
        },
        protocol::Request,
    };
    use std::{collections::HashSet, fs::File};
    use tokio::{
        io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
        net::TcpListener,
    };

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
            lifecycle: Lifecycle::Starting,
            activity: Activity::Idle,
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

    fn broker(storage: Storage) -> Broker {
        let (updates, _) = broadcast::channel(8);
        Broker {
            storage: Arc::new(storage),
            updates,
            sinks: Arc::new(BTreeMap::new()),
            source_id: Arc::from(""),
            source_name: Arc::new(None),
        }
    }

    async fn connected_handle(broker: Broker) -> (UnixStream, tokio::task::JoinHandle<Result<()>>) {
        let (client, server) = UnixStream::pair().unwrap();
        let task = tokio::spawn(handle(server, broker));
        (client, task)
    }

    async fn send_request(stream: &mut UnixStream, request: &Request) {
        stream
            .write_all(&serde_json::to_vec(request).unwrap())
            .await
            .unwrap();
        stream.write_all(b"\n").await.unwrap();
    }

    async fn read_stream_line(reader: &mut BufReader<UnixStream>) -> StreamEnvelope {
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        serde_json::from_str(&line).unwrap()
    }

    #[test]
    fn remote_http_is_rejected() {
        use sessiontap_core::config::validate_sink_url;
        assert!(validate_sink_url("http://example.com/hook", &[]).is_err());
        assert!(validate_sink_url("http://127.0.0.1:9/hook", &[]).is_ok());
    }

    #[tokio::test]
    async fn slow_listener_observes_bounded_lag() {
        let (send, mut receive) = broadcast::channel(2);
        for revision in 1..=3 {
            send.send(revision).unwrap();
        }
        assert!(matches!(
            receive.recv().await,
            Err(broadcast::error::RecvError::Lagged(1))
        ));
    }

    #[test]
    fn daemon_restart_restores_committed_snapshot() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("sessiontap.sqlite3");
        let expected = snapshot();
        Storage::open(&path)
            .unwrap()
            .register(&expected, "credential", None)
            .unwrap();
        let reopened = Storage::open(&path).unwrap();
        let (_, restored) = reopened.snapshot().unwrap();
        assert_eq!(restored.len(), 1);
        assert_eq!(restored[0].invocation_id, expected.invocation_id);
    }

    #[test]
    fn concurrent_activation_has_one_lock_owner() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("sessiontap.lock");
        let first = File::create(&path).unwrap();
        let second = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .unwrap();
        first.try_lock_exclusive().unwrap();
        assert!(second.try_lock_exclusive().is_err());
    }

    #[tokio::test]
    async fn subscriber_race_never_omits_child_binding() {
        let broker = broker(Storage::memory().unwrap());
        let expected = snapshot();
        broker
            .storage
            .register(&expected, "credential", None)
            .unwrap();
        let (mut stream, task) = connected_handle(broker.clone()).await;
        send_request(&mut stream, &Request::Listen).await;
        let id = expected.invocation_id.clone();
        let update = broker
            .storage
            .bind_child(&id, "credential", 42, Some("start".into()), None)
            .unwrap();
        let _ = broker.updates.send(AppliedUpdate {
            snapshot: update,
            event: sessiontap_core::domain::LiveEventMetadata {
                kind: EventKind::Enrichment,
                attention: None,
                failure: None,
                turn_id: None,
            },
        });
        let mut reader = BufReader::new(stream);
        let first = read_stream_line(&mut reader).await;
        let observed = match first {
            StreamEnvelope::Snapshot { invocations, .. } => invocations
                .iter()
                .any(|item| item.process.child_pid == Some(42)),
            _ => false,
        } || matches!(
            read_stream_line(&mut reader).await,
            StreamEnvelope::Update { snapshot, .. } if snapshot.process.child_pid == Some(42)
        );
        assert!(observed);
        drop(reader);
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn reconnect_starts_with_a_fresh_snapshot() {
        let broker = broker(Storage::memory().unwrap());
        broker
            .storage
            .register(&snapshot(), "credential", None)
            .unwrap();
        for _ in 0..2 {
            let (mut stream, task) = connected_handle(broker.clone()).await;
            send_request(&mut stream, &Request::Listen).await;
            let mut reader = BufReader::new(stream);
            assert!(matches!(
                read_stream_line(&mut reader).await,
                StreamEnvelope::Snapshot { .. }
            ));
            drop(reader);
            task.await.unwrap().unwrap();
        }
    }

    #[tokio::test]
    async fn hook_broadcast_contains_effective_live_event() {
        let broker = broker(Storage::memory().unwrap());
        let initial = snapshot();
        broker
            .storage
            .register(&initial, "credential", None)
            .unwrap();
        let mut receiver = broker.updates.subscribe();
        let event = NormalizedEvent {
            schema_version: 1,
            event_id: "approval-live".into(),
            invocation_id: initial.invocation_id.clone(),
            provider_event_id: None,
            provider: initial.provider.clone(),
            observed_at: Utc::now(),
            received_at: Utc::now(),
            source: "test".into(),
            kind: EventKind::WaitingApproval,
            provider_session_id: None,
            provider_session_name: None,
            provider_session_start_reason: None,
            provider_metadata: None,
            usage: None,
            turn_id: None,
        };
        let attention = sessiontap_core::domain::AttentionContext {
            summary: "Approve tests".into(),
            source: sessiontap_core::domain::AttentionSource::Description,
        };
        assert!(matches!(
            process(
                Request::HookIngest {
                    provider: initial.provider,
                    invocation_id: initial.invocation_id,
                    credential: "credential".into(),
                    event: Box::new(event),
                    attention: Some(attention.clone()),
                    failure: None
                },
                &broker
            )
            .unwrap(),
            Response::Ok
        ));
        let update = receiver.recv().await.unwrap();
        assert_eq!(update.event.kind, EventKind::WaitingApproval);
        assert_eq!(update.event.attention, Some(attention));
    }

    #[tokio::test]
    async fn client_disconnect_cleans_up_handler() {
        let broker = broker(Storage::memory().unwrap());
        let (mut stream, task) = connected_handle(broker).await;
        send_request(&mut stream, &Request::Listen).await;
        drop(stream);
        let _ = tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("disconnected handler should stop")
            .unwrap();
    }

    #[tokio::test]
    async fn transient_http_retry_is_deduplicated_then_acknowledged() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let receiver = tokio::spawn(async move {
            let mut seen = HashSet::new();
            for attempt in 0..2 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                let header_end = loop {
                    let mut chunk = [0_u8; 4096];
                    let count = stream.read(&mut chunk).await.unwrap();
                    bytes.extend_from_slice(&chunk[..count]);
                    if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                        break end + 4;
                    }
                };
                let headers = String::from_utf8_lossy(&bytes[..header_end]);
                let length = headers
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length: "))
                    .unwrap()
                    .parse::<usize>()
                    .unwrap();
                while bytes.len() - header_end < length {
                    let mut chunk = [0_u8; 4096];
                    let count = stream.read(&mut chunk).await.unwrap();
                    bytes.extend_from_slice(&chunk[..count]);
                }
                let event: sessiontap_core::protocol::SinkEvent =
                    serde_json::from_slice(&bytes[header_end..header_end + length]).unwrap();
                seen.insert(event.event_id);
                let response = if attempt == 0 {
                    b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n".as_slice()
                } else {
                    b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n".as_slice()
                };
                stream.write_all(response).await.unwrap();
            }
            seen
        });

        let storage = Storage::memory().unwrap();
        let initial = snapshot();
        storage.register(&initial, "credential", None).unwrap();
        let mut sinks = BTreeMap::new();
        sinks.insert(
            "receiver".into(),
            SinkConfig::Http {
                enabled: true,
                url: format!("http://{address}/events"),
                token_env: None,
                token_file: None,
                timeout_ms: 1_000,
                max_payload_bytes: 64 * 1024,
                fields: vec![],
            },
        );
        let publish = sessiontap_storage::Publish {
            sinks: &sinks,
            source_id: "",
            source_name: None,
        };
        let event = NormalizedEvent {
            schema_version: 1,
            event_id: "stable-event-id".into(),
            invocation_id: initial.invocation_id,
            provider_event_id: None,
            provider: "claude".into(),
            observed_at: Utc::now(),
            received_at: Utc::now(),
            source: "test".into(),
            kind: EventKind::NewTurn,
            provider_session_id: None,
            provider_session_name: None,
            provider_session_start_reason: None,
            provider_metadata: None,
            usage: None,
            turn_id: None,
        };
        storage.apply_event(&event, Some(&publish)).unwrap();
        let client = reqwest::Client::new();
        assert_eq!(
            process_outbox_once(&storage, &sinks, &client)
                .await
                .unwrap(),
            1
        );
        assert!(storage.due_outbox(1).unwrap().is_empty());
        tokio::time::sleep(Duration::from_millis(1_050)).await;
        assert_eq!(
            process_outbox_once(&storage, &sinks, &client)
                .await
                .unwrap(),
            1
        );
        assert!(storage.due_outbox(1).unwrap().is_empty());
        let seen = receiver.await.unwrap();
        assert_eq!(seen, HashSet::from(["stable-event-id".to_owned()]));
    }

    #[tokio::test]
    async fn socket_and_token_permission_attack_matrix() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let temp = tempfile::tempdir().unwrap();
        let socket = temp.path().join("sessiontap.sock");
        let _listener = UnixListener::bind(&socket).unwrap();
        fs::set_permissions(&socket, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            socket.metadata().unwrap().permissions().mode() & 0o777,
            0o600
        );

        let client = reqwest::Client::new();
        let token = temp.path().join("token");
        fs::write(&token, "secret").unwrap();
        fs::set_permissions(&token, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(
            deliver_http(
                &client,
                "http://127.0.0.1:9/events",
                None,
                token.to_str(),
                &[],
                10,
                b"{}"
            )
            .await
            .is_err()
        );

        fs::set_permissions(&token, fs::Permissions::from_mode(0o600)).unwrap();
        let link = temp.path().join("token-link");
        symlink(&token, &link).unwrap();
        assert!(
            deliver_http(
                &client,
                "http://127.0.0.1:9/events",
                None,
                link.to_str(),
                &[],
                10,
                b"{}"
            )
            .await
            .is_err()
        );
        assert_eq!(fs::read_to_string(token).unwrap(), "secret");
    }
}
