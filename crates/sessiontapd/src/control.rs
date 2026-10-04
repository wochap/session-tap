//! Outbound hub control channel: relays the hub's terminal requests onto
//! the daemon's live terminal sessions. Opened only for an enabled hub sink
//! with `control: true`, re-checked at every open and input.

use crate::{app::App, server::error_to_response, sinks::TokenSource, terminal::Watcher};
use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use sessiontap_core::{
    domain::InvocationId,
    protocol::{RELAY_PROTOCOL_VERSION, RelayMessage, RelayRequest, Response},
    terminal::{EndReason, TerminalFrame, TerminalInput, error_code},
};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::{sync::mpsc, task::JoinHandle, time::timeout};
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

/// Whether the sink's `control` is `true` right now.
pub type ControlEnabled = Arc<dyn Fn() -> bool + Send + Sync>;

/// Reconnect delays: `initial`, doubling up to `max`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Backoff {
    pub initial: Duration,
    pub max: Duration,
}

/// Production reconnect: 1 s doubling to 30 s, the hub's bind backoff.
pub const RECONNECT_BACKOFF: Backoff = Backoff {
    initial: Duration::from_secs(1),
    max: Duration::from_secs(30),
};

impl Backoff {
    #[must_use]
    pub fn next(&self, previous: Option<Duration>) -> Duration {
        previous.map_or(self.initial, |previous| (previous * 2).min(self.max))
    }
}

/// Relay messages queued for the hub before frame senders wait; waiting
/// makes a stream's watcher lag and resync instead of slowing the pane.
const OUTBOUND_CAPACITY: usize = 256;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const PING_INTERVAL: Duration = Duration::from_secs(30);

/// Control channel URL for a hub sink URL: same host and port, path
/// `/control`, `ws` for `http` and `wss` for `https`.
pub fn control_url(sink_url: &str) -> Result<String> {
    let mut url = url_parts(sink_url)?;
    url.1.push_str("/control");
    Ok(format!("{}://{}", url.0, url.1))
}

fn url_parts(sink_url: &str) -> Result<(&'static str, String)> {
    let (scheme, rest) = sink_url
        .split_once("://")
        .context("sink URL has no scheme")?;
    let scheme = match scheme {
        "http" => "ws",
        "https" => "wss",
        other => anyhow::bail!("unsupported sink URL scheme: {other}"),
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    anyhow::ensure!(!authority.is_empty(), "sink URL has no host");
    Ok((scheme, authority.to_owned()))
}

/// One hub sink's control channel settings.
#[derive(Clone)]
pub struct ControlChannel {
    pub sink: String,
    pub source_id: String,
    pub url: String,
    pub auth: TokenSource,
    pub enabled: ControlEnabled,
    pub backoff: Backoff,
}

impl ControlChannel {
    /// Keeps the channel open for as long as the daemon runs. While
    /// `control` is off no channel is opened. The first failure of a streak
    /// and the next success are logged once each.
    pub async fn run(self, app: App) {
        let mut delay = None;
        let mut failing = false;
        loop {
            if !(self.enabled)() {
                tokio::time::sleep(self.backoff.max).await;
                continue;
            }
            match self.connect().await {
                Ok(ws) => {
                    if failing {
                        eprintln!(
                            "sessiontapd: hub sink '{}' control channel connected",
                            self.sink
                        );
                    }
                    failing = false;
                    delay = None;
                    let ended = serve(ws, &app, &self.source_id, &self.enabled).await;
                    if let Err(error) = ended {
                        eprintln!(
                            "sessiontapd: hub sink '{}' control channel closed: {error:#}; reconnecting",
                            self.sink
                        );
                        failing = true;
                    }
                }
                Err(error) => {
                    if !failing {
                        eprintln!(
                            "sessiontapd: hub sink '{}' control channel unavailable: {error:#}; retrying",
                            self.sink
                        );
                    }
                    failing = true;
                }
            }
            let wait = self.backoff.next(delay);
            delay = Some(wait);
            tokio::time::sleep(wait).await;
        }
    }

    async fn connect(
        &self,
    ) -> Result<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    > {
        let mut request = self.url.as_str().into_client_request()?;
        if let Some(token) = self.auth.token()? {
            request
                .headers_mut()
                .insert("authorization", format!("Bearer {token}").parse()?);
        }
        let (ws, _) = timeout(CONNECT_TIMEOUT, tokio_tungstenite::connect_async(request))
            .await
            .context("connect timed out")??;
        Ok(ws)
    }
}

/// Serves one open channel until it closes. Returns `Ok` when the daemon
/// closed it because `control` turned off.
async fn serve<S>(
    ws: tokio_tungstenite::WebSocketStream<S>,
    app: &App,
    source_id: &str,
    enabled: &ControlEnabled,
) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let (mut tx, mut rx) = ws.split();
    let hello = RelayMessage::Hello {
        source_id: source_id.to_owned(),
        protocol: RELAY_PROTOCOL_VERSION,
    };
    tx.send(Message::text(serde_json::to_string(&hello)?))
        .await?;
    let (out, mut outbound) = mpsc::channel(OUTBOUND_CAPACITY);
    let mut bridge = Bridge::new(app.clone(), enabled.clone(), out);
    let mut ping =
        tokio::time::interval_at(tokio::time::Instant::now() + PING_INTERVAL, PING_INTERVAL);
    loop {
        tokio::select! {
            Some(message) = outbound.recv() => {
                tx.send(Message::text(serde_json::to_string(&message)?)).await?;
            }
            incoming = rx.next() => match incoming {
                Some(Ok(Message::Text(text))) => {
                    let Ok(request) = serde_json::from_str::<RelayRequest>(text.as_str()) else {
                        continue;
                    };
                    if !bridge.handle(request).await {
                        // flush the refusal and the ended frames, then close
                        drop(bridge);
                        while let Ok(message) = outbound.try_recv() {
                            tx.send(Message::text(serde_json::to_string(&message)?)).await?;
                        }
                        let _ = tx.send(Message::Close(None)).await;
                        return Ok(());
                    }
                }
                Some(Ok(Message::Close(Some(frame)))) if !frame.reason.is_empty() => {
                    anyhow::bail!("hub closed the channel: {}", frame.reason)
                }
                Some(Ok(Message::Close(_))) | None => anyhow::bail!("hub closed the channel"),
                Some(Ok(_)) => {}
                Some(Err(error)) => return Err(error.into()),
            },
            _ = ping.tick() => {
                tx.send(Message::Ping(Vec::new().into())).await?;
            }
        }
    }
}

struct RelayStream {
    invocation: InvocationId,
    resync: mpsc::Sender<()>,
    task: JoinHandle<()>,
}

/// Maps relay requests of one channel onto live terminal sessions. One
/// pane stream per invocation is shared by all relay streams through the
/// session fan-out; each relay stream has its own watcher.
pub struct Bridge {
    app: App,
    enabled: ControlEnabled,
    out: mpsc::Sender<RelayMessage>,
    streams: HashMap<u64, RelayStream>,
}

fn refusal(error: &anyhow::Error) -> (String, String) {
    match error_to_response(error) {
        Response::Error(envelope) => (envelope.code, envelope.message),
        _ => ("request_failed".into(), error.to_string()),
    }
}

impl Bridge {
    #[must_use]
    pub fn new(app: App, enabled: ControlEnabled, out: mpsc::Sender<RelayMessage>) -> Self {
        Self {
            app,
            enabled,
            out,
            streams: HashMap::new(),
        }
    }

    async fn send(&self, message: RelayMessage) {
        let _ = self.out.send(message).await;
    }

    /// Handles one hub request. Returns `false` when `control` is off: the
    /// request was refused, every stream ended with
    /// `source_disallows_control`, and the channel must close.
    pub async fn handle(&mut self, request: RelayRequest) -> bool {
        match request {
            RelayRequest::Open {
                req,
                stream,
                invocation_id,
            } => {
                if !(self.enabled)() {
                    self.send(RelayMessage::Error {
                        req,
                        code: error_code::SOURCE_DISALLOWS_CONTROL.into(),
                        message: "the hub sink does not set control".into(),
                    })
                    .await;
                    self.disallow().await;
                    return false;
                }
                self.open(req, stream, &invocation_id).await;
            }
            RelayRequest::Input { req, stream, input } => {
                if !(self.enabled)() {
                    self.send(RelayMessage::InputResult {
                        req,
                        code: Some(error_code::SOURCE_DISALLOWS_CONTROL.into()),
                    })
                    .await;
                    self.disallow().await;
                    return false;
                }
                let code = self.input(stream, input).await;
                self.send(RelayMessage::InputResult { req, code }).await;
            }
            RelayRequest::Close { stream } => {
                if let Some(relay) = self.streams.remove(&stream) {
                    relay.task.abort();
                }
            }
            RelayRequest::Resync { stream } => {
                if let Some(relay) = self.streams.get(&stream) {
                    let _ = relay.resync.try_send(());
                }
            }
        }
        true
    }

    async fn open(&mut self, req: u64, stream: u64, invocation_id: &str) {
        let Ok(invocation) = invocation_id.parse::<InvocationId>() else {
            self.send(RelayMessage::Error {
                req,
                code: error_code::NOT_FOUND.into(),
                message: "unknown invocation".into(),
            })
            .await;
            return;
        };
        let watcher = match self.app.terminal_watch(&invocation) {
            Ok(watcher) => watcher,
            Err(error) => {
                let (code, message) = refusal(&error);
                self.send(RelayMessage::Error { req, code, message }).await;
                return;
            }
        };
        self.send(RelayMessage::Opened { req, stream }).await;
        let (resync, resyncs) = mpsc::channel(1);
        let task = tokio::spawn(forward(stream, watcher, resyncs, self.out.clone()));
        if let Some(previous) = self.streams.insert(
            stream,
            RelayStream {
                invocation,
                resync,
                task,
            },
        ) {
            previous.task.abort();
        }
    }

    /// Writes input through the pane-live-stream rules; `None` on success.
    async fn input(&mut self, stream: u64, input: TerminalInput) -> Option<String> {
        let Some(relay) = self.streams.get(&stream) else {
            return Some(error_code::TERMINAL_ENDED.into());
        };
        if relay.task.is_finished() {
            self.streams.remove(&stream);
            return Some(error_code::TERMINAL_ENDED.into());
        }
        let app = self.app.clone();
        let invocation = relay.invocation.clone();
        let written =
            tokio::task::spawn_blocking(move || app.terminal_input(&invocation, &input)).await;
        match written {
            Ok(Ok(())) => None,
            Ok(Err(error)) => Some(refusal(&error).0),
            Err(_) => Some("request_failed".into()),
        }
    }

    /// Ends every relay stream with `source_disallows_control`.
    async fn disallow(&mut self) {
        let streams: Vec<(u64, RelayStream)> = self.streams.drain().collect();
        for (stream, relay) in streams {
            relay.task.abort();
            self.send(RelayMessage::Frame {
                stream,
                frame: TerminalFrame::Ended {
                    reason: EndReason::SourceDisallowsControl,
                },
            })
            .await;
        }
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        for relay in self.streams.values() {
            relay.task.abort();
        }
    }
}

/// Forwards one watcher's frames unchanged until it ends.
async fn forward(
    stream: u64,
    mut watcher: Watcher,
    mut resyncs: mpsc::Receiver<()>,
    out: mpsc::Sender<RelayMessage>,
) {
    loop {
        tokio::select! {
            frame = watcher.next() => {
                let Some(frame) = frame else { return };
                let ended = matches!(frame, TerminalFrame::Ended { .. });
                if out.send(RelayMessage::Frame { stream, frame }).await.is_err() || ended {
                    return;
                }
            }
            Some(()) = resyncs.recv() => watcher.resync(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sessiontap_hub::{ingest, relay::Relay, store::HubStore};
    use sessiontap_storage::Storage;
    use std::net::SocketAddr;

    /// A hub ingestion listener on `addr`; aborting the handle stops it and
    /// every connection it accepted, like a hub restart.
    async fn hub_on(addr: SocketAddr) -> (Arc<Relay>, Arc<HubStore>, JoinHandle<()>, SocketAddr) {
        let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
        let bound = listener.local_addr().unwrap();
        let relay = Arc::new(Relay::default());
        let store = Arc::new(HubStore::memory().unwrap());
        let task = {
            let relay = relay.clone();
            let store = store.clone();
            tokio::spawn(async move {
                let mut connections = tokio::task::JoinSet::new();
                loop {
                    let (stream, _) = listener.accept().await.unwrap();
                    connections.spawn(ingest::serve_connection(
                        stream,
                        store.clone(),
                        Arc::new(ingest::IngestAuth::default()),
                        1024 * 1024,
                        relay.clone(),
                    ));
                }
            })
        };
        (relay, store, task, bound)
    }

    async fn until(condition: impl Fn() -> bool) {
        timeout(Duration::from_secs(5), async {
            while !condition() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("condition holds");
    }

    #[tokio::test]
    async fn channel_reconnects_after_hub_restart_and_ingestion_continues() {
        let (relay, _store, hub, addr) = hub_on("127.0.0.1:0".parse().unwrap()).await;
        let channel = ControlChannel {
            sink: "hub".into(),
            source_id: "host".into(),
            url: control_url(&format!("http://{addr}/ingest")).unwrap(),
            auth: TokenSource::None,
            enabled: Arc::new(|| true),
            backoff: Backoff {
                initial: Duration::from_millis(10),
                max: Duration::from_millis(50),
            },
        };
        let app = crate::app::tests::app(Storage::memory().unwrap());
        let client = tokio::spawn(channel.run(app));
        until(|| relay.has_channel("host")).await;
        hub.abort();
        let _ = hub.await;
        let (relay, store, _hub, _) = hub_on(addr).await;
        until(|| relay.has_channel("host")).await;
        let snapshot = serde_json::json!({
            "type": "snapshot", "schema_version": 1,
            "source": {"id": "host"}, "revision": 1, "views": [],
        });
        let response = reqwest::Client::new()
            .post(format!("http://{addr}/ingest"))
            .json(&snapshot)
            .send()
            .await
            .unwrap();
        assert!(response.status().is_success());
        assert!(store.has_source("host").unwrap());
        assert!(
            relay.has_channel("host"),
            "ingestion leaves the channel open"
        );
        client.abort();
    }

    #[tokio::test]
    async fn control_off_opens_no_channel() {
        let (relay, _store, _hub, addr) = hub_on("127.0.0.1:0".parse().unwrap()).await;
        let channel = ControlChannel {
            sink: "hub".into(),
            source_id: "host".into(),
            url: control_url(&format!("http://{addr}/ingest")).unwrap(),
            auth: TokenSource::None,
            enabled: Arc::new(|| false),
            backoff: Backoff {
                initial: Duration::from_millis(10),
                max: Duration::from_millis(20),
            },
        };
        let app = crate::app::tests::app(Storage::memory().unwrap());
        let client = tokio::spawn(channel.run(app));
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!relay.has_channel("host"));
        client.abort();
    }

    #[test]
    fn control_url_follows_the_sink_url() {
        assert_eq!(
            control_url("http://127.0.0.1:8931/ingest").unwrap(),
            "ws://127.0.0.1:8931/control"
        );
        assert_eq!(
            control_url("https://hub.example:8931/v1/envelopes?x=1").unwrap(),
            "wss://hub.example:8931/control"
        );
        assert_eq!(
            control_url("http://[::1]:8931").unwrap(),
            "ws://[::1]:8931/control"
        );
        assert!(control_url("ftp://hub/x").is_err());
    }

    #[test]
    fn reconnect_backoff_doubles_to_the_cap() {
        let mut delay = None;
        let waits: Vec<u64> = (0..7)
            .map(|_| {
                let wait = RECONNECT_BACKOFF.next(delay);
                delay = Some(wait);
                wait.as_secs()
            })
            .collect();
        assert_eq!(waits, vec![1, 2, 4, 8, 16, 30, 30]);
    }
}
