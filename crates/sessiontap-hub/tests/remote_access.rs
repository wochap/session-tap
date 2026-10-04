use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use sessiontap_core::{
    domain::{InvocationId, PublicAgentView, PublicField, PublicStatus},
    protocol::{SourceEnvelope, SourceIdentity},
};
use sessiontap_hub::{
    cli,
    config::RemoteConfig,
    ingest::{IngestAuth, IngestedRequest, handle_ingest},
    listen::{HubRequest, HubResponse, HubStreamEnvelope},
    remote::{
        Backoff, CLOSE_REVOKED, CLOSE_SCOPE_WITHDRAWN, RemoteGate, RemoteLimits, serve_remote,
        supervise_listener,
    },
    scope::Scope,
    service::{self, Hub, RemoteInfo, pair_mac},
    store::HubStore,
    tls::{Identity, client_config, client_config_with_versions, server_config},
};
use std::{
    collections::{BTreeSet, HashMap},
    net::{Ipv4Addr, SocketAddr},
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, BufReader},
    net::{TcpListener, TcpSocket, TcpStream, UnixListener, UnixStream},
    sync::broadcast,
    time::timeout,
};
use tokio_rustls::{TlsConnector, client::TlsStream, rustls::pki_types::ServerName};
use tokio_tungstenite::{WebSocketStream, tungstenite::Message};

type Ws = WebSocketStream<TlsStream<TcpStream>>;

const WAIT: Duration = Duration::from_secs(5);

struct TestHub {
    hub: Arc<Hub>,
    hub_id: String,
    addr: SocketAddr,
    socket: PathBuf,
    _temp: tempfile::TempDir,
}

fn view(status: PublicStatus) -> PublicAgentView {
    PublicAgentView {
        invocation_id: InvocationId::new(),
        provider: "codex".into(),
        status,
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

/// Default limits with roomy admission caps, so tests that churn
/// connections from loopback do not trip them.
fn roomy(ping: Duration) -> RemoteLimits {
    RemoteLimits {
        ping_interval: ping,
        max_unauthenticated: 64,
        max_unauthenticated_per_address: 64,
        ..RemoteLimits::default()
    }
}

async fn start_hub(ping: Duration) -> TestHub {
    start_hub_with(roomy(ping), false).await
}

/// Starts a hub with the given limits. Unless `real_rate_limit`, the
/// `pair.*` rate limit is effectively off.
async fn start_hub_with(limits: RemoteLimits, real_rate_limit: bool) -> TestHub {
    start_hub_full(limits, real_rate_limit, false).await
}

/// Like `start_hub_with`, with `remote.control` set to `control`.
async fn start_hub_full(limits: RemoteLimits, real_rate_limit: bool, control: bool) -> TestHub {
    let temp = tempfile::tempdir().unwrap();
    let identity = Identity::generate("test-hub").unwrap();
    let hub_id = identity.spki_sha256();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (updates, _) = broadcast::channel(64);
    let hub = Hub::new(
        Arc::new(HubStore::memory().unwrap()),
        updates,
        Some(RemoteInfo {
            hub_id: hub_id.clone(),
            hub_name: "Test Hub".into(),
            hub_spki: identity.spki.clone(),
            remote: RemoteConfig {
                control,
                ..remote_config(&[&addr.to_string()], &["hub.tailnet.ts.net:8932"])
            },
            interfaces: Vec::new,
        }),
    );
    let hub = Arc::new(if real_rate_limit {
        hub
    } else {
        hub.with_pair_rate(100_000, Duration::from_micros(1))
    });
    let acceptor = tokio_rustls::TlsAcceptor::from(server_config(&identity).unwrap());
    let gate = RemoteGate::new(&limits);
    tokio::spawn(serve_remote(
        listener,
        acceptor,
        Arc::clone(&hub),
        gate,
        limits,
    ));
    let socket = temp.path().join("hub.sock");
    let unix = UnixListener::bind(&socket).unwrap();
    tokio::spawn(service::serve_unix_listener(unix, Arc::clone(&hub)));
    TestHub {
        hub,
        hub_id,
        addr,
        socket,
        _temp: temp,
    }
}

fn remote_config(listen: &[&str], advertise: &[&str]) -> RemoteConfig {
    RemoteConfig {
        name: None,
        listen: listen.iter().map(|entry| (*entry).to_owned()).collect(),
        advertise: advertise.iter().map(|entry| (*entry).to_owned()).collect(),
        control: false,
    }
}

/// Starts a hub whose remote listener is supervised on `listen`; the test
/// connects to `connect`.
async fn start_supervised_hub(
    listen: SocketAddr,
    connect: SocketAddr,
    backoff: Backoff,
) -> TestHub {
    let temp = tempfile::tempdir().unwrap();
    let identity = Identity::generate("test-hub").unwrap();
    let hub_id = identity.spki_sha256();
    let (updates, _) = broadcast::channel(64);
    let hub = Arc::new(Hub::new(
        Arc::new(HubStore::memory().unwrap()),
        updates,
        Some(RemoteInfo {
            hub_id: hub_id.clone(),
            hub_name: "Test Hub".into(),
            hub_spki: identity.spki.clone(),
            remote: remote_config(&[&listen.to_string()], &[]),
            interfaces: Vec::new,
        }),
    ));
    let limits = roomy(Duration::from_secs(60));
    tokio::spawn(supervise_listener(
        listen,
        tokio_rustls::TlsAcceptor::from(server_config(&identity).unwrap()),
        Arc::clone(&hub),
        RemoteGate::new(&limits),
        limits,
        backoff,
    ));
    let socket = temp.path().join("hub.sock");
    let unix = UnixListener::bind(&socket).unwrap();
    tokio::spawn(service::serve_unix_listener(unix, Arc::clone(&hub)));
    TestHub {
        hub,
        hub_id,
        addr: connect,
        socket,
        _temp: temp,
    }
}

const TEST_BACKOFF: Backoff = Backoff {
    initial: Duration::from_millis(10),
    max: Duration::from_millis(50),
};

/// Retries a paired `hub.info` call until the listener answers.
async fn info_when_bound(hub: &TestHub, phone: &Identity) -> Value {
    timeout(WAIT, async {
        loop {
            let config = client_config(&hub.hub_id, Some(phone)).unwrap();
            if let Ok(tls) = tls_connect(hub, config).await
                && let Ok((mut ws, _)) = tokio_tungstenite::client_async("wss://hub/", tls).await
            {
                return call(&mut ws, 1, "hub.info", json!({})).await;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("listener binds")
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

const LOCAL: Ipv4Addr = Ipv4Addr::new(127, 0, 0, 1);

/// Opens TCP to the hub from a chosen loopback source address.
async fn tcp_from(hub: &TestHub, source: Ipv4Addr) -> std::io::Result<TcpStream> {
    let socket = TcpSocket::new_v4()?;
    socket.bind(SocketAddr::from((source, 0)))?;
    socket.connect(hub.addr).await
}

async fn tls_connect_from(
    hub: &TestHub,
    config: Arc<tokio_rustls::rustls::ClientConfig>,
    source: Ipv4Addr,
) -> std::io::Result<TlsStream<TcpStream>> {
    let tcp = tcp_from(hub, source).await?;
    TlsConnector::from(config)
        .connect(ServerName::try_from("hub").unwrap(), tcp)
        .await
}

async fn tls_connect(
    hub: &TestHub,
    config: Arc<tokio_rustls::rustls::ClientConfig>,
) -> std::io::Result<TlsStream<TcpStream>> {
    tls_connect_from(hub, config, LOCAL).await
}

async fn connect_from(hub: &TestHub, client: Option<&Identity>, source: Ipv4Addr) -> Ws {
    let tls = tls_connect_from(hub, client_config(&hub.hub_id, client).unwrap(), source)
        .await
        .unwrap();
    tokio_tungstenite::client_async("wss://hub/", tls)
        .await
        .unwrap()
        .0
}

async fn connect(hub: &TestHub, client: Option<&Identity>) -> Ws {
    connect_from(hub, client, LOCAL).await
}

/// Waits until the hub ends the connection, ignoring frames sent before.
/// Returns whether any text frame arrived and the close code, if any.
async fn wait_closed(ws: &mut Ws) -> (bool, Option<u16>) {
    let mut saw_text = false;
    loop {
        match timeout(WAIT, ws.next())
            .await
            .expect("connection stays open")
        {
            Some(Ok(Message::Text(_))) => saw_text = true,
            Some(Ok(Message::Close(frame))) => {
                return (saw_text, frame.map(|frame| u16::from(frame.code)));
            }
            Some(Ok(_)) => {}
            Some(Err(_)) | None => return (saw_text, None),
        }
    }
}

async fn send(ws: &mut Ws, value: Value) {
    ws.send(Message::text(value.to_string())).await.unwrap();
}

async fn recv(ws: &mut Ws) -> Value {
    loop {
        match timeout(WAIT, ws.next()).await.unwrap().unwrap().unwrap() {
            Message::Text(text) => return serde_json::from_str(text.as_str()).unwrap(),
            Message::Ping(_) | Message::Pong(_) => {}
            other => panic!("unexpected frame {other:?}"),
        }
    }
}

async fn call(ws: &mut Ws, id: u64, method: &str, params: Value) -> Value {
    send(ws, json!({"id": id, "method": method, "params": params})).await;
    let response = recv(ws).await;
    assert_eq!(response["id"], id, "{response}");
    response
}

fn paired(hub: &TestHub, scopes: &[&str]) -> Identity {
    let identity = Identity::generate("phone").unwrap();
    let sha = identity.spki_sha256();
    let scopes: Vec<String> = scopes.iter().map(|scope| (*scope).to_owned()).collect();
    hub.hub
        .store
        .upsert_device(&service::device_id(&sha), &sha, "Phone", &scopes)
        .unwrap();
    identity
}

fn ingest(store: &HubStore, envelope: &SourceEnvelope) {
    let request = IngestedRequest {
        method: "POST".into(),
        path: "/v1/envelopes".into(),
        bearer: None,
        body: serde_json::to_vec(envelope).unwrap(),
    };
    assert_eq!(
        handle_ingest(store, &IngestAuth::default(), &request).status,
        200
    );
}

/// Seeds one stopped and one running agent; returns their invocation IDs.
fn seed(hub: &TestHub) -> (String, String) {
    let stopped = view(PublicStatus::Stopped);
    let running = view(PublicStatus::Running);
    let ids = (
        stopped.invocation_id.to_string(),
        running.invocation_id.to_string(),
    );
    ingest(
        &hub.hub.store,
        &SourceEnvelope::Snapshot {
            schema_version: 1,
            source: SourceIdentity {
                id: "host".into(),
                display_name: None,
            },
            revision: 1,
            views: vec![stopped, running],
        },
    );
    ids
}

async fn unix_listen(hub: &TestHub) -> tokio::io::Lines<BufReader<UnixStream>> {
    let mut stream = UnixStream::connect(&hub.socket).await.unwrap();
    sessiontap_infra::json::write_json_line(&mut stream, &HubRequest::Listen)
        .await
        .unwrap();
    BufReader::new(stream).lines()
}

async fn next_envelope(lines: &mut tokio::io::Lines<BufReader<UnixStream>>) -> HubStreamEnvelope {
    let line = timeout(WAIT, lines.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    serde_json::from_str(&line).unwrap()
}

fn agent_count(envelope: &HubStreamEnvelope) -> usize {
    match envelope {
        HubStreamEnvelope::Snapshot { agents, .. } => agents.len(),
        HubStreamEnvelope::Update { .. } => panic!("expected snapshot"),
    }
}

#[tokio::test]
async fn unix_admin_requests_answer_once() {
    let hub = start_hub(Duration::from_secs(60)).await;
    let (stopped, running) = seed(&hub);
    paired(&hub, &["read"]);
    let response = cli::request_once(&hub.socket, &HubRequest::Devices)
        .await
        .unwrap();
    assert!(matches!(response, HubResponse::Devices { devices, .. } if devices.len() == 1));
    let forget = |invocation_id: &str| HubRequest::Forget {
        source_id: "host".into(),
        invocation_id: invocation_id.into(),
    };
    let response = cli::request_once(&hub.socket, &forget(&running))
        .await
        .unwrap();
    assert!(matches!(response, HubResponse::Error { code, .. } if code == "not_stopped"));
    let response = cli::request_once(&hub.socket, &forget("missing"))
        .await
        .unwrap();
    assert!(matches!(response, HubResponse::Error { code, .. } if code == "not_found"));
    let response = cli::request_once(&hub.socket, &forget(&stopped))
        .await
        .unwrap();
    assert!(matches!(response, HubResponse::Forgotten { .. }));
    let response = cli::request_once(
        &hub.socket,
        &HubRequest::Revoke {
            device: "zzzz".into(),
        },
    )
    .await
    .unwrap();
    assert!(matches!(response, HubResponse::Error { code, .. } if code == "not_found"));
}

#[tokio::test]
async fn cli_commands_run_against_a_hub() {
    let hub = start_hub(Duration::from_secs(60)).await;
    let (stopped, running) = seed(&hub);
    let device = paired(&hub, &["read", "manage"]);
    let mut out = Vec::new();
    cli::devices(&hub.socket, &mut out).await.unwrap();
    let text = String::from_utf8(out).unwrap();
    assert!(
        text.contains("Phone") && text.contains("read,manage"),
        "{text}"
    );

    let mut out = Vec::new();
    let error = cli::forget(&hub.socket, "host", &running, &mut out)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("only stopped"), "{error}");
    cli::forget(&hub.socket, "host", &stopped, &mut out)
        .await
        .unwrap();

    // ambiguous prefix lists matches and revokes nothing
    let other = Identity::generate("tablet").unwrap();
    let mut sha = other.spki_sha256();
    let id = service::device_id(&device.spki_sha256());
    sha.replace_range(..1, &id[..1]);
    hub.hub
        .store
        .upsert_device(&format!("{}x", &id[..1]), &sha, "Tablet", &["read".into()])
        .unwrap();
    let error = cli::revoke(&hub.socket, &id[..1], &mut out)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("Tablet"), "{error}");
    assert_eq!(hub.hub.store.devices().unwrap().len(), 2);
    let mut out = Vec::new();
    cli::revoke(&hub.socket, &id[..6], &mut out).await.unwrap();
    assert!(String::from_utf8(out).unwrap().contains("revoked"));
    assert_eq!(hub.hub.store.devices().unwrap().len(), 1);
}

#[tokio::test]
async fn cli_reports_a_stopped_service() {
    let temp = tempfile::tempdir().unwrap();
    let error = cli::devices(&temp.path().join("missing.sock"), &mut Vec::new())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("not running"), "{error}");
}

#[tokio::test]
async fn forget_rebaselines_unix_listener() {
    let hub = start_hub(Duration::from_secs(60)).await;
    let (stopped, _) = seed(&hub);
    let mut lines = unix_listen(&hub).await;
    assert_eq!(agent_count(&next_envelope(&mut lines).await), 2);
    cli::forget(&hub.socket, "host", &stopped, &mut Vec::new())
        .await
        .unwrap();
    assert_eq!(agent_count(&next_envelope(&mut lines).await), 1);
}

#[tokio::test]
async fn tls12_is_refused_and_client_certs_are_optional() {
    let hub = start_hub(Duration::from_secs(60)).await;
    let tls12 =
        client_config_with_versions(&hub.hub_id, None, &[&tokio_rustls::rustls::version::TLS12])
            .unwrap();
    assert!(tls_connect(&hub, tls12).await.is_err());
    let anonymous = client_config(&hub.hub_id, None).unwrap();
    assert!(tls_connect(&hub, anonymous).await.is_ok());
    let device = Identity::generate("phone").unwrap();
    let with_cert = client_config(&hub.hub_id, Some(&device)).unwrap();
    assert!(tls_connect(&hub, with_cert).await.is_ok());
    let wrong_pin = client_config(&"0".repeat(64), None).unwrap();
    assert!(tls_connect(&hub, wrong_pin).await.is_err());
}

#[tokio::test]
async fn protocol_errors_and_scopes() {
    let hub = start_hub(Duration::from_secs(60)).await;
    let (stopped, _) = seed(&hub);

    let stranger = Identity::generate("stranger").unwrap();
    let mut ws = connect(&hub, Some(&stranger)).await;
    let response = call(&mut ws, 1, "listen", json!({})).await;
    assert_eq!(response["error"]["code"], "unauthorized");
    let mut anonymous = connect(&hub, None).await;
    let response = call(&mut anonymous, 1, "hub.info", json!({})).await;
    assert_eq!(response["error"]["code"], "unauthorized");

    let reader = paired(&hub, &["read"]);
    let mut ws = connect(&hub, Some(&reader)).await;
    let info = call(&mut ws, 1, "hub.info", Value::Null).await;
    assert_eq!(info["result"]["hub_id"], hub.hub_id);
    assert_eq!(info["result"]["hub_name"], "Test Hub");
    assert_eq!(info["result"]["protocol"], 1);
    assert_eq!(info["result"]["scopes"], json!(["read"]));
    let response = call(&mut ws, 2, "capture", json!({})).await;
    assert_eq!(response["error"]["code"], "unknown_method");
    ws.send(Message::text("{not json")).await.unwrap();
    assert_eq!(recv(&mut ws).await["error"]["code"], "bad_request");
    send(&mut ws, json!({"method": "hub.info"})).await;
    assert_eq!(recv(&mut ws).await["error"]["code"], "bad_request");
    let response = call(
        &mut ws,
        3,
        "forget",
        json!({"source_id": "host", "invocation_id": stopped}),
    )
    .await;
    assert_eq!(response["error"]["code"], "forbidden");
    assert_eq!(hub.hub.store.merged().unwrap().2.len(), 2);
    // the connection stays usable after errors
    let info = call(&mut ws, 4, "hub.info", json!({})).await;
    assert!(info["result"].is_object());
}

#[tokio::test]
async fn remote_listen_streams_and_answers_requests() {
    let hub = start_hub(Duration::from_secs(60)).await;
    let (stopped, running) = seed(&hub);
    let device = paired(&hub, &["read", "manage"]);
    let mut ws = connect(&hub, Some(&device)).await;
    let ack = call(&mut ws, 1, "listen", json!({})).await;
    assert!(ack["result"].is_object());
    let snapshot = recv(&mut ws).await;
    assert_eq!(snapshot["event"], "stream");
    assert!(snapshot.get("id").is_none());
    assert_eq!(snapshot["data"]["type"], "snapshot");
    assert_eq!(snapshot["data"]["agents"].as_array().unwrap().len(), 2);

    // a live update arrives as the same envelope the unix stream emits
    let mut unix = unix_listen(&hub).await;
    next_envelope(&mut unix).await;
    let mut idle = hub.hub.store.merged().unwrap().2[0].view.clone();
    if idle.invocation_id.to_string() != running {
        idle = hub.hub.store.merged().unwrap().2[1].view.clone();
    }
    idle.status = PublicStatus::Idle;
    idle.updated_at = Utc::now();
    let update = SourceEnvelope::Update {
        schema_version: 1,
        source_id: "host".into(),
        delivery_id: "d1".into(),
        revision: 2,
        changed: BTreeSet::from([PublicField::Status]),
        view: Box::new(idle),
    };
    let request = IngestedRequest {
        method: "POST".into(),
        path: "/".into(),
        bearer: None,
        body: serde_json::to_vec(&update).unwrap(),
    };
    let publication = handle_ingest(&hub.hub.store, &IngestAuth::default(), &request)
        .publication
        .unwrap();
    hub.hub.updates.send(publication).unwrap();
    let pushed = recv(&mut ws).await;
    assert_eq!(pushed["data"]["type"], "update");
    let local = timeout(WAIT, unix.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        pushed["data"],
        serde_json::from_str::<Value>(&local).unwrap()
    );

    // requests are still served mid-stream
    let info = call(&mut ws, 2, "hub.info", json!({})).await;
    assert_eq!(info["result"]["hub_id"], hub.hub_id);
    let again = call(&mut ws, 3, "listen", json!({})).await;
    assert_eq!(again["error"]["code"], "bad_request");

    // remote forget: not_stopped, not_found, then success re-baselines both
    let response = call(
        &mut ws,
        4,
        "forget",
        json!({"source_id": "host", "invocation_id": running}),
    )
    .await;
    assert_eq!(response["error"]["code"], "not_stopped");
    let response = call(
        &mut ws,
        5,
        "forget",
        json!({"source_id": "host", "invocation_id": "nope"}),
    )
    .await;
    assert_eq!(response["error"]["code"], "not_found");
    send(
        &mut ws,
        json!({"id": 6, "method": "forget", "params": {"source_id": "host", "invocation_id": stopped}}),
    )
    .await;
    let mut saw_response = false;
    let mut saw_snapshot = false;
    while !(saw_response && saw_snapshot) {
        let message = recv(&mut ws).await;
        if message["id"] == 6 {
            assert!(message["result"]["hub_revision"].is_u64(), "{message}");
            saw_response = true;
        } else {
            assert_eq!(message["data"]["type"], "snapshot");
            assert_eq!(message["data"]["agents"].as_array().unwrap().len(), 1);
            saw_snapshot = true;
        }
    }
    assert_eq!(agent_count(&next_envelope(&mut unix).await), 1);
}

#[tokio::test]
async fn revocation_closes_connection_and_refuses_reconnect() {
    let hub = start_hub(Duration::from_secs(60)).await;
    seed(&hub);
    let device = paired(&hub, &["read"]);
    let mut ws = connect(&hub, Some(&device)).await;
    call(&mut ws, 1, "listen", json!({})).await;
    recv(&mut ws).await;
    cli::revoke(
        &hub.socket,
        &service::device_id(&device.spki_sha256()),
        &mut Vec::new(),
    )
    .await
    .unwrap();
    let close = loop {
        match timeout(WAIT, ws.next()).await.unwrap() {
            Some(Ok(Message::Close(frame))) => break frame,
            Some(Ok(_)) => {}
            other => panic!("expected close, got {other:?}"),
        }
    };
    assert_eq!(u16::from(close.unwrap().code), CLOSE_REVOKED);
    // nothing follows the revocation close
    match timeout(WAIT, ws.next()).await.unwrap() {
        None | Some(Err(_) | Ok(Message::Close(_))) => {}
        Some(Ok(other)) => panic!("frame after revocation close: {other:?}"),
    }
    let mut ws = connect(&hub, Some(&device)).await;
    let response = call(&mut ws, 1, "listen", json!({})).await;
    assert_eq!(response["error"]["code"], "unauthorized");
}

#[tokio::test]
async fn dead_peers_are_closed_after_a_missed_pong() {
    let hub = start_hub(Duration::from_millis(100)).await;
    let device = paired(&hub, &["read"]);
    let tls = tls_connect(&hub, client_config(&hub.hub_id, Some(&device)).unwrap())
        .await
        .unwrap();
    let (mut ws, _) = tokio_tungstenite::client_async("wss://hub/", tls)
        .await
        .unwrap();
    // never read: no pong is sent, so the hub must drop the connection
    tokio::time::sleep(Duration::from_millis(400)).await;
    let mut closed = false;
    while let Ok(Some(message)) = timeout(WAIT, ws.next()).await {
        if matches!(message, Err(_) | Ok(Message::Close(_))) {
            closed = true;
            break;
        }
    }
    assert!(closed);
}

/// A scripted device: begins pairing and proves the secret from the QR.
async fn device_pair(hub: &TestHub, device: &Identity, secret: &[u8], name: &str) -> (Ws, Value) {
    device_pair_from(hub, device, secret, name, LOCAL).await
}

async fn device_pair_from(
    hub: &TestHub,
    device: &Identity,
    secret: &[u8],
    name: &str,
    source: Ipv4Addr,
) -> (Ws, Value) {
    let mut ws = connect_from(hub, Some(device), source).await;
    let begin = call(&mut ws, 1, "pair.begin", json!({})).await;
    let nonce = URL_SAFE_NO_PAD
        .decode(begin["result"]["nonce"].as_str().unwrap())
        .unwrap();
    let hub_spki = hub.hub.remote.as_ref().unwrap().hub_spki.clone();
    let mac = pair_mac(secret, &hub_spki, &device.spki, &nonce);
    send(
        &mut ws,
        json!({"id": 2, "method": "pair.complete", "params": {"name": name, "mac": URL_SAFE_NO_PAD.encode(mac)}}),
    )
    .await;
    let response = recv(&mut ws).await;
    (ws, response)
}

/// Opens a pairing window over the unix socket and returns the QR payload
/// plus the open conversation.
async fn open_window(
    hub: &TestHub,
    scopes: Vec<String>,
) -> (Value, tokio::io::Lines<BufReader<UnixStream>>) {
    let mut stream = UnixStream::connect(&hub.socket).await.unwrap();
    sessiontap_infra::json::write_json_line(&mut stream, &HubRequest::Pair { scopes })
        .await
        .unwrap();
    let mut lines = BufReader::new(stream).lines();
    let line = lines.next_line().await.unwrap().unwrap();
    let HubResponse::PairWindow { payload, .. } = serde_json::from_str(&line).unwrap() else {
        panic!("expected pair window: {line}");
    };
    (serde_json::from_str(&payload).unwrap(), lines)
}

fn secret_of(payload: &Value) -> Vec<u8> {
    URL_SAFE_NO_PAD
        .decode(payload["s"].as_str().unwrap())
        .unwrap()
}

#[tokio::test]
async fn hub_info_reports_pairing_endpoints() {
    let hub = start_hub(Duration::from_secs(60)).await;
    let expected = json!([hub.addr.to_string(), "hub.tailnet.ts.net:8932"]);

    let manager = paired(&hub, &["read", "manage"]);
    let mut ws = connect(&hub, Some(&manager)).await;
    let info = call(&mut ws, 1, "hub.info", json!({})).await;
    assert_eq!(info["result"]["endpoints"], expected);

    let reader = paired(&hub, &["read"]);
    let mut ws = connect(&hub, Some(&reader)).await;
    let info = call(&mut ws, 1, "hub.info", json!({})).await;
    assert_eq!(info["result"]["endpoints"], expected);

    let mut anonymous = connect(&hub, None).await;
    let response = call(&mut anonymous, 1, "hub.info", json!({})).await;
    assert_eq!(response["error"]["code"], "unauthorized");
    assert!(response.get("result").is_none());

    let (payload, _lines) = open_window(&hub, vec!["read".into()]).await;
    assert_eq!(payload["ep"], expected);
}

#[tokio::test]
async fn pairing_confirm_repair_and_wrong_secret() {
    let hub = Arc::new(start_hub(Duration::from_secs(60)).await);
    seed(&hub);

    let device = Identity::generate("phone").unwrap();
    // Drive the same conversation explicitly so the test knows the secret.
    let (payload, mut lines) = open_window(&hub, vec!["read".into()]).await;
    assert_eq!(payload["v"], 1);
    assert_eq!(payload["id"], hub.hub_id);
    assert_eq!(payload["hub"], "Test Hub");
    assert_eq!(payload["sc"], json!(["read"]));
    assert_eq!(
        payload["ep"],
        json!([hub.addr.to_string(), "hub.tailnet.ts.net:8932"])
    );
    let secret = secret_of(&payload);
    let hub2 = Arc::clone(&hub);
    let device2 = Identity::generate("phone").unwrap();
    let device_task = {
        let secret = secret.clone();
        tokio::spawn(async move {
            let (ws, response) = device_pair(&hub2, &device2, &secret, "Pixel").await;
            (ws, response, device2)
        })
    };
    let line = timeout(WAIT, lines.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let HubResponse::PairConfirm { name, fingerprint } = serde_json::from_str(&line).unwrap()
    else {
        panic!("expected confirm: {line}");
    };
    assert_eq!(name, "Pixel");
    assert_eq!(fingerprint.split(' ').count(), 4);
    let stream = lines.into_inner().into_inner();
    let (read, mut write) = stream.into_split();
    sessiontap_infra::json::write_json_line(&mut write, &HubRequest::Accept { accept: true })
        .await
        .unwrap();
    let mut lines = BufReader::new(read).lines();
    let done: HubResponse =
        serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
    assert!(matches!(done, HubResponse::PairDone { ref name, .. } if name == "Pixel"));
    let (mut ws, response, device2) = device_task.await.unwrap();
    assert_eq!(response["result"]["hub_name"], "Test Hub");
    // the pairing connection is now authenticated
    let info = call(&mut ws, 3, "hub.info", json!({})).await;
    assert_eq!(info["result"]["scopes"], json!(["read"]));
    // a fresh connection with the same key is a known device
    let mut again = connect(&hub, Some(&device2)).await;
    assert!(call(&mut again, 1, "hub.info", json!({})).await["result"].is_object());
    // the window is single-use
    let (_, closed) = device_pair(&hub, &device, &secret, "Late").await;
    assert_eq!(closed["error"]["code"], "pairing_closed");

    // re-pairing the same key updates in place
    let (payload, lines) = open_window(&hub, vec![]).await;
    let secret = secret_of(&payload);
    let hub3 = Arc::clone(&hub);
    let repair =
        tokio::spawn(async move { device_pair(&hub3, &device2, &secret, "Pixel 9").await.1 });
    let (read, mut write) = lines.into_inner().into_inner().into_split();
    let mut lines = BufReader::new(read).lines();
    lines.next_line().await.unwrap().unwrap();
    sessiontap_infra::json::write_json_line(&mut write, &HubRequest::Accept { accept: true })
        .await
        .unwrap();
    lines.next_line().await.unwrap().unwrap();
    assert!(repair.await.unwrap()["result"].is_object());
    let devices = hub.hub.store.devices().unwrap();
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].name, "Pixel 9");
    assert_eq!(devices[0].scopes, vec!["read", "manage"]);

    // reject through the real CLI client: nothing is stored
    let socket = hub.socket.clone();
    let cli_task = tokio::spawn(async move {
        cli::pair(&socket, vec![], &mut Vec::new(), false, |_, _| false).await
    });
    // wrong secrets fail without prompting; the third locks the key out
    let secret = loop {
        tokio::time::sleep(Duration::from_millis(20)).await;
        if let Some(secret) = hub.hub.pairing_secret() {
            break secret;
        }
    };
    let stranger = Identity::generate("stranger").unwrap();
    for attempt in 0..3 {
        let (_, response) = device_pair(&hub, &stranger, b"wrong", "Evil").await;
        assert_eq!(
            response["error"]["code"], "pairing_failed",
            "attempt {attempt}"
        );
    }
    let (_, response) = device_pair(&hub, &stranger, &secret, "Evil").await;
    assert_eq!(response["error"]["code"], "pairing_failed");
    assert!(!cli_task.is_finished());
    // twenty failures in total, from several addresses, burn the window
    let mut failures = 3;
    for last in 1..=4u8 {
        let source = Ipv4Addr::new(127, 0, 0, last);
        let tries = if last == 1 { 2 } else { 5 };
        for _ in 0..tries {
            let fresh = Identity::generate("stranger").unwrap();
            let (_, response) = device_pair_from(&hub, &fresh, b"wrong", "Evil", source).await;
            assert_eq!(response["error"]["code"], "pairing_failed");
            failures += 1;
        }
    }
    assert_eq!(failures, 20);
    let error = timeout(WAIT, cli_task).await.unwrap().unwrap().unwrap_err();
    assert!(error.to_string().contains("too many"), "{error}");
    assert_eq!(hub.hub.store.devices().unwrap().len(), 1);
}

#[tokio::test]
async fn operator_rejection_is_reported_to_device_and_cli() {
    let hub = Arc::new(start_hub(Duration::from_secs(60)).await);
    let (payload, lines) = open_window(&hub, vec![]).await;
    let secret = secret_of(&payload);
    let hub2 = Arc::clone(&hub);
    let device = tokio::spawn(async move {
        let phone = Identity::generate("phone").unwrap();
        device_pair(&hub2, &phone, &secret, "Pixel").await.1
    });
    let (read, mut write) = lines.into_inner().into_inner().into_split();
    let mut lines = BufReader::new(read).lines();
    lines.next_line().await.unwrap().unwrap();
    sessiontap_infra::json::write_json_line(&mut write, &HubRequest::Accept { accept: false })
        .await
        .unwrap();
    let outcome: HubResponse =
        serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
    assert!(matches!(outcome, HubResponse::PairFailed { reason } if reason.contains("rejected")));
    assert_eq!(device.await.unwrap()["error"]["code"], "pairing_rejected");
    assert!(hub.hub.store.devices().unwrap().is_empty());
}

#[tokio::test]
async fn cli_pair_accepts_a_device_end_to_end() {
    let hub = Arc::new(start_hub(Duration::from_secs(60)).await);
    let socket = hub.socket.clone();
    let cli_task = tokio::spawn(async move {
        let mut out = Vec::new();
        let result = cli::pair(&socket, vec![], &mut out, false, |name, _| name == "Pixel").await;
        (result, String::from_utf8(out).unwrap())
    });
    // the test cannot scan the printed QR, so it reads the window secret
    let secret = loop {
        tokio::time::sleep(Duration::from_millis(20)).await;
        if let Some(secret) = hub.hub.pairing_secret() {
            break secret;
        }
    };
    let phone = Identity::generate("phone").unwrap();
    let (_, response) = device_pair(&hub, &phone, &secret, "Pixel").await;
    assert!(response["result"]["device_id"].is_string(), "{response}");
    let (result, out) = timeout(WAIT, cli_task).await.unwrap().unwrap();
    result.unwrap();
    assert!(out.contains("paired"), "{out}");
    assert!(out.contains('\u{2580}') || out.contains('\u{2588}') || out.contains('\u{2584}'));
}

#[tokio::test]
async fn pair_without_remote_access_fails_clearly() {
    let temp = tempfile::tempdir().unwrap();
    let (updates, _) = broadcast::channel(8);
    let hub = Arc::new(Hub::new(
        Arc::new(HubStore::memory().unwrap()),
        updates,
        None,
    ));
    let socket = temp.path().join("hub.sock");
    tokio::spawn(service::serve_unix_listener(
        UnixListener::bind(&socket).unwrap(),
        hub,
    ));
    let error = cli::pair(&socket, vec![], &mut Vec::new(), false, |_, _| true)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("not configured"), "{error}");
}

#[tokio::test]
async fn pair_without_endpoint_hints_fails_clearly() {
    let temp = tempfile::tempdir().unwrap();
    let identity = Identity::generate("hub").unwrap();
    let (updates, _) = broadcast::channel(8);
    let hub = Arc::new(Hub::new(
        Arc::new(HubStore::memory().unwrap()),
        updates,
        Some(RemoteInfo {
            hub_id: identity.spki_sha256(),
            hub_name: "hub".into(),
            hub_spki: identity.spki.clone(),
            remote: remote_config(&["0.0.0.0:8932"], &[]),
            interfaces: Vec::new,
        }),
    ));
    let socket = temp.path().join("hub.sock");
    tokio::spawn(service::serve_unix_listener(
        UnixListener::bind(&socket).unwrap(),
        Arc::clone(&hub),
    ));
    let error = timeout(
        WAIT,
        cli::pair(&socket, vec![], &mut Vec::new(), false, |_, _| true),
    )
    .await
    .unwrap()
    .unwrap_err();
    let message = format!("{error:#}");
    assert!(
        message.contains("no_endpoints") || message.contains("no endpoint hints"),
        "{message}"
    );
    assert!(message.contains("remote.advertise"), "{message}");
    assert!(hub.pairing_secret().is_none(), "a window was opened");
}

#[tokio::test]
async fn bind_that_fails_then_succeeds() {
    let blocker = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = blocker.local_addr().unwrap();
    let hub = start_supervised_hub(address, address, TEST_BACKOFF).await;
    let phone = paired(&hub, &["read"]);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let response = cli::request_once(&hub.socket, &HubRequest::Devices)
        .await
        .unwrap();
    assert!(matches!(response, HubResponse::Devices { devices, .. } if devices.len() == 1));
    drop(blocker);
    let info = info_when_bound(&hub, &phone).await;
    assert!(info["result"].is_object(), "{info}");
}

#[tokio::test]
async fn wildcard_listener_serves_loopback() {
    let port = free_port();
    let hub = start_supervised_hub(
        SocketAddr::from(([0, 0, 0, 0], port)),
        SocketAddr::from((LOCAL, port)),
        TEST_BACKOFF,
    )
    .await;
    let phone = paired(&hub, &["read"]);
    let info = info_when_bound(&hub, &phone).await;
    assert!(info["result"].is_object(), "{info}");
}

#[tokio::test]
async fn pairing_window_expires() {
    let temp = tempfile::tempdir().unwrap();
    let identity = Identity::generate("hub").unwrap();
    let (updates, _) = broadcast::channel(8);
    let hub = Arc::new(Hub::with_pair_ttl(
        Arc::new(HubStore::memory().unwrap()),
        updates,
        Some(RemoteInfo {
            hub_id: identity.spki_sha256(),
            hub_name: "hub".into(),
            hub_spki: identity.spki.clone(),
            remote: remote_config(&["127.0.0.1:8932"], &[]),
            interfaces: Vec::new,
        }),
        Duration::from_millis(200),
    ));
    let socket = temp.path().join("hub.sock");
    tokio::spawn(service::serve_unix_listener(
        UnixListener::bind(&socket).unwrap(),
        hub,
    ));
    let error = timeout(
        WAIT,
        cli::pair(&socket, vec![], &mut Vec::new(), false, |_, _| true),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert!(error.to_string().contains("expired"), "{error}");
}

fn short(deadline: Duration) -> RemoteLimits {
    RemoteLimits {
        unauthenticated_deadline: deadline,
        ..roomy(Duration::from_secs(60))
    }
}

/// Publishes a status change for the running agent.
fn publish_update(hub: &TestHub, running: &str, revision: u64) {
    let mut view = hub
        .hub
        .store
        .merged()
        .unwrap()
        .2
        .into_iter()
        .find(|agent| agent.view.invocation_id.to_string() == running)
        .unwrap()
        .view;
    view.status = PublicStatus::Idle;
    view.updated_at = Utc::now();
    let update = SourceEnvelope::Update {
        schema_version: 1,
        source_id: "host".into(),
        delivery_id: format!("d{revision}"),
        revision,
        changed: BTreeSet::from([PublicField::Status]),
        view: Box::new(view),
    };
    let request = IngestedRequest {
        method: "POST".into(),
        path: "/".into(),
        bearer: None,
        body: serde_json::to_vec(&update).unwrap(),
    };
    let publication = handle_ingest(&hub.hub.store, &IngestAuth::default(), &request)
        .publication
        .unwrap();
    hub.hub.updates.send(publication).unwrap();
}

#[tokio::test]
async fn per_address_unauthenticated_cap() {
    let hub = start_hub_with(RemoteLimits::default(), false).await;
    let anonymous = client_config(&hub.hub_id, None).unwrap();
    let mut held = Vec::new();
    for _ in 0..4 {
        held.push(tls_connect(&hub, Arc::clone(&anonymous)).await.unwrap());
    }
    assert!(tls_connect(&hub, Arc::clone(&anonymous)).await.is_err());
    let device = paired(&hub, &["read"]);
    let mut ws = connect_from(&hub, Some(&device), Ipv4Addr::new(127, 0, 0, 2)).await;
    let info = call(&mut ws, 1, "hub.info", json!({})).await;
    assert_eq!(info["result"]["hub_id"], hub.hub_id);
    drop(held);
}

#[tokio::test]
async fn unauthenticated_pool_cap() {
    let hub = start_hub_with(
        RemoteLimits {
            max_unauthenticated: 2,
            ..roomy(Duration::from_secs(60))
        },
        false,
    )
    .await;
    let anonymous = client_config(&hub.hub_id, None).unwrap();
    let first = tls_connect(&hub, Arc::clone(&anonymous)).await.unwrap();
    let _second = tls_connect_from(&hub, Arc::clone(&anonymous), Ipv4Addr::new(127, 0, 0, 2))
        .await
        .unwrap();
    assert!(
        tls_connect_from(&hub, Arc::clone(&anonymous), Ipv4Addr::new(127, 0, 0, 3))
            .await
            .is_err()
    );
    drop(first);
    let started = Instant::now();
    loop {
        if tls_connect(&hub, Arc::clone(&anonymous)).await.is_ok() {
            break;
        }
        assert!(started.elapsed() < WAIT, "slot was never freed");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn stalled_handshakes_are_closed() {
    let limit = Duration::from_millis(300);
    let slack = Duration::from_secs(2);
    let hub = start_hub_with(
        RemoteLimits {
            handshake_timeout: limit,
            ..roomy(Duration::from_secs(60))
        },
        false,
    )
    .await;
    let started = Instant::now();
    let mut tcp = TcpStream::connect(hub.addr).await.unwrap();
    let mut buffer = [0u8; 64];
    let read = timeout(limit + slack, tcp.read(&mut buffer)).await.unwrap();
    assert!(matches!(read, Ok(0) | Err(_)), "{read:?}");
    assert!(started.elapsed() >= limit);

    let mut tls = tls_connect(&hub, client_config(&hub.hub_id, None).unwrap())
        .await
        .unwrap();
    let started = Instant::now();
    let read = timeout(limit + slack, tls.read(&mut buffer)).await.unwrap();
    assert!(matches!(read, Ok(0) | Err(_)), "{read:?}");
    assert!(started.elapsed() < limit + slack);
}

#[tokio::test]
async fn oversized_message_closes_connection() {
    let hub = start_hub(Duration::from_secs(60)).await;
    let stranger = Identity::generate("stranger").unwrap();
    let mut ws = connect(&hub, Some(&stranger)).await;
    let padding = "x".repeat(65 * 1024);
    let _ = ws
        .send(Message::text(
            json!({"id": 1, "method": "pair.begin", "params": {"pad": padding}}).to_string(),
        ))
        .await;
    let (saw_text, _) = wait_closed(&mut ws).await;
    assert!(!saw_text);
}

#[tokio::test]
async fn idle_unpaired_connection_is_closed() {
    let deadline = Duration::from_millis(300);
    let hub = start_hub_with(short(deadline), false).await;
    let started = Instant::now();
    let stranger = Identity::generate("stranger").unwrap();
    let mut ws = connect(&hub, Some(&stranger)).await;
    let (_, code) = wait_closed(&mut ws).await;
    assert!(started.elapsed() >= deadline);
    assert_eq!(code, Some(1008));
}

#[tokio::test]
async fn pairing_wait_survives_deadline() {
    let deadline = Duration::from_millis(300);
    let hub = Arc::new(start_hub_with(short(deadline), false).await);
    let (payload, lines) = open_window(&hub, vec!["read".into()]).await;
    let secret = secret_of(&payload);
    let hub2 = Arc::clone(&hub);
    let device = tokio::spawn(async move {
        let phone = Identity::generate("phone").unwrap();
        device_pair(&hub2, &phone, &secret, "Pixel").await
    });
    let (read, mut write) = lines.into_inner().into_inner().into_split();
    let mut lines = BufReader::new(read).lines();
    timeout(WAIT, lines.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    tokio::time::sleep(deadline * 3).await;
    sessiontap_infra::json::write_json_line(&mut write, &HubRequest::Accept { accept: true })
        .await
        .unwrap();
    let (mut ws, response) = timeout(WAIT, device).await.unwrap().unwrap();
    assert!(response["result"]["device_id"].is_string(), "{response}");
    let info = call(&mut ws, 3, "hub.info", json!({})).await;
    assert_eq!(info["result"]["scopes"], json!(["read"]));
}

#[tokio::test]
async fn paired_connection_has_no_deadline() {
    let deadline = Duration::from_millis(200);
    let hub = start_hub_with(short(deadline), false).await;
    let device = paired(&hub, &["read"]);
    let mut ws = connect(&hub, Some(&device)).await;
    tokio::time::sleep(deadline * 3).await;
    let info = call(&mut ws, 1, "hub.info", json!({})).await;
    assert_eq!(info["result"]["hub_id"], hub.hub_id);
}

#[tokio::test]
async fn unpaired_pipelining_closes_connection() {
    let hub = start_hub(Duration::from_secs(60)).await;
    let stranger = Identity::generate("stranger").unwrap();
    let mut ws = connect(&hub, Some(&stranger)).await;
    for id in 1..=2 {
        ws.feed(Message::text(
            json!({"id": id, "method": "pair.begin"}).to_string(),
        ))
        .await
        .unwrap();
    }
    ws.flush().await.unwrap();
    let (_, code) = wait_closed(&mut ws).await;
    assert_eq!(code, Some(1008));
}

#[tokio::test]
async fn paired_inflight_limit_answers_busy() {
    let hub = start_hub_with(
        RemoteLimits {
            max_inflight_requests: 1,
            ..roomy(Duration::from_secs(60))
        },
        false,
    )
    .await;
    seed(&hub);
    let device = paired(&hub, &["read"]);
    let mut ws = connect(&hub, Some(&device)).await;
    assert!(call(&mut ws, 1, "listen", json!({})).await["result"].is_object());
    assert_eq!(recv(&mut ws).await["event"], "stream");
    // the acknowledged stream holds no permit
    assert!(call(&mut ws, 2, "hub.info", json!({})).await["result"].is_object());
    for id in 3..=4 {
        ws.feed(Message::text(
            json!({"id": id, "method": "hub.info"}).to_string(),
        ))
        .await
        .unwrap();
    }
    ws.flush().await.unwrap();
    let mut responses = HashMap::new();
    while responses.len() < 2 {
        let message = recv(&mut ws).await;
        if let Some(id) = message["id"].as_u64() {
            responses.insert(id, message);
        }
    }
    assert!(responses[&3]["result"].is_object(), "{responses:?}");
    assert_eq!(responses[&4]["error"]["code"], "busy");
    // the connection stays open
    assert!(call(&mut ws, 5, "hub.info", json!({})).await["result"].is_object());
}

#[tokio::test]
async fn unpaired_non_reader_does_not_stall_hub() {
    let deadline = Duration::from_millis(500);
    let hub = start_hub_with(short(deadline), false).await;
    let (_, running) = seed(&hub);
    let device = paired(&hub, &["read"]);
    let mut listener = connect(&hub, Some(&device)).await;
    call(&mut listener, 1, "listen", json!({})).await;
    recv(&mut listener).await;

    let started = Instant::now();
    let stranger = Identity::generate("stranger").unwrap();
    let mut silent = connect(&hub, Some(&stranger)).await;
    send(&mut silent, json!({"id": 1, "method": "pair.begin"})).await;
    publish_update(&hub, &running, 2);
    let pushed = recv(&mut listener).await;
    assert_eq!(pushed["data"]["type"], "update");
    tokio::time::sleep(deadline * 2).await;
    publish_update(&hub, &running, 3);
    assert_eq!(recv(&mut listener).await["data"]["type"], "update");
    wait_closed(&mut silent).await;
    assert!(started.elapsed() < deadline + WAIT);
}

#[tokio::test]
async fn pair_calls_are_rate_limited() {
    let hub = Arc::new(start_hub_with(roomy(Duration::from_secs(60)), true).await);
    let (payload, lines) = open_window(&hub, vec!["read".into()]).await;
    let secret = secret_of(&payload);
    let phone = Identity::generate("phone").unwrap();
    let mut ws = connect(&hub, Some(&phone)).await;
    let mut nonce = Vec::new();
    for id in 1..=5 {
        let begin = call(&mut ws, id, "pair.begin", json!({})).await;
        nonce = URL_SAFE_NO_PAD
            .decode(begin["result"]["nonce"].as_str().unwrap())
            .unwrap();
    }
    let hub_spki = hub.hub.remote.as_ref().unwrap().hub_spki.clone();
    let mac = URL_SAFE_NO_PAD.encode(pair_mac(&secret, &hub_spki, &phone.spki, &nonce));
    let params = json!({"name": "Pixel", "mac": mac});
    let limited = call(&mut ws, 6, "pair.complete", params.clone()).await;
    assert_eq!(limited["error"]["code"], "rate_limited");
    // after a refill the same proof still claims the window: nothing counted
    tokio::time::sleep(Duration::from_millis(2100)).await;
    send(
        &mut ws,
        json!({"id": 7, "method": "pair.complete", "params": params}),
    )
    .await;
    let (read, mut write) = lines.into_inner().into_inner().into_split();
    let mut lines = BufReader::new(read).lines();
    let line = timeout(WAIT, lines.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(
        matches!(
            serde_json::from_str(&line),
            Ok(HubResponse::PairConfirm { .. })
        ),
        "{line}"
    );
    sessiontap_infra::json::write_json_line(&mut write, &HubRequest::Accept { accept: true })
        .await
        .unwrap();
    assert!(recv(&mut ws).await["result"]["device_id"].is_string());
}

#[tokio::test]
async fn hostile_host_cannot_burn_window() {
    let hub = Arc::new(start_hub(Duration::from_secs(60)).await);
    let (payload, lines) = open_window(&hub, vec!["read".into()]).await;
    let secret = secret_of(&payload);
    for attempt in 0..8 {
        let fresh = Identity::generate("stranger").unwrap();
        // past the fifth failure the address is locked, even with the secret
        let proof: &[u8] = if attempt < 5 { b"wrong" } else { &secret };
        let (_, response) = device_pair(&hub, &fresh, proof, "Evil").await;
        assert_eq!(
            response["error"]["code"], "pairing_failed",
            "attempt {attempt}"
        );
    }
    let (read, mut write) = lines.into_inner().into_inner().into_split();
    let mut lines = BufReader::new(read).lines();
    // the conversation is still open and saw no prompt
    assert!(
        timeout(Duration::from_millis(200), lines.next_line())
            .await
            .is_err()
    );
    let hub2 = Arc::clone(&hub);
    let device = tokio::spawn(async move {
        let phone = Identity::generate("phone").unwrap();
        device_pair_from(&hub2, &phone, &secret, "Pixel", Ipv4Addr::new(127, 0, 0, 2))
            .await
            .1
    });
    let line = timeout(WAIT, lines.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(
        matches!(
            serde_json::from_str(&line),
            Ok(HubResponse::PairConfirm { .. })
        ),
        "{line}"
    );
    sessiontap_infra::json::write_json_line(&mut write, &HubRequest::Accept { accept: true })
        .await
        .unwrap();
    assert!(device.await.unwrap()["result"]["device_id"].is_string());
}

#[tokio::test]
async fn repairing_narrows_scopes_immediately() {
    let hub = start_hub(Duration::from_secs(60)).await;
    let (stopped, _) = seed(&hub);
    let device = paired(&hub, &["read", "manage"]);
    let mut ws = connect(&hub, Some(&device)).await;
    assert_eq!(
        call(&mut ws, 1, "hub.info", json!({})).await["result"]["scopes"],
        json!(["read", "manage"])
    );
    let sha = device.spki_sha256();
    hub.hub
        .store
        .upsert_device(&service::device_id(&sha), &sha, "Phone", &["read".into()])
        .unwrap();
    let response = call(
        &mut ws,
        2,
        "forget",
        json!({"source_id": "host", "invocation_id": stopped}),
    )
    .await;
    assert_eq!(response["error"]["code"], "forbidden");
    assert_eq!(hub.hub.store.merged().unwrap().2.len(), 2);
}

#[tokio::test]
async fn pairing_on_another_connection_authenticates_open_connection() {
    let hub = start_hub(Duration::from_secs(60)).await;
    let phone = Identity::generate("phone").unwrap();
    let mut ws = connect(&hub, Some(&phone)).await;
    let response = call(&mut ws, 1, "hub.info", json!({})).await;
    assert_eq!(response["error"]["code"], "unauthorized");
    let sha = phone.spki_sha256();
    hub.hub
        .store
        .upsert_device(&service::device_id(&sha), &sha, "Phone", &["read".into()])
        .unwrap();
    let info = call(&mut ws, 2, "hub.info", json!({})).await;
    assert_eq!(info["result"]["scopes"], json!(["read"]));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn revoke_races_inflight_forget() {
    for iteration in 0..20 {
        let hub = start_hub(Duration::from_secs(60)).await;
        let (stopped, _) = seed(&hub);
        let device = paired(&hub, &["read", "manage"]);
        let mut ws = connect(&hub, Some(&device)).await;
        assert!(call(&mut ws, 1, "hub.info", json!({})).await["result"].is_object());
        send(
            &mut ws,
            json!({"id": 2, "method": "forget", "params": {"source_id": "host", "invocation_id": stopped}}),
        )
        .await;
        let core = Arc::clone(&hub.hub);
        let id = service::device_id(&device.spki_sha256());
        tokio::task::spawn_blocking(move || core.revoke(&id).unwrap())
            .await
            .unwrap();
        let mut succeeded = false;
        let close = loop {
            match timeout(WAIT, ws.next()).await.unwrap() {
                Some(Ok(Message::Text(text))) => {
                    let message: Value = serde_json::from_str(text.as_str()).unwrap();
                    if message["id"] == 2 && message["result"].is_object() {
                        succeeded = true;
                    }
                }
                Some(Ok(Message::Close(frame))) => break frame.map(|frame| u16::from(frame.code)),
                Some(Ok(_)) => {}
                other => panic!("expected close, got {other:?}"),
            }
        };
        assert_eq!(close, Some(CLOSE_REVOKED));
        let remaining = hub.hub.store.merged().unwrap().2.len();
        // a success response means the forget happened before revoke
        // returned; a kept agent means the device got no success
        assert!(
            !succeeded || remaining == 1,
            "iteration {iteration}: success without effect"
        );
    }
}

#[tokio::test]
async fn terminal_scopes_follow_remote_control() {
    for (control, expected, listed) in [
        (
            false,
            json!(["read"]),
            "read,watch(disabled),control(disabled)",
        ),
        (
            true,
            json!(["read", "watch", "control"]),
            "read,watch,control",
        ),
    ] {
        let hub = start_hub_full(roomy(Duration::from_secs(60)), false, control).await;
        let device = paired(&hub, &["read", "watch", "control"]);
        let mut ws = connect(&hub, Some(&device)).await;
        let info = call(&mut ws, 1, "hub.info", json!({})).await;
        assert_eq!(info["result"]["scopes"], expected);
        let mut out = Vec::new();
        cli::devices(&hub.socket, &mut out).await.unwrap();
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains(listed), "{out}");
    }
}

#[tokio::test]
async fn pair_with_terminal_scope_needs_remote_control() {
    let hub = start_hub(Duration::from_secs(60)).await;
    let error = cli::pair(
        &hub.socket,
        vec!["control".into()],
        &mut Vec::new(),
        false,
        |_, _| true,
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("remote.control"), "{error}");
    assert!(hub.hub.pairing_secret().is_none());

    let hub = start_hub_full(roomy(Duration::from_secs(60)), false, true).await;
    let (payload, _lines) = open_window(&hub, vec!["control".into()]).await;
    assert_eq!(payload["sc"], json!(["read", "watch", "control"]));
}

#[tokio::test]
async fn repairing_without_read_withdraws_listen() {
    let hub = start_hub(Duration::from_secs(60)).await;
    seed(&hub);
    let device = paired(&hub, &["read", "manage"]);
    let mut ws = connect(&hub, Some(&device)).await;
    call(&mut ws, 1, "listen", json!({})).await;
    recv(&mut ws).await;
    hub.hub
        .pair_device(&device.spki_sha256(), "Phone", &[Scope::Manage])
        .unwrap();
    let (_, close) = wait_closed(&mut ws).await;
    assert_eq!(close, Some(CLOSE_SCOPE_WITHDRAWN));
    let mut ws = connect(&hub, Some(&device)).await;
    let response = call(&mut ws, 1, "listen", json!({})).await;
    assert_eq!(response["error"]["code"], "forbidden");
}

#[tokio::test]
async fn repairing_that_keeps_read_keeps_listen() {
    let hub = start_hub(Duration::from_secs(60)).await;
    let (stopped, running) = seed(&hub);
    let device = paired(&hub, &["read", "manage"]);
    let mut ws = connect(&hub, Some(&device)).await;
    call(&mut ws, 1, "listen", json!({})).await;
    recv(&mut ws).await;
    hub.hub
        .pair_device(&device.spki_sha256(), "Phone", &[Scope::Read])
        .unwrap();
    let response = call(
        &mut ws,
        2,
        "forget",
        json!({"source_id": "host", "invocation_id": stopped}),
    )
    .await;
    assert_eq!(response["error"]["code"], "forbidden");
    publish_update(&hub, &running, 3);
    let pushed = recv(&mut ws).await;
    assert_eq!(pushed["data"]["type"], "update");
}
