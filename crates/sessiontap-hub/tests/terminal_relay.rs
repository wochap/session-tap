//! Terminal relay over a real hub: source control channels on the
//! ingestion listener and device terminal methods on the remote listener.

use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use sessiontap_core::{
    domain::{InvocationId, PublicAgentView, PublicStatus},
    protocol::{RelayMessage, RelayRequest, SourceEnvelope, SourceIdentity},
    terminal::{Cursor, InputState, TerminalFrame},
};
use sessiontap_hub::{
    config::{RemoteConfig, SourceAuth},
    ingest::{self, IngestAuth, IngestedRequest, handle_ingest},
    relay::Relay,
    remote::{CLOSE_REVOKED, CLOSE_SCOPE_WITHDRAWN, RemoteGate, RemoteLimits, serve_remote},
    scope::Scope,
    service::{self, Hub, RemoteInfo},
    store::HubStore,
    tls::{Identity, client_config, server_config},
};
use std::{
    collections::BTreeMap, net::SocketAddr, os::unix::fs::PermissionsExt, sync::Arc, time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::broadcast,
    time::timeout,
};
use tokio_rustls::{TlsConnector, client::TlsStream, rustls::pki_types::ServerName};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream,
    tungstenite::{Message, client::IntoClientRequest},
};

type Phone = WebSocketStream<TlsStream<TcpStream>>;
type Source = WebSocketStream<MaybeTlsStream<TcpStream>>;

const WAIT: Duration = Duration::from_secs(5);

struct TestHub {
    hub: Arc<Hub>,
    hub_id: String,
    remote: SocketAddr,
    ingest: SocketAddr,
    _temp: tempfile::TempDir,
}

struct Options {
    control: bool,
    /// `(source, token)` pairs; empty means tokenless loopback ingestion.
    tokens: &'static [(&'static str, &'static str)],
    answer_timeout: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            control: true,
            tokens: &[],
            answer_timeout: Duration::from_secs(5),
        }
    }
}

async fn start_hub(options: Options) -> TestHub {
    let temp = tempfile::tempdir().unwrap();
    let identity = Identity::generate("test-hub").unwrap();
    let hub_id = identity.spki_sha256();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let remote = listener.local_addr().unwrap();
    let (updates, _) = broadcast::channel(64);
    let hub = Arc::new(
        Hub::new(
            Arc::new(HubStore::memory().unwrap()),
            updates,
            Some(RemoteInfo {
                hub_id: hub_id.clone(),
                hub_name: "Test Hub".into(),
                hub_spki: identity.spki.clone(),
                remote: RemoteConfig {
                    name: None,
                    listen: vec![remote.to_string()],
                    advertise: Vec::new(),
                    control: options.control,
                    discovery: false,
                },
                interfaces: Vec::new,
            }),
        )
        .with_relay(Arc::new(Relay::new(options.answer_timeout))),
    );
    let limits = RemoteLimits {
        max_unauthenticated: 64,
        max_unauthenticated_per_address: 64,
        ..RemoteLimits::default()
    };
    tokio::spawn(serve_remote(
        listener,
        tokio_rustls::TlsAcceptor::from(server_config(&identity).unwrap()),
        Arc::clone(&hub),
        RemoteGate::new(&limits),
        limits,
    ));
    let mut sources = BTreeMap::new();
    for (source, token) in options.tokens {
        let path = temp.path().join(format!("{source}.token"));
        std::fs::write(&path, token).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        sources.insert(
            (*source).to_owned(),
            SourceAuth {
                token_file: path.to_string_lossy().into_owned(),
            },
        );
    }
    let auth = Arc::new(IngestAuth::new(&sources));
    let ingest_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let ingest = ingest_listener.local_addr().unwrap();
    {
        let hub = Arc::clone(&hub);
        tokio::spawn(async move {
            loop {
                let (stream, _) = ingest_listener.accept().await.unwrap();
                let store = Arc::clone(&hub.store);
                let relay = Arc::clone(&hub.relay);
                let auth = Arc::clone(&auth);
                tokio::spawn(ingest::serve_connection(
                    stream,
                    store,
                    auth,
                    1024 * 1024,
                    relay,
                ));
            }
        });
    }
    TestHub {
        hub,
        hub_id,
        remote,
        ingest,
        _temp: temp,
    }
}

fn view() -> PublicAgentView {
    PublicAgentView {
        invocation_id: InvocationId::new(),
        provider: "claude".into(),
        status: PublicStatus::Running,
        reason: None,
        cwd: "/tmp".into(),
        created_at: Utc::now(),
        updated_at: Utc::now(),
        session: None,
        metadata: None,
        usage: None,
        repository: None,
        children: None,
        terminal: None,
    }
}

/// Seeds one running agent of `source`; returns its invocation ID.
fn seed(hub: &TestHub, source: &str) -> String {
    let agent = view();
    let id = agent.invocation_id.to_string();
    let request = IngestedRequest {
        method: "POST".into(),
        path: "/ingest".into(),
        bearer: None,
        body: serde_json::to_vec(&SourceEnvelope::Snapshot {
            schema_version: 1,
            source: SourceIdentity {
                id: source.into(),
                display_name: None,
            },
            revision: 1,
            views: vec![agent],
        })
        .unwrap(),
    };
    assert_eq!(
        handle_ingest(&hub.hub.store, &IngestAuth::default(), &request).status,
        200
    );
    id
}

fn paired(hub: &TestHub, scopes: &[&str]) -> Identity {
    let identity = Identity::generate("phone").unwrap();
    let sha = identity.spki_sha256();
    // stored as pairing stores them, with implied scopes
    let scopes: Vec<String> = scopes.iter().map(|scope| (*scope).to_owned()).collect();
    let scopes = Scope::names(&Scope::parse_request(&scopes).unwrap());
    hub.hub
        .store
        .upsert_device(&service::device_id(&sha), &sha, "Phone", &scopes)
        .unwrap();
    identity
}

async fn phone(hub: &TestHub, identity: &Identity) -> Phone {
    let tcp = TcpStream::connect(hub.remote).await.unwrap();
    let tls = TlsConnector::from(client_config(&hub.hub_id, Some(identity)).unwrap())
        .connect(ServerName::try_from("hub").unwrap(), tcp)
        .await
        .unwrap();
    tokio_tungstenite::client_async("wss://hub/", tls)
        .await
        .unwrap()
        .0
}

async fn recv<S>(ws: &mut WebSocketStream<S>) -> Value
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    loop {
        match timeout(WAIT, ws.next()).await.unwrap().unwrap().unwrap() {
            Message::Text(text) => return serde_json::from_str(text.as_str()).unwrap(),
            Message::Ping(_) | Message::Pong(_) => {}
            other => panic!("unexpected frame {other:?}"),
        }
    }
}

async fn call(ws: &mut Phone, id: u64, method: &str, params: Value) -> Value {
    ws.send(Message::text(
        json!({"id": id, "method": method, "params": params}).to_string(),
    ))
    .await
    .unwrap();
    let response = recv(ws).await;
    assert_eq!(response["id"], id, "{response}");
    response
}

async fn wait_close_code(ws: &mut Phone) -> Option<u16> {
    loop {
        match timeout(WAIT, ws.next()).await.expect("closes") {
            Some(Ok(Message::Close(frame))) => return frame.map(|frame| u16::from(frame.code)),
            Some(Ok(_)) => {}
            Some(Err(_)) | None => return None,
        }
    }
}

/// Dials the control channel with an optional bearer token.
async fn dial(hub: &TestHub, token: Option<&str>) -> Result<Source, u16> {
    let mut request = format!("ws://{}/control", hub.ingest)
        .into_client_request()
        .unwrap();
    if let Some(token) = token {
        request
            .headers_mut()
            .insert("authorization", format!("Bearer {token}").parse().unwrap());
    }
    match tokio_tungstenite::connect_async(request).await {
        Ok((ws, _)) => Ok(ws),
        Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
            Err(response.status().as_u16())
        }
        Err(error) => panic!("{error}"),
    }
}

async fn hello(ws: &mut Source, source: &str) {
    send_source(
        ws,
        &RelayMessage::Hello {
            source_id: source.into(),
            protocol: 1,
        },
    )
    .await;
}

async fn send_source(ws: &mut Source, message: &RelayMessage) {
    ws.send(Message::text(serde_json::to_string(message).unwrap()))
        .await
        .unwrap();
}

async fn source_request(ws: &mut Source) -> RelayRequest {
    serde_json::from_value(recv(ws).await).unwrap()
}

/// A source channel bound to `source`, once the hub has attached it.
async fn bound_source(hub: &TestHub, source: &str, token: Option<&str>) -> Source {
    let mut ws = dial(hub, token).await.unwrap();
    hello(&mut ws, source).await;
    timeout(WAIT, async {
        while !hub.hub.relay.has_channel(source) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("channel binds");
    ws
}

fn snapshot(seq: u64) -> TerminalFrame {
    TerminalFrame::Snapshot {
        seq,
        cols: 80,
        rows: 24,
        cursor: Cursor {
            x: 0,
            y: 0,
            visible: true,
        },
        alternate_screen: false,
        data: b"1. Yes\r\n2. No".to_vec(),
        input: InputState::from_guard(None),
    }
}

/// Opens a terminal: the stub source answers and sends a snapshot.
async fn open_stream(phone: &mut Phone, source: &mut Source, id: u64, invocation: &str) -> u64 {
    phone
        .send(Message::text(
            json!({"id": id, "method": "terminal.open",
                   "params": {"source_id": "host", "invocation_id": invocation}})
            .to_string(),
        ))
        .await
        .unwrap();
    let RelayRequest::Open {
        req,
        stream,
        invocation_id,
    } = source_request(source).await
    else {
        panic!("expected open");
    };
    assert_eq!(invocation_id, invocation);
    send_source(source, &RelayMessage::Opened { req, stream }).await;
    send_source(
        source,
        &RelayMessage::Frame {
            stream,
            frame: snapshot(0),
        },
    )
    .await;
    let response = recv(phone).await;
    assert_eq!(response["id"], id, "{response}");
    assert_eq!(response["result"]["stream"], stream);
    let pushed = recv(phone).await;
    assert_eq!(pushed["type"], "terminal");
    assert_eq!(pushed["frame"]["type"], "snapshot");
    stream
}

#[tokio::test]
async fn unauthenticated_upgrade_is_401() {
    let hub = start_hub(Options {
        tokens: &[("host", "host-token")],
        ..Options::default()
    })
    .await;
    assert_eq!(dial(&hub, None).await.err(), Some(401));
    assert_eq!(dial(&hub, Some("wrong")).await.err(), Some(401));
    let mut raw = TcpStream::connect(hub.ingest).await.unwrap();
    raw.write_all(b"GET /control HTTP/1.1\r\nhost: hub\r\nupgrade: websocket\r\nconnection: Upgrade\r\nsec-websocket-key: dGhlIHNhbXBsZSBub25jZQ==\r\nsec-websocket-version: 13\r\n\r\n")
        .await
        .unwrap();
    let mut response = String::new();
    raw.read_to_string(&mut response).await.unwrap();
    assert!(response.starts_with("HTTP/1.1 401"), "{response}");
    assert!(
        response.contains(r#"{"error":"unauthorized"}"#),
        "{response}"
    );
    // ingestion itself is unchanged
    let mut health = TcpStream::connect(hub.ingest).await.unwrap();
    health
        .write_all(b"GET /health HTTP/1.1\r\nhost: hub\r\n\r\n")
        .await
        .unwrap();
    let mut response = String::new();
    health.read_to_string(&mut response).await.unwrap();
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
}

#[tokio::test]
async fn token_binds_only_its_own_sources() {
    let hub = start_hub(Options {
        tokens: &[("host", "host-token"), ("sandbox", "sandbox-token")],
        ..Options::default()
    })
    .await;
    let mut forbidden = dial(&hub, Some("sandbox-token")).await.unwrap();
    hello(&mut forbidden, "host").await;
    match timeout(WAIT, forbidden.next()).await.unwrap() {
        Some(Ok(Message::Close(Some(frame)))) => {
            assert_eq!(frame.reason.as_str(), "source_not_permitted");
        }
        other => panic!("expected close, got {other:?}"),
    }
    assert!(!hub.hub.relay.has_channel("host"));
    let _permitted = bound_source(&hub, "host", Some("host-token")).await;
    assert!(hub.hub.relay.has_channel("host"));
}

#[tokio::test]
async fn newer_channel_replaces_older_and_ends_its_streams() {
    let hub = start_hub(Options::default()).await;
    let invocation = seed(&hub, "host");
    let identity = paired(&hub, &["watch"]);
    let mut device = phone(&hub, &identity).await;
    let mut old = bound_source(&hub, "host", None).await;
    let stream = open_stream(&mut device, &mut old, 1, &invocation).await;
    let mut new = dial(&hub, None).await.unwrap();
    hello(&mut new, "host").await;
    let ended = recv(&mut device).await;
    assert_eq!(ended["stream"], stream);
    assert_eq!(
        ended["frame"],
        json!({"type": "ended", "reason": "source_unavailable"})
    );
    // the old channel is closed by the hub
    loop {
        match timeout(WAIT, old.next()).await.unwrap() {
            Some(Ok(Message::Close(_)) | Err(_)) | None => break,
            Some(Ok(_)) => {}
        }
    }
    // the new channel serves the next open
    open_stream(&mut device, &mut new, 2, &invocation).await;
}

#[tokio::test]
async fn silent_source_answers_source_unavailable() {
    let hub = start_hub(Options {
        answer_timeout: Duration::from_millis(200),
        ..Options::default()
    })
    .await;
    let invocation = seed(&hub, "host");
    let identity = paired(&hub, &["watch"]);
    let mut device = phone(&hub, &identity).await;
    let mut source = bound_source(&hub, "host", None).await;
    let response = call(
        &mut device,
        1,
        "terminal.open",
        json!({"source_id": "host", "invocation_id": invocation}),
    )
    .await;
    assert_eq!(response["error"]["code"], "source_unavailable");
    assert!(matches!(
        source_request(&mut source).await,
        RelayRequest::Open { .. }
    ));
    // the abandoned stream is released on the source
    assert!(matches!(
        source_request(&mut source).await,
        RelayRequest::Close { .. }
    ));
}

#[tokio::test]
async fn terminal_methods_check_effective_scopes() {
    for (control, scopes, expected) in [
        (true, &["read", "manage"][..], Some("forbidden")),
        (false, &["read", "watch", "control"][..], Some("forbidden")),
        (true, &["control"][..], None),
    ] {
        let hub = start_hub(Options {
            control,
            ..Options::default()
        })
        .await;
        let invocation = seed(&hub, "host");
        let identity = paired(&hub, scopes);
        let mut device = phone(&hub, &identity).await;
        let mut source = bound_source(&hub, "host", None).await;
        match expected {
            Some(code) => {
                let response = call(
                    &mut device,
                    1,
                    "terminal.open",
                    json!({"source_id": "host", "invocation_id": invocation}),
                )
                .await;
                assert_eq!(response["error"]["code"], code, "{scopes:?}");
                assert_eq!(
                    hub.hub.relay.stream_count(),
                    0,
                    "nothing sent to the source"
                );
            }
            None => {
                open_stream(&mut device, &mut source, 1, &invocation).await;
            }
        }
    }
}

#[tokio::test]
async fn open_refusals_name_the_reason() {
    let hub = start_hub(Options::default()).await;
    let invocation = seed(&hub, "host");
    let identity = paired(&hub, &["watch"]);
    let mut device = phone(&hub, &identity).await;
    let missing = call(
        &mut device,
        1,
        "terminal.open",
        json!({"source_id": "host", "invocation_id": "nope"}),
    )
    .await;
    assert_eq!(missing["error"]["code"], "not_found");
    let no_channel = call(
        &mut device,
        2,
        "terminal.open",
        json!({"source_id": "host", "invocation_id": invocation}),
    )
    .await;
    assert_eq!(no_channel["error"]["code"], "source_disallows_control");
    let mut source = bound_source(&hub, "host", None).await;
    device
        .send(Message::text(
            json!({"id": 3, "method": "terminal.open",
                   "params": {"source_id": "host", "invocation_id": invocation}})
            .to_string(),
        ))
        .await
        .unwrap();
    let RelayRequest::Open { req, .. } = source_request(&mut source).await else {
        panic!("expected open");
    };
    send_source(
        &mut source,
        &RelayMessage::Error {
            req,
            code: "source_disallows_control".into(),
            message: "control is off".into(),
        },
    )
    .await;
    let refused = recv(&mut device).await;
    assert_eq!(refused["error"]["code"], "source_disallows_control");
}

#[tokio::test]
async fn frames_follow_the_open_answer_in_source_order() {
    let hub = start_hub(Options::default()).await;
    let invocation = seed(&hub, "host");
    let identity = paired(&hub, &["control"]);
    let mut device = phone(&hub, &identity).await;
    let mut source = bound_source(&hub, "host", None).await;
    let stream = open_stream(&mut device, &mut source, 1, &invocation).await;
    for seq in 1..=3 {
        send_source(
            &mut source,
            &RelayMessage::Frame {
                stream,
                frame: TerminalFrame::Output {
                    seq,
                    data: format!("line {seq}").into_bytes(),
                },
            },
        )
        .await;
    }
    for seq in 1..=3 {
        let pushed = recv(&mut device).await;
        assert!(pushed.get("id").is_none());
        assert_eq!(pushed["frame"]["type"], "output");
        assert_eq!(pushed["frame"]["seq"], seq);
    }
    device
        .send(Message::text(
            json!({"id": 2, "method": "terminal.input", "params": {"stream": stream, "keys": ["1"]}})
                .to_string(),
        ))
        .await
        .unwrap();
    let RelayRequest::Input { req, input, .. } = source_request(&mut source).await else {
        panic!("expected input");
    };
    assert_eq!(serde_json::to_value(input).unwrap(), json!({"keys": ["1"]}));
    send_source(&mut source, &RelayMessage::InputResult { req, code: None }).await;
    assert_eq!(recv(&mut device).await["result"], json!({}));
    device
        .send(Message::text(
            json!({"id": 3, "method": "terminal.input", "params": {"stream": stream, "paste": {"text": "hi", "enter": true}}})
                .to_string(),
        ))
        .await
        .unwrap();
    let RelayRequest::Input { req, .. } = source_request(&mut source).await else {
        panic!("expected input");
    };
    send_source(
        &mut source,
        &RelayMessage::InputResult {
            req,
            code: Some("not_foreground".into()),
        },
    )
    .await;
    assert_eq!(recv(&mut device).await["error"]["code"], "not_foreground");
    device
        .send(Message::text(
            json!({"id": 4, "method": "terminal.close", "params": {"stream": stream}}).to_string(),
        ))
        .await
        .unwrap();
    // the ended frame is pushed before the close answer
    assert_eq!(recv(&mut device).await["frame"]["reason"], "closed");
    assert_eq!(recv(&mut device).await["result"], json!({}));
    assert_eq!(
        source_request(&mut source).await,
        RelayRequest::Close { stream }
    );
}

#[tokio::test]
async fn foreign_streams_and_disconnects() {
    let hub = start_hub(Options::default()).await;
    let invocation = seed(&hub, "host");
    let mut owner = phone(&hub, &paired(&hub, &["control"])).await;
    let mut other = phone(&hub, &paired(&hub, &["control"])).await;
    let mut source = bound_source(&hub, "host", None).await;
    let stream = open_stream(&mut owner, &mut source, 1, &invocation).await;
    let response = call(
        &mut other,
        1,
        "terminal.input",
        json!({"stream": stream, "keys": ["1"]}),
    )
    .await;
    assert_eq!(response["error"]["code"], "forbidden");
    drop(owner);
    assert_eq!(
        source_request(&mut source).await,
        RelayRequest::Close { stream },
        "the disconnect releases the stream; foreign input was never forwarded"
    );
}

#[tokio::test]
async fn revoke_releases_streams_before_returning() {
    let hub = start_hub(Options::default()).await;
    let invocation = seed(&hub, "host");
    let identity = paired(&hub, &["control"]);
    let mut device = phone(&hub, &identity).await;
    let mut source = bound_source(&hub, "host", None).await;
    let stream = open_stream(&mut device, &mut source, 1, &invocation).await;
    let device_id = service::device_id(&identity.spki_sha256());
    hub.hub.revoke(&device_id).unwrap();
    assert_eq!(hub.hub.relay.stream_count(), 0);
    assert_eq!(
        source_request(&mut source).await,
        RelayRequest::Close { stream }
    );
    // input after revoke is never forwarded
    let _ = device
        .send(Message::text(
            json!({"id": 2, "method": "terminal.input", "params": {"stream": stream, "keys": ["1"]}})
                .to_string(),
        ))
        .await;
    assert_eq!(wait_close_code(&mut device).await, Some(CLOSE_REVOKED));
    assert!(
        timeout(Duration::from_millis(200), source.next())
            .await
            .is_err(),
        "nothing reaches the source after revoke"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn revoke_during_input_forwards_or_answers_nothing() {
    for _ in 0..10 {
        let hub = start_hub(Options::default()).await;
        let invocation = seed(&hub, "host");
        let identity = paired(&hub, &["control"]);
        let mut device = phone(&hub, &identity).await;
        let mut source = bound_source(&hub, "host", None).await;
        let stream = open_stream(&mut device, &mut source, 1, &invocation).await;
        let device_id = service::device_id(&identity.spki_sha256());
        device
            .send(Message::text(
                json!({"id": 2, "method": "terminal.input", "params": {"stream": stream, "keys": ["1"]}})
                    .to_string(),
            ))
            .await
            .unwrap();
        let revoker = {
            let hub = Arc::clone(&hub.hub);
            tokio::task::spawn_blocking(move || hub.revoke(&device_id).unwrap())
        };
        revoker.await.unwrap();
        // after revoke returns: input, if forwarded, precedes the close
        let mut forwarded = None;
        loop {
            match source_request(&mut source).await {
                RelayRequest::Input { req, .. } => forwarded = Some(req),
                RelayRequest::Close { stream: closed } => {
                    assert_eq!(closed, stream);
                    break;
                }
                other => panic!("unexpected {other:?}"),
            }
        }
        if let Some(req) = forwarded {
            send_source(&mut source, &RelayMessage::InputResult { req, code: None }).await;
        }
        let mut success = false;
        loop {
            match timeout(WAIT, device.next()).await.expect("closes") {
                Some(Ok(Message::Text(text))) => {
                    let value: Value = serde_json::from_str(text.as_str()).unwrap();
                    if value["id"] == 2 && value.get("result").is_some() {
                        success = true;
                    }
                }
                Some(Ok(Message::Close(_)) | Err(_)) | None => break,
                Some(Ok(_)) => {}
            }
        }
        assert!(
            forwarded.is_some() || !success,
            "a success answer implies the input was forwarded"
        );
    }
}

#[tokio::test]
async fn repair_without_control_keeps_streams_and_refuses_input() {
    let hub = start_hub(Options::default()).await;
    let invocation = seed(&hub, "host");
    let identity = paired(&hub, &["control"]);
    let mut device = phone(&hub, &identity).await;
    let mut source = bound_source(&hub, "host", None).await;
    let stream = open_stream(&mut device, &mut source, 1, &invocation).await;
    hub.hub
        .pair_device(
            &identity.spki_sha256(),
            "Phone",
            &[Scope::Read, Scope::Watch],
        )
        .unwrap();
    send_source(
        &mut source,
        &RelayMessage::Frame {
            stream,
            frame: TerminalFrame::Output {
                seq: 1,
                data: b"still here".to_vec(),
            },
        },
    )
    .await;
    assert_eq!(recv(&mut device).await["frame"]["seq"], 1);
    let response = call(
        &mut device,
        2,
        "terminal.input",
        json!({"stream": stream, "keys": ["1"]}),
    )
    .await;
    assert_eq!(response["error"]["code"], "forbidden");
    assert_eq!(hub.hub.relay.stream_count(), 1);
}

#[tokio::test]
async fn repair_without_watch_closes_4403_and_releases() {
    let hub = start_hub(Options::default()).await;
    let invocation = seed(&hub, "host");
    let identity = paired(&hub, &["watch"]);
    let mut device = phone(&hub, &identity).await;
    let mut source = bound_source(&hub, "host", None).await;
    let stream = open_stream(&mut device, &mut source, 1, &invocation).await;
    hub.hub
        .pair_device(&identity.spki_sha256(), "Phone", &[Scope::Read])
        .unwrap();
    assert_eq!(
        source_request(&mut source).await,
        RelayRequest::Close { stream }
    );
    assert_eq!(
        wait_close_code(&mut device).await,
        Some(CLOSE_SCOPE_WITHDRAWN)
    );
}

#[tokio::test]
async fn control_channel_drop_ends_streams() {
    let hub = start_hub(Options::default()).await;
    let invocation = seed(&hub, "host");
    let mut device = phone(&hub, &paired(&hub, &["watch"])).await;
    let mut source = bound_source(&hub, "host", None).await;
    let stream = open_stream(&mut device, &mut source, 1, &invocation).await;
    source.close(None).await.unwrap();
    let ended = recv(&mut device).await;
    assert_eq!(ended["stream"], stream);
    assert_eq!(ended["frame"]["reason"], "source_unavailable");
}

/// Ingests through the normal path and publishes like the ingestion
/// listener does.
fn ingest_and_publish(hub: &TestHub, envelope: &SourceEnvelope) {
    let request = IngestedRequest {
        method: "POST".into(),
        path: "/ingest".into(),
        bearer: None,
        body: serde_json::to_vec(envelope).unwrap(),
    };
    let outcome = handle_ingest(&hub.hub.store, &IngestAuth::default(), &request);
    assert_eq!(outcome.status, 200, "{}", outcome.body);
    if let Some(publication) = outcome.publication {
        let _ = hub.hub.updates.send(publication);
    }
}

#[tokio::test]
async fn terminal_descriptor_is_carried_end_to_end() {
    use sessiontap_core::{
        domain::PublicField,
        terminal::{QuickPick, TerminalDescriptor},
    };
    use sessiontap_hub::listen::HubRequest;
    use tokio::io::{AsyncBufReadExt, BufReader};
    let hub = start_hub(Options::default()).await;
    let mut agent = view();
    agent.terminal = Some(TerminalDescriptor {
        quick_pick: QuickPick::Digits,
    });
    ingest_and_publish(
        &hub,
        &SourceEnvelope::Snapshot {
            schema_version: 1,
            source: SourceIdentity {
                id: "host".into(),
                display_name: None,
            },
            revision: 1,
            views: vec![agent.clone()],
        },
    );
    let (_, _, merged) = hub.hub.store.merged().unwrap();
    assert_eq!(merged[0].view.terminal, agent.terminal, "store round-trip");

    let socket = hub._temp.path().join("hub.sock");
    let unix = tokio::net::UnixListener::bind(&socket).unwrap();
    tokio::spawn(service::serve_unix_listener(unix, Arc::clone(&hub.hub)));
    let mut stream = tokio::net::UnixStream::connect(&socket).await.unwrap();
    sessiontap_infra::json::write_json_line(&mut stream, &HubRequest::Listen)
        .await
        .unwrap();
    let mut lines = BufReader::new(stream).lines();
    let mut unix_next = async || -> Value {
        let line = timeout(WAIT, lines.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        serde_json::from_str(&line).unwrap()
    };
    let unix_snapshot = unix_next().await;
    assert_eq!(
        unix_snapshot["agents"][0]["view"]["terminal"],
        json!({"quick_pick": "digits"})
    );

    let mut device = phone(&hub, &paired(&hub, &["read"])).await;
    assert!(call(&mut device, 1, "listen", json!({})).await["result"].is_object());
    let remote_snapshot = recv(&mut device).await;
    assert_eq!(
        remote_snapshot["data"]["agents"][0]["view"]["terminal"],
        json!({"quick_pick": "digits"})
    );

    let mut stopped = agent;
    stopped.status = PublicStatus::Stopped;
    stopped.terminal = None;
    stopped.updated_at = Utc::now() + chrono::Duration::seconds(1);
    ingest_and_publish(
        &hub,
        &SourceEnvelope::Update {
            schema_version: 1,
            source_id: "host".into(),
            delivery_id: "d-2".into(),
            revision: 2,
            changed: std::collections::BTreeSet::from([PublicField::Status, PublicField::Terminal]),
            view: Box::new(stopped),
        },
    );
    let (_, _, merged) = hub.hub.store.merged().unwrap();
    assert!(merged[0].view.terminal.is_none(), "persisted without it");
    for update in [unix_next().await, recv(&mut device).await["data"].clone()] {
        assert_eq!(update["type"], "update", "{update}");
        assert!(
            update["changed"]
                .as_array()
                .unwrap()
                .contains(&json!("terminal")),
            "{update}"
        );
        assert!(update["view"].get("terminal").is_none(), "{update}");
    }
}
