//! Daemon behavior ported from the disabled `main.rs` tests, driven through
//! the library `App`, sinks, and worker.

mod common;

use common::{app, app_with, event, snapshot};
use sessiontap_core::{
    config::{Config, DaemonConfig},
    domain::{EventKind, PublicReasonKind, PublicStatus, StatusReasonContext, StatusReasonSource},
};
use sessiontap_infra::socket::{acquire_exclusive_lock, bind_private_unix_socket};
use sessiontap_storage::Storage;
use sessiontapd::{
    app::{App, Collection, PublishConfig},
    sinks::{TokenSource, build_sinks},
    workers::SinkWorker,
};
use std::{
    collections::HashSet,
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    sync::{Arc, Barrier},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::broadcast,
};

#[test]
fn remote_http_is_rejected() {
    let remote: Config = toml::from_str(
        "version=1\n[sinks.remote]\ntype='http'\nenabled=true\nurl='http://example.com/hook'\n",
    )
    .unwrap();
    assert!(build_sinks(&remote.sinks).is_err());
    let local: Config = toml::from_str(
        "version=1\n[sinks.local]\ntype='http'\nenabled=true\nurl='http://127.0.0.1:9/hook'\n",
    )
    .unwrap();
    assert!(build_sinks(&local.sinks).is_ok());
}

#[tokio::test]
async fn slow_listener_observes_bounded_lag() {
    let app = app_with(
        Storage::memory().unwrap(),
        &DaemonConfig {
            update_buffer: 2,
            ..DaemonConfig::default()
        },
    );
    let (_, _, mut receiver) = app.subscribe().unwrap();
    for _ in 0..3 {
        app.register(snapshot(), "credential").unwrap();
    }
    assert!(matches!(
        receiver.recv().await,
        Err(broadcast::error::RecvError::Lagged(1))
    ));
}

#[test]
fn daemon_restart_restores_committed_snapshot() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("sessiontap.sqlite3");
    let expected = snapshot();
    app(Storage::open(&path).unwrap())
        .register(expected.clone(), "credential")
        .unwrap();
    let (_, restored) = app(Storage::open(&path).unwrap()).status().unwrap();
    assert_eq!(restored.len(), 1);
    assert_eq!(restored[0].invocation_id, expected.invocation_id);
}

#[test]
fn concurrent_activation_has_one_lock_owner() {
    let temp = tempfile::tempdir().unwrap();
    let path = Arc::new(temp.path().join("sessiontap.lock"));
    let barrier = Arc::new(Barrier::new(2));
    let attempts: Vec<_> = (0..2)
        .map(|_| {
            let (path, barrier) = (path.clone(), barrier.clone());
            std::thread::spawn(move || {
                barrier.wait();
                let lock = acquire_exclusive_lock(&path);
                // Keep the winner's lock alive until both have tried.
                barrier.wait();
                lock.is_ok()
            })
        })
        .collect();
    let owners = attempts
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .filter(|owned| *owned)
        .count();
    assert_eq!(owners, 1);
}

#[tokio::test]
async fn hook_broadcast_contains_effective_live_event() {
    let app = app(Storage::memory().unwrap());
    let initial = snapshot();
    app.register(initial.clone(), "credential").unwrap();
    app.bind_child(&initial.invocation_id, "credential", 42, None)
        .unwrap();
    let (_, _, mut receiver) = app.subscribe().unwrap();
    app.ingest_hook(
        initial.provider.clone(),
        initial.invocation_id.clone(),
        "credential".into(),
        event(&initial, "approval-live", EventKind::WaitingApproval),
        Some(StatusReasonContext {
            summary: "Approve tests".into(),
            source: StatusReasonSource::Description,
        }),
        None,
    )
    .unwrap();
    let update = receiver.recv().await.unwrap();
    assert_eq!(update.view.status, PublicStatus::Blocked);
    let reason = update.view.reason.unwrap();
    assert_eq!(reason.kind, PublicReasonKind::Approval);
    assert_eq!(reason.summary, "Approve tests");
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
            let headers = String::from_utf8_lossy(&bytes[..header_end]).to_ascii_lowercase();
            let length = headers
                .lines()
                .find_map(|line| line.strip_prefix("content-length: "))
                .unwrap()
                .trim()
                .parse::<usize>()
                .unwrap();
            while bytes.len() - header_end < length {
                let mut chunk = [0_u8; 4096];
                let count = stream.read(&mut chunk).await.unwrap();
                bytes.extend_from_slice(&chunk[..count]);
            }
            let envelope: serde_json::Value =
                serde_json::from_slice(&bytes[header_end..header_end + length]).unwrap();
            seen.insert(envelope["delivery_id"].as_str().unwrap().to_owned());
            let response = if attempt == 0 {
                b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n".as_slice()
            } else {
                b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n".as_slice()
            };
            stream.write_all(response).await.unwrap();
        }
        seen
    });

    let config: Config = toml::from_str(&format!(
        "version=1\n[sinks.receiver]\ntype='http'\nenabled=true\nurl='http://{address}/events'\ntimeout_ms=1000\n"
    ))
    .unwrap();
    let daemon = DaemonConfig::default();
    let app = App::new(
        Arc::new(Storage::memory().unwrap()),
        PublishConfig {
            sinks: config.sinks.clone(),
            source_id: String::new(),
            source_name: None,
        },
        &daemon,
        Arc::new(sessiontap_infra::multiplexer::MultiplexerRegistry::empty()),
        Collection {
            home: "/nonexistent".into(),
            registry: Arc::new(sessiontap_adapters::AdapterRegistry::new(&config)),
        },
    );
    let initial = snapshot();
    app.register(initial.clone(), "credential").unwrap();
    let worker = SinkWorker::new(&app, build_sinks(&config.sinks).unwrap(), &daemon);
    // Drain the registration update so only the hook event is observed.
    let first = app.storage().due_outbox(10).unwrap();
    for record in &first {
        app.storage()
            .acknowledge(&record.sink_name, &record.event_id)
            .unwrap();
    }
    app.bind_child(&initial.invocation_id, "credential", 42, None)
        .unwrap();
    app.ingest_hook(
        initial.provider.clone(),
        initial.invocation_id.clone(),
        "credential".into(),
        event(&initial, "stable-event-id", EventKind::NewTurn),
        None,
        None,
    )
    .unwrap();

    assert_eq!(worker.process_outbox_once().await.unwrap(), 1);
    assert!(app.storage().due_outbox(1).unwrap().is_empty());
    tokio::time::sleep(Duration::from_millis(1_050)).await;
    assert_eq!(worker.process_outbox_once().await.unwrap(), 1);
    assert!(app.storage().due_outbox(1).unwrap().is_empty());
    let seen = receiver.await.unwrap();
    assert_eq!(seen, HashSet::from(["stable-event-id".to_owned()]));
}

#[tokio::test]
async fn socket_and_token_permission_attack_matrix() {
    let temp = tempfile::tempdir().unwrap();
    let socket = temp.path().join("sessiontap.sock");
    let lock = temp.path().join("sessiontap.lock");
    let (listener, held) = bind_private_unix_socket(&socket, &lock).unwrap();
    assert_eq!(
        socket.metadata().unwrap().permissions().mode() & 0o777,
        0o600
    );
    // A second daemon cannot take the lock; with the lock released, a live
    // listener still cannot be displaced; a stale socket file is replaced.
    assert!(bind_private_unix_socket(&socket, &lock).is_err());
    assert!(acquire_exclusive_lock(&lock).is_err());
    drop(held);
    assert!(bind_private_unix_socket(&socket, &lock).is_err());
    drop(listener);
    let rebound = bind_private_unix_socket(&socket, &lock).unwrap();
    drop(rebound);

    let token = temp.path().join("token");
    fs::write(&token, "secret").unwrap();
    fs::set_permissions(&token, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(TokenSource::File(token.clone()).token().is_err());

    fs::set_permissions(&token, fs::Permissions::from_mode(0o600)).unwrap();
    let link = temp.path().join("token-link");
    symlink(&token, &link).unwrap();
    assert!(TokenSource::File(link).token().is_err());
    assert_eq!(
        TokenSource::File(token.clone()).token().unwrap().as_deref(),
        Some("secret")
    );
    assert_eq!(fs::read_to_string(token).unwrap(), "secret");
}
