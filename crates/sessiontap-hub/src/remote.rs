//! Remote device listener: TLS 1.3 + WebSocket, JSON requests and pushed
//! stream events. Authorization comes from the client certificate SPKI.

use anyhow::{Context, Result};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{
    net::SocketAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::{mpsc, oneshot},
};
use tokio_rustls::TlsAcceptor;
use tokio_tungstenite::tungstenite::{
    Message,
    protocol::{CloseFrame, frame::coding::CloseCode},
};
use tokio_util::sync::CancellationToken;

use crate::listen::{HubStreamEnvelope, StreamSink, stream_merged};
use crate::service::{Hub, PairAttempt, PairEvent, SCOPE_MANAGE, SCOPE_READ, fingerprint};
use crate::store::{Device, ForgetOutcome};
use crate::tls::sha256_hex;

pub const PROTOCOL_VERSION: u32 = 1;
/// WebSocket close code sent when a device is revoked.
pub const CLOSE_REVOKED: u16 = 4401;
pub const PING_INTERVAL: Duration = Duration::from_secs(60);
const LAST_SEEN_THROTTLE: Duration = Duration::from_secs(60);
const OUTBOUND_CAPACITY: usize = 256;

/// Binds every address, logging (and skipping) the ones that fail.
pub async fn bind_all(addresses: &[SocketAddr]) -> Vec<TcpListener> {
    let mut listeners = Vec::new();
    for address in addresses {
        match TcpListener::bind(address).await {
            Ok(listener) => listeners.push(listener),
            Err(error) => {
                eprintln!("sessiontap-hub: cannot bind remote address {address}: {error}");
            }
        }
    }
    listeners
}

/// Accepts remote connections on one listener forever.
pub async fn serve_remote(
    listener: TcpListener,
    acceptor: TlsAcceptor,
    hub: Arc<Hub>,
    ping_interval: Duration,
) -> Result<()> {
    loop {
        let (tcp, _) = listener.accept().await?;
        let acceptor = acceptor.clone();
        let hub = Arc::clone(&hub);
        tokio::spawn(async move {
            if let Err(error) = serve_connection(tcp, acceptor, hub, ping_interval).await {
                eprintln!("sessiontap-hub: remote connection ended: {error:#}");
            }
        });
    }
}

struct Connection {
    hub: Arc<Hub>,
    device_spki: Option<Vec<u8>>,
    device: Mutex<Option<Device>>,
    nonce: Mutex<Option<[u8; 32]>>,
    streaming: AtomicBool,
    out: mpsc::Sender<Message>,
    cancel: CancellationToken,
}

impl Connection {
    async fn send(&self, value: &Value) {
        let _ = self.out.send(Message::text(value.to_string())).await;
    }

    fn device(&self) -> Option<Device> {
        self.device.lock().expect("device mutex poisoned").clone()
    }
}

/// Serves one remote connection from TLS handshake to close.
pub async fn serve_connection(
    tcp: TcpStream,
    acceptor: TlsAcceptor,
    hub: Arc<Hub>,
    ping_interval: Duration,
) -> Result<()> {
    let tls = acceptor.accept(tcp).await.context("tls handshake")?;
    let device_spki = tls
        .get_ref()
        .1
        .peer_certificates()
        .and_then(|certs| certs.first())
        .and_then(|cert| crate::tls::spki_of(cert).ok());
    let device = match &device_spki {
        Some(spki) => hub.store.device_by_spki(&sha256_hex(spki))?,
        None => None,
    };
    let ws = tokio_tungstenite::accept_async(tls)
        .await
        .context("websocket upgrade")?;
    let (mut ws_tx, mut ws_rx) = ws.split();
    let (out, mut outbound) = mpsc::channel::<Message>(OUTBOUND_CAPACITY);
    let cancel = CancellationToken::new();
    if let Some(device) = &device {
        hub.register_connection(&device.device_id, cancel.clone());
        let _ = hub.store.touch_device(&device.device_id);
    }
    let connection = Arc::new(Connection {
        hub: Arc::clone(&hub),
        device_spki,
        device: Mutex::new(device),
        nonce: Mutex::new(None),
        streaming: AtomicBool::new(false),
        out,
        cancel: cancel.clone(),
    });
    let mut ping =
        tokio::time::interval_at(tokio::time::Instant::now() + ping_interval, ping_interval);
    let mut awaiting_pong = false;
    let mut last_touch = Instant::now();
    let result = loop {
        tokio::select! {
            () = cancel.cancelled() => {
                let _ = ws_tx
                    .send(Message::Close(Some(CloseFrame {
                        code: CloseCode::from(CLOSE_REVOKED),
                        reason: "device revoked".into(),
                    })))
                    .await;
                break Ok(());
            }
            Some(message) = outbound.recv() => {
                if let Err(error) = ws_tx.send(message).await {
                    break Err(error.into());
                }
            }
            incoming = ws_rx.next() => match incoming {
                Some(Ok(Message::Text(text))) => {
                    let connection = Arc::clone(&connection);
                    tokio::spawn(async move { handle_request(&connection, text.as_str()).await });
                }
                Some(Ok(Message::Binary(_))) => {
                    connection
                        .send(&error_response(Value::Null, "bad_request", "expected a text message"))
                        .await;
                }
                Some(Ok(Message::Pong(_))) => {
                    awaiting_pong = false;
                    if last_touch.elapsed() >= LAST_SEEN_THROTTLE
                        && let Some(device) = connection.device()
                    {
                        last_touch = Instant::now();
                        let _ = hub.store.touch_device(&device.device_id);
                    }
                }
                Some(Ok(Message::Close(_))) | None => break Ok(()),
                Some(Ok(_)) => {}
                Some(Err(error)) => break Err(error.into()),
            },
            _ = ping.tick() => {
                if awaiting_pong {
                    break Ok(());
                }
                awaiting_pong = true;
                if let Err(error) = ws_tx.send(Message::Ping(Vec::new().into())).await {
                    break Err(error.into());
                }
            }
        }
    };
    let revoked = cancel.is_cancelled();
    cancel.cancel();
    if !revoked && let Some(device) = connection.device() {
        hub.unregister_connection(&device.device_id);
    }
    result
}

fn ok_response(id: Value, result: Value) -> Value {
    json!({"id": id, "result": result})
}

fn error_response(id: Value, code: &str, message: &str) -> Value {
    json!({"id": id, "error": {"code": code, "message": message}})
}

/// Answers one request frame with exactly one response.
async fn handle_request(connection: &Arc<Connection>, text: &str) {
    let Ok(request) = serde_json::from_str::<Value>(text) else {
        connection
            .send(&error_response(
                Value::Null,
                "bad_request",
                "malformed JSON",
            ))
            .await;
        return;
    };
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    let (Some(_), Some(method)) = (id.as_u64(), request.get("method").and_then(Value::as_str))
    else {
        connection
            .send(&error_response(
                id,
                "bad_request",
                "a request needs a numeric id and a method",
            ))
            .await;
        return;
    };
    let params = request.get("params").cloned().unwrap_or(Value::Null);
    if !(params.is_null() || params.is_object()) {
        connection
            .send(&error_response(
                id,
                "bad_request",
                "params must be an object",
            ))
            .await;
        return;
    }
    let response = match method {
        "pair.begin" => pair_begin(connection, id),
        "pair.complete" => pair_complete(connection, id, &params).await,
        _ => {
            let Some(device) = connection.device() else {
                connection
                    .send(&error_response(
                        id,
                        "unauthorized",
                        "this device is not paired",
                    ))
                    .await;
                return;
            };
            let required = match method {
                "hub.info" => None,
                "listen" => Some(SCOPE_READ),
                "forget" => Some(SCOPE_MANAGE),
                _ => {
                    connection
                        .send(&error_response(
                            id,
                            "unknown_method",
                            &format!("unknown method '{method}'"),
                        ))
                        .await;
                    return;
                }
            };
            if let Some(scope) = required
                && !device.scopes.iter().any(|granted| granted == scope)
            {
                connection
                    .send(&error_response(
                        id,
                        "forbidden",
                        &format!("'{method}' needs the '{scope}' scope"),
                    ))
                    .await;
                return;
            }
            match method {
                "hub.info" => hub_info(connection, id, &device),
                "listen" => {
                    listen(connection, id).await;
                    return;
                }
                _ => forget(connection, id, &params),
            }
        }
    };
    connection.send(&response).await;
}

fn hub_info(connection: &Connection, id: Value, device: &Device) -> Value {
    let Some(remote) = &connection.hub.remote else {
        return error_response(id, "internal", "remote access is not configured");
    };
    ok_response(
        id,
        json!({
            "hub_id": remote.hub_id,
            "hub_name": remote.hub_name,
            "protocol": PROTOCOL_VERSION,
            "scopes": device.scopes,
        }),
    )
}

/// Acknowledges, then streams on the same connection until it closes.
async fn listen(connection: &Arc<Connection>, id: Value) {
    if connection.streaming.swap(true, Ordering::SeqCst) {
        connection
            .send(&error_response(id, "bad_request", "already listening"))
            .await;
        return;
    }
    let receiver = connection.hub.updates.subscribe();
    connection.send(&ok_response(id, json!({}))).await;
    let sink = WsSink(connection.out.clone());
    let _ = stream_merged(
        sink,
        Arc::clone(&connection.hub.store),
        receiver,
        connection.cancel.clone(),
    )
    .await;
}

fn forget(connection: &Connection, id: Value, params: &Value) -> Value {
    let (Some(source_id), Some(invocation_id)) = (
        params.get("source_id").and_then(Value::as_str),
        params.get("invocation_id").and_then(Value::as_str),
    ) else {
        return error_response(
            id,
            "bad_request",
            "forget needs source_id and invocation_id",
        );
    };
    match connection.hub.forget(source_id, invocation_id) {
        Ok(ForgetOutcome::Forgotten { hub_revision }) => {
            ok_response(id, json!({"hub_revision": hub_revision}))
        }
        Ok(ForgetOutcome::NotFound) => error_response(
            id,
            "not_found",
            &format!("no agent {source_id}/{invocation_id}"),
        ),
        Ok(ForgetOutcome::NotStopped) => {
            error_response(id, "not_stopped", "only stopped agents can be forgotten")
        }
        Err(error) => error_response(id, "internal", &error.to_string()),
    }
}

fn pair_begin(connection: &Connection, id: Value) -> Value {
    if connection.device_spki.is_none() {
        return error_response(id, "pairing_failed", "pairing needs a client certificate");
    }
    let nonce: [u8; 32] = rand::random();
    *connection.nonce.lock().expect("nonce mutex poisoned") = Some(nonce);
    ok_response(id, json!({"nonce": URL_SAFE_NO_PAD.encode(nonce)}))
}

async fn pair_complete(connection: &Arc<Connection>, id: Value, params: &Value) -> Value {
    let Some(device_spki) = &connection.device_spki else {
        return error_response(id, "pairing_failed", "pairing needs a client certificate");
    };
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty() && name.chars().count() <= 64);
    let mac = params
        .get("mac")
        .and_then(Value::as_str)
        .and_then(|mac| URL_SAFE_NO_PAD.decode(mac).ok());
    let (Some(name), Some(mac)) = (name, mac) else {
        return error_response(
            id,
            "bad_request",
            "pair.complete needs a name (1-64 characters) and a base64url mac",
        );
    };
    let Some(nonce) = connection
        .nonce
        .lock()
        .expect("nonce mutex poisoned")
        .take()
    else {
        return error_response(id, "bad_request", "call pair.begin first");
    };
    let events = match connection.hub.attempt_pairing(device_spki, &nonce, &mac) {
        PairAttempt::Claimed { events, .. } => events,
        PairAttempt::Closed => {
            return error_response(id, "pairing_closed", "no pairing window is open");
        }
        PairAttempt::Failed => {
            return error_response(id, "pairing_failed", "pairing proof did not verify");
        }
    };
    let (reply, decision) = oneshot::channel();
    let confirm = PairEvent::Confirm {
        name: name.to_owned(),
        fingerprint: fingerprint(device_spki),
        spki_sha256: sha256_hex(device_spki),
        reply,
    };
    if events.send(confirm).await.is_err() {
        return error_response(id, "pairing_closed", "the pairing command went away");
    }
    match decision.await {
        Ok(Some(device)) => {
            let hub_name = connection
                .hub
                .remote
                .as_ref()
                .map(|remote| remote.hub_name.clone())
                .unwrap_or_default();
            connection
                .hub
                .register_connection(&device.device_id, connection.cancel.clone());
            let result = json!({"device_id": device.device_id, "hub_name": hub_name});
            *connection.device.lock().expect("device mutex poisoned") = Some(device);
            ok_response(id, result)
        }
        _ => error_response(id, "pairing_rejected", "the operator rejected pairing"),
    }
}

/// Wraps each envelope as a pushed stream event. `data` is exactly the line
/// `sessiontap-hub listen` prints.
struct WsSink(mpsc::Sender<Message>);

impl StreamSink for WsSink {
    async fn send(&mut self, envelope: &HubStreamEnvelope) -> Result<()> {
        let data = serde_json::to_string(envelope)?;
        self.0
            .send(Message::text(format!(
                r#"{{"event":"stream","data":{data}}}"#
            )))
            .await
            .context("remote connection closed")?;
        Ok(())
    }
}
