//! Terminal relay: one control channel per opted-in source, and the device
//! terminal streams multiplexed over it. The hub routes by source ID,
//! invocation ID, and stream ID only; the source daemon resolves the pane
//! and applies every input guard.

use futures_util::{SinkExt, StreamExt};
use sessiontap_core::{
    protocol::{RelayMessage, RelayRequest},
    terminal::{EndReason, TerminalFrame, TerminalInput, error_code},
};
use std::{
    collections::{BTreeSet, HashMap},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::AsyncWriteExt,
    net::TcpStream,
    sync::{mpsc, oneshot},
    time::timeout,
};
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{
        Message,
        handshake::derive_accept_key,
        protocol::{CloseFrame, Role, WebSocketConfig, frame::coding::CloseCode},
    },
};

/// Time a source has to answer `open` or `input`.
pub const ANSWER_TIMEOUT: Duration = Duration::from_secs(5);
/// Time a new control channel has to send `hello`.
pub const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
/// Largest control channel message; snapshots carry scrollback.
pub const MAX_CHANNEL_MESSAGE: usize = 16 * 1024 * 1024;
/// Largest raw output chunk pushed to a device in one message; its base64
/// form stays under the remote frame limit.
pub const MAX_OUTPUT_CHUNK: usize = 32 * 1024;
/// Frames held for a stream whose open answer is not yet sent.
const OPENING_BUFFER: usize = 256;

/// A source's answer to one correlated request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    Opened,
    /// Input result: `None` when written, else the refusal code.
    Input(Option<String>),
    Error {
        code: String,
        message: String,
    },
}

/// Relay failure answered to the device with its code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayError {
    pub code: &'static str,
    pub message: String,
}

impl RelayError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// The device side of a stream: its connection ID and outbound queue.
#[derive(Debug, Clone)]
pub struct DeviceOut {
    pub owner: u64,
    pub out: mpsc::Sender<Message>,
}

/// An answer the caller waits for, with what is needed to forget it.
pub struct Pending {
    source: String,
    channel: u64,
    req: u64,
    pub stream: u64,
    answer: oneshot::Receiver<Answer>,
}

struct Channel {
    id: u64,
    requests: mpsc::UnboundedSender<RelayRequest>,
    pending: HashMap<u64, oneshot::Sender<Answer>>,
}

struct Stream {
    source: String,
    channel: u64,
    device: DeviceOut,
    /// Frames wait here until the device has its open answer.
    opening: Option<Vec<TerminalFrame>>,
    /// Frames were dropped; skip until the next snapshot.
    lagging: bool,
    ended: bool,
}

#[derive(Default)]
struct State {
    channels: HashMap<String, Channel>,
    streams: HashMap<u64, Stream>,
    next_id: u64,
}

impl State {
    fn next(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    fn channel_of(&self, stream: &Stream) -> Option<&Channel> {
        self.channels
            .get(&stream.source)
            .filter(|channel| channel.id == stream.channel)
    }

    fn send(&self, stream: &Stream, request: RelayRequest) {
        if let Some(channel) = self.channel_of(stream) {
            let _ = channel.requests.send(request);
        }
    }

    /// Ends every stream relayed over channel `id` with `reason`.
    fn end_channel(&mut self, id: u64, reason: EndReason) {
        let ids: Vec<u64> = self
            .streams
            .iter()
            .filter(|(_, stream)| stream.channel == id && !stream.ended)
            .map(|(id, _)| *id)
            .collect();
        for stream_id in ids {
            self.push(stream_id, TerminalFrame::Ended { reason });
        }
    }

    /// Pushes one source frame to the stream's device in source order.
    fn push(&mut self, stream_id: u64, frame: TerminalFrame) {
        let Some(stream) = self.streams.get_mut(&stream_id) else {
            return;
        };
        if stream.ended {
            return;
        }
        let ended = matches!(frame, TerminalFrame::Ended { .. });
        if let Some(buffer) = &mut stream.opening {
            if ended {
                buffer.clear();
                buffer.push(frame);
            } else if buffer.len() >= OPENING_BUFFER {
                buffer.clear();
                stream.lagging = true;
                self.resync(stream_id);
            } else {
                buffer.push(frame);
            }
            return;
        }
        let snapshot = matches!(frame, TerminalFrame::Snapshot { .. });
        if stream.lagging && !snapshot && !ended {
            return;
        }
        stream.lagging = false;
        stream.ended = ended;
        let mut full = false;
        for message in device_messages(stream_id, frame) {
            if let Err(mpsc::error::TrySendError::Full(_)) = stream.device.out.try_send(message) {
                full = true;
                break;
            }
        }
        if full && !ended {
            stream.lagging = true;
            self.resync(stream_id);
        }
    }

    fn resync(&self, stream_id: u64) {
        if let Some(stream) = self.streams.get(&stream_id) {
            self.send(stream, RelayRequest::Resync { stream: stream_id });
        }
    }
}

/// Pushed device messages for one frame. Output larger than
/// [`MAX_OUTPUT_CHUNK`] is split into several `output` frames.
fn device_messages(stream: u64, frame: TerminalFrame) -> Vec<Message> {
    let frames = match frame {
        TerminalFrame::Output { seq, data } if data.len() > MAX_OUTPUT_CHUNK => data
            .chunks(MAX_OUTPUT_CHUNK)
            .map(|chunk| TerminalFrame::Output {
                seq,
                data: chunk.to_vec(),
            })
            .collect(),
        frame => vec![frame],
    };
    frames
        .into_iter()
        .map(|frame| {
            Message::text(
                serde_json::json!({"type": "terminal", "stream": stream, "frame": frame})
                    .to_string(),
            )
        })
        .collect()
}

/// Every control channel and terminal stream of the hub.
pub struct Relay {
    state: Mutex<State>,
    answer_timeout: Duration,
}

impl Default for Relay {
    fn default() -> Self {
        Self::new(ANSWER_TIMEOUT)
    }
}

impl Relay {
    #[must_use]
    pub fn new(answer_timeout: Duration) -> Self {
        Self {
            state: Mutex::new(State::default()),
            answer_timeout,
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().expect("relay state poisoned")
    }

    /// Binds a new control channel to `source`, replacing an older one
    /// whose streams end with `source_unavailable`.
    pub fn attach(&self, source: &str) -> (u64, mpsc::UnboundedReceiver<RelayRequest>) {
        let (requests, receiver) = mpsc::unbounded_channel();
        let mut state = self.state();
        let id = state.next();
        let previous = state.channels.insert(
            source.to_owned(),
            Channel {
                id,
                requests,
                pending: HashMap::new(),
            },
        );
        if let Some(previous) = previous {
            state.end_channel(previous.id, EndReason::SourceUnavailable);
        }
        (id, receiver)
    }

    /// Drops channel `id` of `source` when it is still current; its streams
    /// end with `source_unavailable` and its waiters get no answer.
    pub fn detach(&self, source: &str, id: u64) {
        let mut state = self.state();
        if state
            .channels
            .get(source)
            .is_some_and(|channel| channel.id == id)
        {
            state.channels.remove(source);
        }
        state.end_channel(id, EndReason::SourceUnavailable);
    }

    #[must_use]
    pub fn has_channel(&self, source: &str) -> bool {
        self.state().channels.contains_key(source)
    }

    /// Opens a stream on `source`'s channel. The device must get the
    /// answer before [`Relay::activate`] releases the stream's frames.
    pub fn open(
        &self,
        device: &DeviceOut,
        source: &str,
        invocation_id: &str,
    ) -> Result<Pending, RelayError> {
        let mut state = self.state();
        let stream = state.next();
        let req = state.next();
        let Some(channel) = state.channels.get_mut(source) else {
            return Err(RelayError::new(
                error_code::SOURCE_DISALLOWS_CONTROL,
                format!("source '{source}' has no control channel"),
            ));
        };
        let (answer, receiver) = oneshot::channel();
        channel.pending.insert(req, answer);
        let channel_id = channel.id;
        let _ = channel.requests.send(RelayRequest::Open {
            req,
            stream,
            invocation_id: invocation_id.to_owned(),
        });
        state.streams.insert(
            stream,
            Stream {
                source: source.to_owned(),
                channel: channel_id,
                device: device.clone(),
                opening: Some(Vec::new()),
                lagging: false,
                ended: false,
            },
        );
        Ok(Pending {
            source: source.to_owned(),
            channel: channel_id,
            req,
            stream,
            answer: receiver,
        })
    }

    /// Forwards input for a stream `owner` opened.
    pub fn input(
        &self,
        owner: u64,
        stream_id: u64,
        input: TerminalInput,
    ) -> Result<Pending, RelayError> {
        let mut state = self.state();
        let req = state.next();
        let Some(stream) = state.streams.get(&stream_id) else {
            return Err(RelayError::new(
                error_code::BAD_REQUEST,
                format!("unknown stream {stream_id}"),
            ));
        };
        if stream.device.owner != owner {
            return Err(RelayError::new(
                "forbidden",
                "the stream belongs to another connection",
            ));
        }
        if stream.ended {
            return Err(RelayError::new(
                error_code::TERMINAL_ENDED,
                "terminal stream ended",
            ));
        }
        let (source, channel_id) = (stream.source.clone(), stream.channel);
        let Some(channel) = state
            .channels
            .get_mut(&source)
            .filter(|channel| channel.id == channel_id)
        else {
            return Err(RelayError::new(
                error_code::SOURCE_UNAVAILABLE,
                "the source's control channel closed",
            ));
        };
        let (answer, receiver) = oneshot::channel();
        channel.pending.insert(req, answer);
        let _ = channel.requests.send(RelayRequest::Input {
            req,
            stream: stream_id,
            input,
        });
        Ok(Pending {
            source,
            channel: channel_id,
            req,
            stream: stream_id,
            answer: receiver,
        })
    }

    /// Waits for the source's answer; no answer in time is
    /// `source_unavailable`.
    pub async fn answer(&self, pending: Pending) -> Result<Answer, RelayError> {
        let Pending {
            source,
            channel,
            req,
            answer,
            ..
        } = pending;
        match timeout(self.answer_timeout, answer).await {
            Ok(Ok(answer)) => Ok(answer),
            Ok(Err(_)) => Err(RelayError::new(
                error_code::SOURCE_UNAVAILABLE,
                "the source's control channel closed",
            )),
            Err(_) => {
                let mut state = self.state();
                if let Some(channel) = state
                    .channels
                    .get_mut(&source)
                    .filter(|known| known.id == channel)
                {
                    channel.pending.remove(&req);
                }
                Err(RelayError::new(
                    error_code::SOURCE_UNAVAILABLE,
                    "the source did not answer in time",
                ))
            }
        }
    }

    /// Releases the frames of a stream whose open answer the device has.
    pub fn activate(&self, stream_id: u64) {
        let mut state = self.state();
        let Some(buffer) = state
            .streams
            .get_mut(&stream_id)
            .and_then(|stream| stream.opening.take())
        else {
            return;
        };
        for frame in buffer {
            state.push(stream_id, frame);
        }
    }

    /// Forgets a stream whose open failed and releases it on the source.
    pub fn abandon(&self, stream_id: u64) {
        let mut state = self.state();
        if let Some(stream) = state.streams.remove(&stream_id) {
            state.send(&stream, RelayRequest::Close { stream: stream_id });
        }
    }

    /// Ends a stream `owner` opened with `closed` and releases it on the
    /// source.
    pub fn close(&self, owner: u64, stream_id: u64) -> Result<(), RelayError> {
        let mut state = self.state();
        match state.streams.get(&stream_id) {
            None => {
                return Err(RelayError::new(
                    error_code::BAD_REQUEST,
                    format!("unknown stream {stream_id}"),
                ));
            }
            Some(stream) if stream.device.owner != owner => {
                return Err(RelayError::new(
                    "forbidden",
                    "the stream belongs to another connection",
                ));
            }
            Some(_) => {}
        }
        if let Some(stream) = state.streams.get_mut(&stream_id) {
            stream.opening = None;
            stream.lagging = false;
        }
        state.push(
            stream_id,
            TerminalFrame::Ended {
                reason: EndReason::Closed,
            },
        );
        if let Some(stream) = state.streams.remove(&stream_id) {
            state.send(&stream, RelayRequest::Close { stream: stream_id });
        }
        Ok(())
    }

    /// Drops every stream of a closed connection and releases them on the
    /// sources. Sends nothing to the device.
    pub fn release_owner(&self, owner: u64) {
        let mut state = self.state();
        let ids: Vec<u64> = state
            .streams
            .iter()
            .filter(|(_, stream)| stream.device.owner == owner)
            .map(|(id, _)| *id)
            .collect();
        for stream_id in ids {
            if let Some(stream) = state.streams.remove(&stream_id) {
                state.send(&stream, RelayRequest::Close { stream: stream_id });
            }
        }
    }

    /// Number of live relay streams, for tests.
    #[doc(hidden)]
    #[must_use]
    pub fn stream_count(&self) -> usize {
        self.state().streams.len()
    }

    /// Routes one message from channel `id` of `source`.
    pub fn deliver(&self, source: &str, id: u64, message: RelayMessage) {
        let mut state = self.state();
        let answer = match message {
            RelayMessage::Hello { .. } => return,
            RelayMessage::Frame { stream, frame } => {
                if state
                    .streams
                    .get(&stream)
                    .is_some_and(|known| known.channel == id)
                {
                    state.push(stream, frame);
                }
                return;
            }
            RelayMessage::Opened { req, .. } => (req, Answer::Opened),
            RelayMessage::InputResult { req, code } => (req, Answer::Input(code)),
            RelayMessage::Error { req, code, message } => (req, Answer::Error { code, message }),
        };
        if let Some(waiter) = state
            .channels
            .get_mut(source)
            .filter(|channel| channel.id == id)
            .and_then(|channel| channel.pending.remove(&answer.0))
        {
            let _ = waiter.send(answer.1);
        }
    }
}

fn channel_config() -> WebSocketConfig {
    WebSocketConfig::default()
        .max_message_size(Some(MAX_CHANNEL_MESSAGE))
        .max_frame_size(Some(MAX_CHANNEL_MESSAGE))
}

/// Completes the WebSocket upgrade of an authenticated `GET /control` and
/// serves the channel until it closes or is replaced. `permitted` is the
/// token's source set, or `None` for tokenless loopback ingestion.
pub async fn serve_channel(
    mut stream: TcpStream,
    websocket_key: &str,
    relay: Arc<Relay>,
    permitted: Option<BTreeSet<String>>,
) {
    let head = format!(
        "HTTP/1.1 101 Switching Protocols\r\nupgrade: websocket\r\nconnection: Upgrade\r\nsec-websocket-accept: {}\r\n\r\n",
        derive_accept_key(websocket_key.as_bytes())
    );
    if stream.write_all(head.as_bytes()).await.is_err() {
        return;
    }
    let ws = WebSocketStream::from_raw_socket(stream, Role::Server, Some(channel_config())).await;
    let (mut tx, mut rx) = ws.split();
    let hello = timeout(HELLO_TIMEOUT, async {
        loop {
            match rx.next().await {
                Some(Ok(Message::Text(text))) => {
                    return serde_json::from_str::<RelayMessage>(text.as_str()).ok();
                }
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                _ => return None,
            }
        }
    })
    .await;
    let Ok(Some(RelayMessage::Hello { source_id, .. })) = hello else {
        let _ = tx.send(close(CloseCode::Protocol, "expected hello")).await;
        return;
    };
    if permitted
        .as_ref()
        .is_some_and(|permitted| !permitted.contains(&source_id))
    {
        let _ = tx
            .send(close(CloseCode::Policy, "source_not_permitted"))
            .await;
        return;
    }
    let (id, mut requests) = relay.attach(&source_id);
    loop {
        tokio::select! {
            request = requests.recv() => match request {
                Some(request) => {
                    let Ok(text) = serde_json::to_string(&request) else { continue };
                    if tx.send(Message::text(text)).await.is_err() {
                        break;
                    }
                }
                None => {
                    let _ = tx.send(close(CloseCode::Normal, "replaced")).await;
                    break;
                }
            },
            incoming = rx.next() => match incoming {
                Some(Ok(Message::Text(text))) => {
                    if let Ok(message) = serde_json::from_str::<RelayMessage>(text.as_str()) {
                        relay.deliver(&source_id, id, message);
                    }
                }
                Some(Ok(Message::Close(_)) | Err(_)) | None => break,
                Some(Ok(_)) => {}
            },
        }
    }
    relay.detach(&source_id, id);
}

fn close(code: CloseCode, reason: &str) -> Message {
    Message::Close(Some(CloseFrame {
        code,
        reason: reason.to_owned().into(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sessiontap_core::terminal::{Cursor, InputState, Key};

    fn device(owner: u64, capacity: usize) -> (DeviceOut, mpsc::Receiver<Message>) {
        let (out, receiver) = mpsc::channel(capacity);
        (DeviceOut { owner, out }, receiver)
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
            data: b"menu".to_vec(),
            input: InputState::from_guard(None),
        }
    }

    fn output(seq: u64, data: &[u8]) -> TerminalFrame {
        TerminalFrame::Output {
            seq,
            data: data.to_vec(),
        }
    }

    fn pushed(receiver: &mut mpsc::Receiver<Message>) -> Vec<serde_json::Value> {
        let mut out = Vec::new();
        while let Ok(message) = receiver.try_recv() {
            out.push(serde_json::from_str(message.to_text().unwrap()).unwrap());
        }
        out
    }

    fn requests(receiver: &mut mpsc::UnboundedReceiver<RelayRequest>) -> Vec<RelayRequest> {
        let mut out = Vec::new();
        while let Ok(request) = receiver.try_recv() {
            out.push(request);
        }
        out
    }

    /// Opens a stream and answers it, returning the stream ID.
    async fn opened(relay: &Relay, channel: u64, device: &DeviceOut) -> u64 {
        let pending = relay.open(device, "host", "inv").unwrap();
        let stream = pending.stream;
        relay.deliver(
            "host",
            channel,
            RelayMessage::Opened {
                req: pending.req,
                stream,
            },
        );
        assert_eq!(relay.answer(pending).await.unwrap(), Answer::Opened);
        relay.activate(stream);
        stream
    }

    #[tokio::test]
    async fn frames_wait_for_the_open_answer_and_keep_order() {
        let relay = Relay::default();
        let (channel, mut source) = relay.attach("host");
        let (phone, mut inbox) = device(1, 64);
        let pending = relay.open(&phone, "host", "inv").unwrap();
        let stream = pending.stream;
        let [RelayRequest::Open { req, .. }] = requests(&mut source)[..] else {
            panic!("expected open");
        };
        relay.deliver("host", channel, RelayMessage::Opened { req, stream });
        relay.deliver(
            "host",
            channel,
            RelayMessage::Frame {
                stream,
                frame: snapshot(0),
            },
        );
        relay.deliver(
            "host",
            channel,
            RelayMessage::Frame {
                stream,
                frame: output(1, b"a"),
            },
        );
        assert!(pushed(&mut inbox).is_empty(), "held until activation");
        assert_eq!(relay.answer(pending).await.unwrap(), Answer::Opened);
        relay.activate(stream);
        relay.deliver(
            "host",
            channel,
            RelayMessage::Frame {
                stream,
                frame: output(2, b"b"),
            },
        );
        let types: Vec<_> = pushed(&mut inbox)
            .into_iter()
            .map(|message| {
                assert_eq!(message["type"], "terminal");
                assert_eq!(message["stream"], stream);
                (
                    message["frame"]["type"].clone(),
                    message["frame"]["seq"].clone(),
                )
            })
            .collect();
        assert_eq!(
            types,
            vec![
                ("snapshot".into(), 0.into()),
                ("output".into(), 1.into()),
                ("output".into(), 2.into())
            ]
        );
    }

    #[tokio::test]
    async fn unanswered_requests_time_out_as_source_unavailable() {
        let relay = Relay::new(Duration::from_millis(50));
        let (_channel, _source) = relay.attach("host");
        let (phone, _inbox) = device(1, 8);
        let pending = relay.open(&phone, "host", "inv").unwrap();
        let error = relay.answer(pending).await.unwrap_err();
        assert_eq!(error.code, error_code::SOURCE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn no_channel_is_source_disallows_control() {
        let relay = Relay::default();
        let (phone, _inbox) = device(1, 8);
        let error = relay.open(&phone, "host", "inv").err().unwrap();
        assert_eq!(error.code, error_code::SOURCE_DISALLOWS_CONTROL);
    }

    #[tokio::test]
    async fn replacement_ends_streams_of_the_old_channel() {
        let relay = Relay::default();
        let (old, mut old_requests) = relay.attach("host");
        let (phone, mut inbox) = device(1, 64);
        let stream = opened(&relay, old, &phone).await;
        let (new, _requests) = relay.attach("host");
        assert_ne!(old, new);
        let messages = pushed(&mut inbox);
        assert_eq!(
            messages.last().unwrap()["frame"],
            serde_json::json!({"type": "ended", "reason": "source_unavailable"})
        );
        let error = relay
            .input(1, stream, TerminalInput::Keys(vec![Key::Char('1')]))
            .err()
            .unwrap();
        assert_eq!(error.code, error_code::TERMINAL_ENDED);
        requests(&mut old_requests);
        // the old channel's task ends once the relay drops its sender
        assert!(old_requests.recv().await.is_none());
    }

    #[tokio::test]
    async fn input_is_refused_on_foreign_streams_and_close_releases() {
        let relay = Relay::default();
        let (channel, mut source) = relay.attach("host");
        let (phone, mut inbox) = device(1, 64);
        let stream = opened(&relay, channel, &phone).await;
        requests(&mut source);
        let error = relay
            .input(2, stream, TerminalInput::Keys(vec![Key::Char('1')]))
            .err()
            .unwrap();
        assert_eq!(error.code, "forbidden");
        assert!(requests(&mut source).is_empty(), "nothing forwarded");
        assert_eq!(relay.close(2, stream).unwrap_err().code, "forbidden");
        relay.close(1, stream).unwrap();
        assert_eq!(requests(&mut source), vec![RelayRequest::Close { stream }]);
        assert_eq!(
            pushed(&mut inbox).last().unwrap()["frame"]["reason"],
            "closed"
        );
        relay.deliver(
            "host",
            channel,
            RelayMessage::Frame {
                stream,
                frame: output(9, b"late"),
            },
        );
        assert!(pushed(&mut inbox).is_empty(), "no frames after close");
    }

    #[tokio::test]
    async fn released_owner_closes_its_streams_on_the_source() {
        let relay = Relay::default();
        let (channel, mut source) = relay.attach("host");
        let (phone, mut inbox) = device(1, 64);
        let (other, _other_inbox) = device(2, 64);
        let mine = opened(&relay, channel, &phone).await;
        let theirs = opened(&relay, channel, &other).await;
        requests(&mut source);
        relay.release_owner(1);
        assert_eq!(
            requests(&mut source),
            vec![RelayRequest::Close { stream: mine }]
        );
        assert!(
            pushed(&mut inbox).is_empty(),
            "device sees only the close code"
        );
        assert_eq!(relay.stream_count(), 1);
        relay
            .input(2, theirs, TerminalInput::Keys(vec![Key::Char('1')]))
            .unwrap();
    }

    #[tokio::test]
    async fn large_output_is_split_under_the_frame_limit() {
        let relay = Relay::default();
        let (channel, _source) = relay.attach("host");
        let (phone, mut inbox) = device(1, 64);
        let stream = opened(&relay, channel, &phone).await;
        let data: Vec<u8> = (0..MAX_OUTPUT_CHUNK * 2 + 10).map(|i| i as u8).collect();
        relay.deliver(
            "host",
            channel,
            RelayMessage::Frame {
                stream,
                frame: output(3, &data),
            },
        );
        let mut joined = Vec::new();
        let messages: Vec<Message> = std::iter::from_fn(|| inbox.try_recv().ok()).collect();
        assert_eq!(messages.len(), 3);
        for message in messages {
            assert!(message.len() <= crate::remote::MAX_FRAME_SIZE);
            let value: serde_json::Value =
                serde_json::from_str(message.to_text().unwrap()).unwrap();
            let frame: TerminalFrame = serde_json::from_value(value["frame"].clone()).unwrap();
            let TerminalFrame::Output { seq: 3, data } = frame else {
                panic!("expected output");
            };
            joined.extend(data);
        }
        assert_eq!(joined, data);
    }

    #[tokio::test]
    async fn stalled_device_drops_backlog_and_resyncs() {
        let relay = Relay::default();
        let (channel, mut source) = relay.attach("host");
        let (phone, mut inbox) = device(1, 4);
        let stream = opened(&relay, channel, &phone).await;
        requests(&mut source);
        for seq in 0..10 {
            relay.deliver(
                "host",
                channel,
                RelayMessage::Frame {
                    stream,
                    frame: output(seq, b"x"),
                },
            );
        }
        assert_eq!(requests(&mut source), vec![RelayRequest::Resync { stream }]);
        assert_eq!(pushed(&mut inbox).len(), 4);
        relay.deliver(
            "host",
            channel,
            RelayMessage::Frame {
                stream,
                frame: output(10, b"skipped"),
            },
        );
        relay.deliver(
            "host",
            channel,
            RelayMessage::Frame {
                stream,
                frame: snapshot(11),
            },
        );
        let after = pushed(&mut inbox);
        assert_eq!(after.len(), 1);
        assert_eq!(after[0]["frame"]["type"], "snapshot");
    }
}
