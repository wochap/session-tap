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
    ingest::{IngestAuth, IngestedRequest, handle_ingest},
    listen::{HubRequest, HubResponse, HubStreamEnvelope},
    remote::{CLOSE_REVOKED, serve_remote},
    service::{self, Hub, RemoteInfo, pair_mac},
    store::HubStore,
    tls::{Identity, client_config, client_config_with_versions, server_config},
};
use std::{collections::BTreeSet, net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    net::{TcpListener, TcpStream, UnixListener, UnixStream},
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

async fn start_hub(ping: Duration) -> TestHub {
    let temp = tempfile::tempdir().unwrap();
    let identity = Identity::generate("test-hub").unwrap();
    let hub_id = identity.spki_sha256();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (updates, _) = broadcast::channel(64);
    let hub = Arc::new(Hub::new(
        Arc::new(HubStore::memory().unwrap()),
        updates,
        Some(RemoteInfo {
            hub_id: hub_id.clone(),
            hub_name: "Test Hub".into(),
            hub_spki: identity.spki.clone(),
            endpoints: vec![addr.to_string(), "hub.tailnet.ts.net:8932".into()],
        }),
    ));
    let acceptor = tokio_rustls::TlsAcceptor::from(server_config(&identity).unwrap());
    tokio::spawn(serve_remote(listener, acceptor, Arc::clone(&hub), ping));
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

async fn tls_connect(
    hub: &TestHub,
    config: Arc<tokio_rustls::rustls::ClientConfig>,
) -> std::io::Result<TlsStream<TcpStream>> {
    let tcp = TcpStream::connect(hub.addr).await?;
    TlsConnector::from(config)
        .connect(ServerName::try_from("hub").unwrap(), tcp)
        .await
}

async fn connect(hub: &TestHub, client: Option<&Identity>) -> Ws {
    let tls = tls_connect(hub, client_config(&hub.hub_id, client).unwrap())
        .await
        .unwrap();
    tokio_tungstenite::client_async("wss://hub/", tls)
        .await
        .unwrap()
        .0
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
    assert!(matches!(response, HubResponse::Devices { devices } if devices.len() == 1));
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
    let mut ws = connect(hub, Some(device)).await;
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
    // wrong secrets fail without prompting; the third closes the window
    tokio::time::sleep(Duration::from_millis(100)).await;
    let stranger = Identity::generate("stranger").unwrap();
    for attempt in 0..3 {
        let (_, response) = device_pair(&hub, &stranger, b"wrong", "Evil").await;
        assert_eq!(
            response["error"]["code"], "pairing_failed",
            "attempt {attempt}"
        );
    }
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
            endpoints: vec![],
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
