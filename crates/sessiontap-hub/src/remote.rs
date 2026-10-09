//! Remote device listener: TLS 1.3 + WebSocket, JSON requests and pushed
//! stream events. Authorization comes from the client certificate SPKI.

use anyhow::{Context, Result};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::{OwnedSemaphorePermit, Semaphore, broadcast, mpsc, oneshot},
    time::timeout,
};
use tokio_rustls::TlsAcceptor;
use tokio_tungstenite::tungstenite::{
    Message,
    protocol::{CloseFrame, WebSocketConfig, frame::coding::CloseCode},
};
use tokio_util::sync::CancellationToken;

use crate::discovery::Discovery;
use crate::ingest::HubPublication;
use crate::listen::{HubStreamEnvelope, StreamSink, stream_merged};
use crate::relay::{Answer, DeviceOut, Pending, RelayError};
use crate::scope::Scope;
use crate::service::{ConnectionHandle, Hub, PairAttempt, PairEvent, fingerprint};
use crate::store::{Device, ForgetOutcome};
use crate::tls::sha256_hex;

pub const PROTOCOL_VERSION: u32 = 1;
/// WebSocket close code sent when a device is revoked.
pub const CLOSE_REVOKED: u16 = 4401;
/// WebSocket close code sent when an open stream's scope is withdrawn.
pub const CLOSE_SCOPE_WITHDRAWN: u16 = 4403;
pub const PING_INTERVAL: Duration = Duration::from_secs(60);
/// Limit for the TLS handshake, and separately for the WebSocket upgrade.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// Time from accept within which a connection must authenticate.
pub const UNAUTHENTICATED_DEADLINE: Duration = Duration::from_secs(30);
pub const MAX_CONNECTIONS: usize = 64;
pub const MAX_UNAUTHENTICATED: usize = 16;
pub const MAX_UNAUTHENTICATED_PER_ADDRESS: u32 = 4;
pub const MAX_MESSAGE_SIZE: usize = 64 * 1024;
pub const MAX_FRAME_SIZE: usize = 64 * 1024;
pub const MAX_INFLIGHT_REQUESTS: usize = 8;
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(30);
const LAST_SEEN_THROTTLE: Duration = Duration::from_secs(60);
const OUTBOUND_CAPACITY: usize = 256;
/// How often an expired deadline is rechecked while the operator decides.
const OPERATOR_WAIT_RECHECK: Duration = Duration::from_secs(1);

/// Remote connection limits. `Default` holds the production values; tests
/// shorten them.
#[derive(Debug, Clone)]
pub struct RemoteLimits {
    pub ping_interval: Duration,
    pub handshake_timeout: Duration,
    pub unauthenticated_deadline: Duration,
    pub max_connections: usize,
    pub max_unauthenticated: usize,
    pub max_unauthenticated_per_address: u32,
    pub max_inflight_requests: usize,
    pub write_timeout: Duration,
}

impl Default for RemoteLimits {
    fn default() -> Self {
        Self {
            ping_interval: PING_INTERVAL,
            handshake_timeout: HANDSHAKE_TIMEOUT,
            unauthenticated_deadline: UNAUTHENTICATED_DEADLINE,
            max_connections: MAX_CONNECTIONS,
            max_unauthenticated: MAX_UNAUTHENTICATED,
            max_unauthenticated_per_address: MAX_UNAUTHENTICATED_PER_ADDRESS,
            max_inflight_requests: MAX_INFLIGHT_REQUESTS,
            write_timeout: WRITE_TIMEOUT,
        }
    }
}

/// Peer grouping for per-address limits: an IPv4 address, or an IPv6 /64.
/// IPv4-mapped IPv6 addresses count as IPv4.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AddressKey {
    V4(Ipv4Addr),
    V6(u64),
}

impl From<IpAddr> for AddressKey {
    fn from(address: IpAddr) -> Self {
        match address {
            IpAddr::V4(v4) => Self::V4(v4),
            IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
                Some(v4) => Self::V4(v4),
                None => Self::V6((u128::from(v6) >> 64) as u64),
            },
        }
    }
}

/// Connection admission shared by every remote listener.
pub struct RemoteGate {
    connections: Arc<Semaphore>,
    unauthenticated: Arc<Semaphore>,
    per_address: Arc<Mutex<HashMap<AddressKey, u32>>>,
    max_per_address: u32,
}

/// Holds a connection's place in the global cap for its whole life.
pub struct Admission {
    _connection: OwnedSemaphorePermit,
    ticket: UnauthTicket,
}

/// Holds an unauthenticated slot, globally and for the peer address.
/// Dropping it frees both.
pub struct UnauthTicket {
    _permit: OwnedSemaphorePermit,
    key: AddressKey,
    per_address: Arc<Mutex<HashMap<AddressKey, u32>>>,
}

impl Drop for UnauthTicket {
    fn drop(&mut self) {
        let mut counts = self.per_address.lock().expect("gate mutex poisoned");
        if let Some(count) = counts.get_mut(&self.key) {
            *count -= 1;
            if *count == 0 {
                counts.remove(&self.key);
            }
        }
    }
}

impl RemoteGate {
    #[must_use]
    pub fn new(limits: &RemoteLimits) -> Arc<Self> {
        Arc::new(Self {
            connections: Arc::new(Semaphore::new(limits.max_connections)),
            unauthenticated: Arc::new(Semaphore::new(limits.max_unauthenticated)),
            per_address: Arc::new(Mutex::new(HashMap::new())),
            max_per_address: limits.max_unauthenticated_per_address,
        })
    }

    /// Admits a new connection, or `None` when any cap is reached.
    fn admit(&self, peer: IpAddr) -> Option<Admission> {
        let connection = Arc::clone(&self.connections).try_acquire_owned().ok()?;
        let permit = Arc::clone(&self.unauthenticated).try_acquire_owned().ok()?;
        let key = AddressKey::from(peer);
        {
            let mut counts = self.per_address.lock().expect("gate mutex poisoned");
            let count = counts.entry(key).or_default();
            if *count >= self.max_per_address {
                return None;
            }
            *count += 1;
        }
        Some(Admission {
            _connection: connection,
            ticket: UnauthTicket {
                _permit: permit,
                key,
                per_address: Arc::clone(&self.per_address),
            },
        })
    }
}

/// Retry delays for binding a remote address: `initial`, doubling up to
/// `max`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Backoff {
    pub initial: Duration,
    pub max: Duration,
}

/// Production bind retry: 1 s doubling to 30 s.
pub const BIND_BACKOFF: Backoff = Backoff {
    initial: Duration::from_secs(1),
    max: Duration::from_secs(30),
};

impl Backoff {
    /// The delay after `previous`, or `initial` when a streak starts.
    #[must_use]
    pub fn next(&self, previous: Option<Duration>) -> Duration {
        previous.map_or(self.initial, |previous| (previous * 2).min(self.max))
    }
}

/// Keeps one remote address served forever. Binding is retried with
/// `backoff`; a failure streak and a successful bind are each logged once.
/// When the accept loop fails, the address goes back to the retry loop.
/// `discovery` is announced only while the listener is bound.
pub async fn supervise_listener(
    address: SocketAddr,
    acceptor: TlsAcceptor,
    hub: Arc<Hub>,
    gate: Arc<RemoteGate>,
    limits: RemoteLimits,
    backoff: Backoff,
    discovery: Option<Discovery>,
) {
    let mut delay = None;
    loop {
        let listener = match TcpListener::bind(address).await {
            Ok(listener) => listener,
            Err(error) => {
                if delay.is_none() {
                    eprintln!(
                        "sessiontap-hub: cannot bind remote address {address}: {error}; retrying"
                    );
                }
                let wait = backoff.next(delay);
                delay = Some(wait);
                tokio::time::sleep(wait).await;
                continue;
            }
        };
        let local = listener.local_addr().unwrap_or(address);
        eprintln!("sessiontap-hub: remote access on {local}");
        let announcement = discovery
            .as_ref()
            .map(|discovery| discovery.announce(local));
        let error = serve_remote(
            listener,
            acceptor.clone(),
            Arc::clone(&hub),
            Arc::clone(&gate),
            limits.clone(),
        )
        .await;
        drop(announcement);
        eprintln!("sessiontap-hub: remote listener {address} failed: {error}; rebinding");
        // the failure above starts the streak, so its retries stay quiet
        let wait = backoff.next(None);
        delay = Some(wait);
        tokio::time::sleep(wait).await;
    }
}

/// Accepts remote connections on one listener until `accept` fails, and
/// returns that error.
pub async fn serve_remote(
    listener: TcpListener,
    acceptor: TlsAcceptor,
    hub: Arc<Hub>,
    gate: Arc<RemoteGate>,
    limits: RemoteLimits,
) -> std::io::Error {
    loop {
        let (tcp, peer) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(error) => return error,
        };
        // over a cap: drop the socket before TLS, without logging
        let Some(admission) = gate.admit(peer.ip()) else {
            drop(tcp);
            continue;
        };
        let acceptor = acceptor.clone();
        let hub = Arc::clone(&hub);
        let limits = limits.clone();
        tokio::spawn(async move {
            if let Err(error) =
                serve_connection(tcp, peer.ip(), admission, acceptor, hub, limits).await
            {
                eprintln!("sessiontap-hub: remote connection ended: {error:#}");
            }
        });
    }
}

struct Connection {
    hub: Arc<Hub>,
    peer: IpAddr,
    device_spki: Option<Vec<u8>>,
    device: Mutex<Option<Device>>,
    nonce: Mutex<Option<[u8; 32]>>,
    streaming: AtomicBool,
    authenticated: AtomicBool,
    awaiting_operator: AtomicBool,
    ticket: Mutex<Option<UnauthTicket>>,
    requests: Arc<Semaphore>,
    max_inflight_requests: usize,
    out: mpsc::Sender<Message>,
    /// Revocation: close with `CLOSE_REVOKED`.
    cancel: CancellationToken,
    /// Registry entry; its `withdraw` token is a child of `cancel`.
    handle: ConnectionHandle,
    /// Policy violation: close with 1008.
    kill: CancellationToken,
}

impl Connection {
    /// Queues a frame. Authenticated connections wait for room; an
    /// unauthenticated connection with a full queue is closed instead.
    async fn send(&self, value: &Value) {
        let message = Message::text(value.to_string());
        if self.is_authenticated() {
            let _ = self.out.send(message).await;
        } else {
            self.try_send(message);
        }
    }

    fn try_send(&self, message: Message) {
        if let Err(mpsc::error::TrySendError::Full(_)) = self.out.try_send(message)
            && !self.is_authenticated()
        {
            self.kill.cancel();
        }
    }

    fn device(&self) -> Option<Device> {
        self.device.lock().expect("device mutex poisoned").clone()
    }

    fn is_authenticated(&self) -> bool {
        self.authenticated.load(Ordering::SeqCst)
    }

    /// Records the device. The first time, the connection registers for
    /// revocation, leaves the unauthenticated pool, and gets its full
    /// request budget. Callers hold `device_gate` for reading.
    fn authenticate(&self, device: &Device) {
        *self.device.lock().expect("device mutex poisoned") = Some(device.clone());
        if !self.authenticated.swap(true, Ordering::SeqCst) {
            self.hub
                .register_connection(&device.device_id, self.handle.clone());
            self.ticket.lock().expect("ticket mutex poisoned").take();
            self.requests
                .add_permits(self.max_inflight_requests.saturating_sub(1));
        }
    }

    /// Re-reads this connection's device from the store. Callers hold
    /// `device_gate` for reading.
    fn current_device(&self) -> Result<Option<Device>> {
        let Some(spki) = &self.device_spki else {
            return Ok(None);
        };
        let device = self.hub.store.device_by_spki(&sha256_hex(spki))?;
        match &device {
            Some(device) => self.authenticate(device),
            None => *self.device.lock().expect("device mutex poisoned") = None,
        }
        Ok(device)
    }
}

fn websocket_config() -> WebSocketConfig {
    WebSocketConfig::default()
        .max_message_size(Some(MAX_MESSAGE_SIZE))
        .max_frame_size(Some(MAX_FRAME_SIZE))
}

fn close_frame(code: CloseCode, reason: &str) -> Message {
    Message::Close(Some(CloseFrame {
        code,
        reason: reason.to_owned().into(),
    }))
}

/// Serves one admitted remote connection from TLS handshake to close.
async fn serve_connection(
    tcp: TcpStream,
    peer: IpAddr,
    admission: Admission,
    acceptor: TlsAcceptor,
    hub: Arc<Hub>,
    limits: RemoteLimits,
) -> Result<()> {
    let Admission {
        _connection: _connection_permit,
        ticket,
    } = admission;
    let accepted_at = tokio::time::Instant::now();
    let tls = timeout(limits.handshake_timeout, acceptor.accept(tcp))
        .await
        .context("tls handshake timed out")?
        .context("tls handshake")?;
    let device_spki = tls
        .get_ref()
        .1
        .peer_certificates()
        .and_then(|certs| certs.first())
        .and_then(|cert| crate::tls::spki_of(cert).ok());
    let (out, mut outbound) = mpsc::channel::<Message>(OUTBOUND_CAPACITY);
    let handle = ConnectionHandle::new();
    let cancel = handle.revoke.clone();
    let withdraw = handle.withdraw.clone();
    let kill = CancellationToken::new();
    let connection = Arc::new(Connection {
        hub: Arc::clone(&hub),
        peer,
        device_spki,
        device: Mutex::new(None),
        nonce: Mutex::new(None),
        streaming: AtomicBool::new(false),
        authenticated: AtomicBool::new(false),
        awaiting_operator: AtomicBool::new(false),
        ticket: Mutex::new(Some(ticket)),
        requests: Arc::new(Semaphore::new(1)),
        max_inflight_requests: limits.max_inflight_requests,
        out,
        cancel: cancel.clone(),
        handle,
        kill: kill.clone(),
    });
    let known = {
        let _gate = hub.device_gate.read().expect("device gate poisoned");
        connection.current_device()?
    };
    if let Some(device) = &known {
        let _ = hub.store.touch_device(&device.device_id);
    }
    let ws = timeout(
        limits.handshake_timeout,
        tokio_tungstenite::accept_async_with_config(tls, Some(websocket_config())),
    )
    .await
    .context("websocket upgrade timed out")?
    .context("websocket upgrade")?;
    let (mut ws_tx, mut ws_rx) = ws.split();
    let mut ping = tokio::time::interval_at(
        tokio::time::Instant::now() + limits.ping_interval,
        limits.ping_interval,
    );
    let mut deadline = accepted_at + limits.unauthenticated_deadline;
    let mut deadline_armed = true;
    let mut awaiting_pong = false;
    let mut last_touch = Instant::now();
    // an unauthenticated connection's writes may not outlive its deadline
    let write_limit = |armed: bool, deadline: tokio::time::Instant| {
        let limit = tokio::time::Instant::now() + limits.write_timeout;
        if armed { limit.min(deadline) } else { limit }
    };
    let result = loop {
        if deadline_armed && connection.is_authenticated() {
            deadline_armed = false;
        }
        tokio::select! {
            biased;
            () = cancel.cancelled() => {
                connection.device.lock().expect("device mutex poisoned").take();
                let close = close_frame(CloseCode::from(CLOSE_REVOKED), "device revoked");
                let _ = timeout(limits.write_timeout, ws_tx.send(close)).await;
                break Ok(());
            }
            () = withdraw.cancelled() => {
                let close = close_frame(CloseCode::from(CLOSE_SCOPE_WITHDRAWN), "scope withdrawn");
                let _ = timeout(limits.write_timeout, ws_tx.send(close)).await;
                break Ok(());
            }
            () = kill.cancelled() => {
                let close = close_frame(CloseCode::Policy, "policy violation");
                let _ = tokio::time::timeout_at(write_limit(deadline_armed, deadline), ws_tx.send(close)).await;
                break Ok(());
            }
            () = tokio::time::sleep_until(deadline), if deadline_armed => {
                if connection.is_authenticated() {
                    deadline_armed = false;
                } else if connection.awaiting_operator.load(Ordering::SeqCst) {
                    deadline = tokio::time::Instant::now() + OPERATOR_WAIT_RECHECK;
                } else {
                    let close = close_frame(CloseCode::Policy, "not authenticated in time");
                    let _ = timeout(Duration::from_secs(1), ws_tx.send(close)).await;
                    break Ok(());
                }
            }
            Some(message) = outbound.recv() => {
                match tokio::time::timeout_at(write_limit(deadline_armed, deadline), ws_tx.send(message)).await {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => break Err(error.into()),
                    Err(_) => break Err(anyhow::anyhow!("write timed out")),
                }
            }
            incoming = ws_rx.next() => match incoming {
                Some(Ok(Message::Text(text))) => {
                    match Arc::clone(&connection.requests).try_acquire_owned() {
                        Ok(permit) => {
                            let connection = Arc::clone(&connection);
                            tokio::spawn(async move {
                                handle_request(&connection, text.as_str(), permit).await;
                            });
                        }
                        Err(_) if !connection.is_authenticated() => {
                            // unauthenticated clients may not pipeline
                            connection.kill.cancel();
                        }
                        Err(_) => {
                            let id = serde_json::from_str::<Value>(text.as_str())
                                .ok()
                                .and_then(|request| request.get("id").cloned())
                                .unwrap_or(Value::Null);
                            connection.try_send(Message::text(
                                error_response(id, "busy", "too many requests in flight").to_string(),
                            ));
                        }
                    }
                }
                Some(Ok(Message::Binary(_))) => {
                    connection.try_send(Message::text(
                        error_response(Value::Null, "bad_request", "expected a text message").to_string(),
                    ));
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
                let ping = Message::Ping(Vec::new().into());
                match tokio::time::timeout_at(write_limit(deadline_armed, deadline), ws_tx.send(ping)).await {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => break Err(error.into()),
                    Err(_) => break Err(anyhow::anyhow!("write timed out")),
                }
            }
        }
    };
    let revoked = cancel.is_cancelled();
    cancel.cancel();
    hub.relay.release_owner(connection.handle.id);
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

/// What a device request resolved to while `device_gate` was held.
enum Dispatch {
    Respond(Value),
    Listen(Value, broadcast::Receiver<HubPublication>),
    /// A terminal request forwarded to its source, awaiting the answer.
    Terminal(Value, TerminalWait),
    /// The connection is closing: answer nothing.
    Silent,
}

enum TerminalWait {
    Open(Pending),
    Input(Pending),
}

/// Answers one request frame with exactly one response, unless the
/// connection is being revoked. The permit is released before the answer
/// is queued, so a client that waits for each answer never trips the
/// unauthenticated pipelining check.
async fn handle_request(connection: &Arc<Connection>, text: &str, permit: OwnedSemaphorePermit) {
    if connection.cancel.is_cancelled() {
        return;
    }
    let Ok(request) = serde_json::from_str::<Value>(text) else {
        drop(permit);
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
        drop(permit);
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
        drop(permit);
        connection
            .send(&error_response(
                id,
                "bad_request",
                "params must be an object",
            ))
            .await;
        return;
    }
    let dispatch = match method {
        "pair.begin" => Dispatch::Respond(pair_begin(connection, id)),
        "pair.complete" => Dispatch::Respond(pair_complete(connection, id, &params).await),
        _ => device_request(connection, id, method, &params),
    };
    if let Dispatch::Terminal(id, wait) = dispatch {
        let (response, activate) = terminal_answer(connection, id, wait).await;
        drop(permit);
        connection.send(&response).await;
        if let Some(stream) = activate {
            connection.hub.relay.activate(stream);
        }
        return;
    }
    drop(permit);
    match dispatch {
        Dispatch::Respond(response) => connection.send(&response).await,
        Dispatch::Listen(id, receiver) => listen(connection, id, receiver).await,
        Dispatch::Terminal(..) | Dispatch::Silent => {}
    }
}

/// Authorizes a non-pairing request against the stored device and runs it.
/// Holding `device_gate` for reading means a concurrent revoke either
/// waits for this request or is seen by it.
fn device_request(connection: &Connection, id: Value, method: &str, params: &Value) -> Dispatch {
    let _gate = connection
        .hub
        .device_gate
        .read()
        .expect("device gate poisoned");
    if connection.handle.withdraw.is_cancelled() {
        return Dispatch::Silent;
    }
    let device = match connection.current_device() {
        Ok(Some(device)) => device,
        Ok(None) => {
            return Dispatch::Respond(error_response(
                id,
                "unauthorized",
                "this device is not paired",
            ));
        }
        Err(error) => return Dispatch::Respond(error_response(id, "internal", &error.to_string())),
    };
    let required = match method {
        "hub.info" => None,
        "listen" => Some(Scope::Read),
        "forget" => Some(Scope::Manage),
        "terminal.open" | "terminal.close" => Some(Scope::Watch),
        "terminal.input" => Some(Scope::Control),
        _ => {
            return Dispatch::Respond(error_response(
                id,
                "unknown_method",
                &format!("unknown method '{method}'"),
            ));
        }
    };
    let effective = connection.hub.effective_scopes(&device);
    if let Some(scope) = required
        && !effective.contains(&scope)
    {
        return Dispatch::Respond(error_response(
            id,
            "forbidden",
            &format!("'{method}' needs the '{scope}' scope"),
        ));
    }
    match method {
        "hub.info" => Dispatch::Respond(hub_info(connection, id, &effective)),
        "listen" => {
            if connection.streaming.swap(true, Ordering::SeqCst) {
                return Dispatch::Respond(error_response(id, "bad_request", "already listening"));
            }
            connection.handle.add_stream_scope(Scope::Read);
            Dispatch::Listen(id, connection.hub.updates.subscribe())
        }
        "terminal.open" => terminal_open(connection, id, params),
        "terminal.input" => terminal_input(connection, id, params),
        "terminal.close" => terminal_close(connection, id, params),
        _ => Dispatch::Respond(forget(connection, id, params)),
    }
}

fn relay_error(id: Value, error: &RelayError) -> Value {
    error_response(id, error.code, &error.message)
}

/// Forwards `terminal.open` to the agent's source. Callers hold
/// `device_gate` for reading.
fn terminal_open(connection: &Connection, id: Value, params: &Value) -> Dispatch {
    let (Some(source_id), Some(invocation_id)) = (
        params.get("source_id").and_then(Value::as_str),
        params.get("invocation_id").and_then(Value::as_str),
    ) else {
        return Dispatch::Respond(error_response(
            id,
            "bad_request",
            "terminal.open needs source_id and invocation_id",
        ));
    };
    match connection.hub.store.has_agent(source_id, invocation_id) {
        Ok(true) => {}
        Ok(false) => {
            return Dispatch::Respond(error_response(
                id,
                "not_found",
                &format!("no agent {source_id}/{invocation_id}"),
            ));
        }
        Err(error) => return Dispatch::Respond(error_response(id, "internal", &error.to_string())),
    }
    let device = DeviceOut {
        owner: connection.handle.id,
        out: connection.out.clone(),
    };
    match connection.hub.relay.open(&device, source_id, invocation_id) {
        Ok(pending) => {
            connection.handle.add_stream_scope(Scope::Watch);
            Dispatch::Terminal(id, TerminalWait::Open(pending))
        }
        Err(error) => Dispatch::Respond(relay_error(id, &error)),
    }
}

fn stream_param(params: &Value) -> Option<u64> {
    params.get("stream").and_then(Value::as_u64)
}

/// Forwards `terminal.input` for a stream this connection opened. Callers
/// hold `device_gate` for reading, so a revoke either sees the input
/// forwarded or forwards nothing.
fn terminal_input(connection: &Connection, id: Value, params: &Value) -> Dispatch {
    let Some(stream) = stream_param(params) else {
        return Dispatch::Respond(error_response(
            id,
            "bad_request",
            "terminal.input needs a stream",
        ));
    };
    let input = match (params.get("keys"), params.get("paste")) {
        (Some(keys), None) => json!({"keys": keys}),
        (None, Some(paste)) => json!({"paste": paste}),
        _ => {
            return Dispatch::Respond(error_response(
                id,
                "bad_request",
                "terminal.input needs exactly one of keys or paste",
            ));
        }
    };
    let input = match serde_json::from_value(input) {
        Ok(input) => input,
        Err(error) => {
            return Dispatch::Respond(error_response(id, "bad_request", &error.to_string()));
        }
    };
    match connection
        .hub
        .relay
        .input(connection.handle.id, stream, input)
    {
        Ok(pending) => Dispatch::Terminal(id, TerminalWait::Input(pending)),
        Err(error) => Dispatch::Respond(relay_error(id, &error)),
    }
}

fn terminal_close(connection: &Connection, id: Value, params: &Value) -> Dispatch {
    let Some(stream) = stream_param(params) else {
        return Dispatch::Respond(error_response(
            id,
            "bad_request",
            "terminal.close needs a stream",
        ));
    };
    Dispatch::Respond(
        match connection.hub.relay.close(connection.handle.id, stream) {
            Ok(()) => ok_response(id, json!({})),
            Err(error) => relay_error(id, &error),
        },
    )
}

/// Waits for the source's answer. Returns the response and, for an opened
/// stream, its ID to activate once the response is queued.
async fn terminal_answer(
    connection: &Connection,
    id: Value,
    wait: TerminalWait,
) -> (Value, Option<u64>) {
    let relay = &connection.hub.relay;
    match wait {
        TerminalWait::Open(pending) => {
            let stream = pending.stream;
            match relay.answer(pending).await {
                Ok(Answer::Opened) => (ok_response(id, json!({"stream": stream})), Some(stream)),
                Ok(Answer::Error { code, message }) => {
                    relay.abandon(stream);
                    (error_response(id, &code, &message), None)
                }
                Ok(Answer::Input(_)) => {
                    relay.abandon(stream);
                    (
                        error_response(id, "internal", "unexpected source answer"),
                        None,
                    )
                }
                Err(error) => {
                    relay.abandon(stream);
                    (relay_error(id, &error), None)
                }
            }
        }
        TerminalWait::Input(pending) => match relay.answer(pending).await {
            Ok(Answer::Input(None)) => (ok_response(id, json!({})), None),
            Ok(Answer::Input(Some(code)) | Answer::Error { code, .. }) => (
                error_response(id, &code, "input refused by the source"),
                None,
            ),
            Ok(Answer::Opened) => (
                error_response(id, "internal", "unexpected source answer"),
                None,
            ),
            Err(error) => (relay_error(id, &error), None),
        },
    }
}

fn hub_info(connection: &Connection, id: Value, scopes: &[Scope]) -> Value {
    let Some(remote) = &connection.hub.remote else {
        return error_response(id, "internal", "remote access is not configured");
    };
    ok_response(
        id,
        json!({
            "hub_id": remote.hub_id,
            "hub_name": remote.hub_name,
            "protocol": PROTOCOL_VERSION,
            "scopes": Scope::names(scopes),
            "endpoints": remote.endpoints(),
        }),
    )
}

/// Acknowledges, then streams on the same connection until it closes.
async fn listen(
    connection: &Arc<Connection>,
    id: Value,
    receiver: broadcast::Receiver<HubPublication>,
) {
    connection.send(&ok_response(id, json!({}))).await;
    let sink = WsSink(connection.out.clone());
    let _ = stream_merged(
        sink,
        Arc::clone(&connection.hub.store),
        receiver,
        connection.handle.withdraw.clone(),
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

fn rate_limited(id: Value) -> Value {
    error_response(id, "rate_limited", "too many pairing calls; slow down")
}

fn pair_begin(connection: &Connection, id: Value) -> Value {
    if !connection.hub.allow_pair_call(connection.peer) {
        return rate_limited(id);
    }
    if connection.device_spki.is_none() {
        return error_response(id, "pairing_failed", "pairing needs a client certificate");
    }
    let nonce: [u8; 32] = rand::random();
    *connection.nonce.lock().expect("nonce mutex poisoned") = Some(nonce);
    ok_response(id, json!({"nonce": URL_SAFE_NO_PAD.encode(nonce)}))
}

async fn pair_complete(connection: &Arc<Connection>, id: Value, params: &Value) -> Value {
    if !connection.hub.allow_pair_call(connection.peer) {
        return rate_limited(id);
    }
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
    let events = match connection
        .hub
        .attempt_pairing(device_spki, connection.peer, &nonce, &mac)
    {
        PairAttempt::Claimed { events, .. } => events,
        PairAttempt::Closed => {
            return error_response(id, "pairing_closed", "no pairing window is open");
        }
        PairAttempt::Failed => {
            return error_response(id, "pairing_failed", "pairing proof did not verify");
        }
        PairAttempt::Locked => {
            return error_response(id, "pairing_failed", "too many failed pairing attempts");
        }
    };
    let (reply, decision) = oneshot::channel();
    let confirm = PairEvent::Confirm {
        name: name.to_owned(),
        fingerprint: fingerprint(device_spki),
        spki_sha256: sha256_hex(device_spki),
        reply,
    };
    connection.awaiting_operator.store(true, Ordering::SeqCst);
    let outcome = if events.send(confirm).await.is_err() {
        None
    } else {
        Some(decision.await)
    };
    let response = match outcome {
        None => error_response(id, "pairing_closed", "the pairing command went away"),
        Some(Ok(Some(device))) => {
            let hub_name = connection
                .hub
                .remote
                .as_ref()
                .map(|remote| remote.hub_name.clone())
                .unwrap_or_default();
            {
                let _gate = connection
                    .hub
                    .device_gate
                    .read()
                    .expect("device gate poisoned");
                connection.authenticate(&device);
            }
            ok_response(
                id,
                json!({"device_id": device.device_id, "hub_name": hub_name}),
            )
        }
        Some(_) => error_response(id, "pairing_rejected", "the operator rejected pairing"),
    };
    connection.awaiting_operator.store(false, Ordering::SeqCst);
    response
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv6Addr;

    #[test]
    fn address_keys_group_peers() {
        let v4: IpAddr = "192.168.1.7".parse().unwrap();
        assert_eq!(
            AddressKey::from(v4),
            AddressKey::V4(Ipv4Addr::new(192, 168, 1, 7))
        );
        let mapped: IpAddr = "::ffff:192.168.1.7".parse().unwrap();
        assert_eq!(AddressKey::from(mapped), AddressKey::from(v4));
        let a: IpAddr = "2001:db8:1:2::1".parse().unwrap();
        let b: IpAddr = "2001:db8:1:2:ffff:ffff:ffff:ffff".parse().unwrap();
        let other: IpAddr = "2001:db8:1:3::1".parse().unwrap();
        assert_eq!(AddressKey::from(a), AddressKey::from(b));
        assert_ne!(AddressKey::from(a), AddressKey::from(other));
        assert_eq!(
            AddressKey::from(IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 1, 2, 0, 0, 0, 9))),
            AddressKey::V6(0x2001_0db8_0001_0002)
        );
    }

    #[test]
    fn bind_backoff_doubles_to_the_cap() {
        let mut delay = None;
        let waits: Vec<u64> = (0..7)
            .map(|_| {
                let wait = BIND_BACKOFF.next(delay);
                delay = Some(wait);
                wait.as_secs()
            })
            .collect();
        assert_eq!(waits, vec![1, 2, 4, 8, 16, 30, 30]);
    }
}
