//! Live terminal behaviour against a fake multiplexer adapter.

use crate::{
    app::{
        App, Collection, PublishConfig,
        tests::{app as headless_app, event, snapshot},
    },
    terminal::{TerminalError, Watcher},
};
use anyhow::Result;
use sessiontap_adapters::AdapterRegistry;
use sessiontap_core::{
    config::{Config, DaemonConfig},
    domain::{
        EventKind, InvocationSnapshot, Lifecycle, MultiplexerBackend, MultiplexerMetadata,
        PublicField, PublicStatus,
    },
    terminal::{
        Cursor, EndReason, InputUnavailable, Key, QuickPick, TerminalDescriptor, TerminalFrame,
        TerminalInput, error_code,
    },
};
use sessiontap_infra::{
    multiplexer::{
        MultiplexerAdapter, MultiplexerRegistry, PaneEvent, PaneSnapshot, PaneState, PaneStream,
        PaneStreamControl,
    },
    process::process_start_identity,
};
use sessiontap_storage::Storage;
use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::mpsc;

struct Fake {
    state: Mutex<PaneState>,
    events: Mutex<Option<mpsc::UnboundedSender<PaneEvent>>>,
    snapshots: Arc<AtomicUsize>,
    opened: AtomicUsize,
    control_dropped: Arc<AtomicBool>,
    sent: Mutex<Vec<String>>,
}

impl Fake {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(PaneState {
                foreground_pgid: Some(std::process::id()),
                in_mode: false,
            }),
            events: Mutex::new(None),
            snapshots: Arc::new(AtomicUsize::new(0)),
            opened: AtomicUsize::new(0),
            control_dropped: Arc::new(AtomicBool::new(false)),
            sent: Mutex::new(Vec::new()),
        })
    }

    fn push(&self, event: PaneEvent) {
        let _ = self.events.lock().unwrap().as_ref().unwrap().send(event);
    }

    fn set(&self, change: impl FnOnce(&mut PaneState)) {
        change(&mut self.state.lock().unwrap());
    }
}

fn pane_snapshot() -> PaneEvent {
    PaneEvent::Snapshot(PaneSnapshot {
        cols: 80,
        rows: 24,
        cursor: Cursor {
            x: 0,
            y: 0,
            visible: true,
        },
        alternate_screen: false,
        in_mode: false,
        data: b"snap".to_vec(),
    })
}

struct FakeControl {
    events: mpsc::UnboundedSender<PaneEvent>,
    snapshots: Arc<AtomicUsize>,
    dropped: Arc<AtomicBool>,
}

impl PaneStreamControl for FakeControl {
    fn request_snapshot(&self) -> Result<()> {
        self.snapshots.fetch_add(1, Ordering::SeqCst);
        let _ = self.events.send(pane_snapshot());
        Ok(())
    }
}

impl Drop for FakeControl {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::SeqCst);
    }
}

impl MultiplexerAdapter for Fake {
    fn inspect(&self) -> Result<Option<MultiplexerMetadata>> {
        Ok(None)
    }
    fn capture(&self, _: &MultiplexerMetadata, _: u32) -> Result<String> {
        Ok(String::new())
    }
    fn open_stream(&self, _: &MultiplexerMetadata, _: u32) -> Result<PaneStream> {
        self.opened.fetch_add(1, Ordering::SeqCst);
        let (sender, events) = mpsc::unbounded_channel();
        sender.send(pane_snapshot()).unwrap();
        *self.events.lock().unwrap() = Some(sender.clone());
        Ok(PaneStream {
            events,
            control: Arc::new(FakeControl {
                events: sender,
                snapshots: self.snapshots.clone(),
                dropped: self.control_dropped.clone(),
            }),
        })
    }
    fn send_keys(&self, _: &MultiplexerMetadata, _: u32, keys: &[Key]) -> Result<()> {
        self.sent
            .lock()
            .unwrap()
            .extend(keys.iter().map(ToString::to_string));
        Ok(())
    }
    fn paste(&self, _: &MultiplexerMetadata, _: u32, text: &str, _: bool) -> Result<()> {
        self.sent.lock().unwrap().push(text.into());
        Ok(())
    }
    fn pane_state(&self, _: &MultiplexerMetadata, _: u32) -> Result<PaneState> {
        Ok(*self.state.lock().unwrap())
    }
}

fn app_with(fake: &Arc<Fake>) -> App {
    let mut config = Config::default();
    config.adapters.insert(
        "company-claude".into(),
        sessiontap_core::config::CustomAdapter {
            executable: "company-claude".into(),
            inherits: "claude".into(),
        },
    );
    App::new(
        Arc::new(Storage::memory().unwrap()),
        PublishConfig::default(),
        &DaemonConfig::default(),
        Arc::new(MultiplexerRegistry::empty().with_adapter(MultiplexerBackend::Tmux, fake.clone())),
        Collection {
            home: PathBuf::from("/nonexistent"),
            registry: Arc::new(AdapterRegistry::new(&config)),
        },
    )
}

fn interactive(provider: &str) -> InvocationSnapshot {
    let mut initial = snapshot();
    initial.provider = provider.into();
    initial.lifecycle = Lifecycle::Alive;
    initial.process.child_pid = Some(std::process::id());
    initial.process.start_identity = process_start_identity(std::process::id());
    initial.capabilities.terminal = true;
    initial.multiplexer = Some(MultiplexerMetadata {
        backend: MultiplexerBackend::Tmux,
        socket: "/fake".into(),
        pane_id: "%1".into(),
        ..Default::default()
    });
    initial
}

fn registered(fake: &Arc<Fake>) -> (App, InvocationSnapshot) {
    let app = app_with(fake);
    let initial = interactive("claude");
    app.register(initial.clone(), "credential").unwrap();
    (app, initial)
}

async fn next(watcher: &mut Watcher) -> Option<TerminalFrame> {
    tokio::time::timeout(Duration::from_secs(5), watcher.next())
        .await
        .expect("frame")
}

/// Frames that arrive within `window`.
async fn drain(watcher: &mut Watcher, window: Duration) -> Vec<TerminalFrame> {
    let mut frames = Vec::new();
    let deadline = tokio::time::Instant::now() + window;
    while let Ok(Some(frame)) = tokio::time::timeout_at(deadline, watcher.next()).await {
        frames.push(frame);
    }
    frames
}

fn code(error: &anyhow::Error) -> &'static str {
    error.downcast_ref::<TerminalError>().expect("typed").code
}

fn keys(text: &str) -> TerminalInput {
    TerminalInput::Keys(text.chars().map(Key::char).collect())
}

#[tokio::test]
async fn stalled_watcher_does_not_block_and_resyncs_with_snapshot() {
    let fake = Fake::new();
    let (app, initial) = registered(&fake);
    let mut stalled = app.terminal_watch(&initial.invocation_id).unwrap();
    assert!(matches!(
        next(&mut stalled).await,
        Some(TerminalFrame::Snapshot { .. })
    ));
    let mut reading = app.terminal_watch(&initial.invocation_id).unwrap();
    assert!(matches!(
        next(&mut reading).await,
        Some(TerminalFrame::Snapshot { .. })
    ));
    assert_eq!(fake.opened.load(Ordering::SeqCst), 1);

    let mut received = 0;
    for batch in 0..10 {
        for line in 0..100 {
            fake.push(PaneEvent::Output(format!("{batch}:{line}\n").into_bytes()));
        }
        while received < (batch + 1) * 100 {
            if let Some(TerminalFrame::Output { .. }) = next(&mut reading).await {
                received += 1;
            }
        }
    }
    let requested = fake.snapshots.load(Ordering::SeqCst);
    // The stalled watcher skipped the missed output and gets a snapshot.
    let mut frame = next(&mut stalled).await;
    while matches!(frame, Some(TerminalFrame::Snapshot { .. }))
        && fake.snapshots.load(Ordering::SeqCst) == requested
    {
        // A snapshot still buffered from before the lag.
        frame = next(&mut stalled).await;
    }
    assert!(
        matches!(frame, Some(TerminalFrame::Snapshot { .. })),
        "{frame:?}"
    );
    assert!(fake.snapshots.load(Ordering::SeqCst) > requested);

    drop(stalled);
    assert!(app.terminal_watch(&initial.invocation_id).is_ok());
    drop(reading);
}

#[tokio::test]
async fn last_watcher_leaving_closes_the_stream() {
    let fake = Fake::new();
    let (app, initial) = registered(&fake);
    let first = app.terminal_watch(&initial.invocation_id).unwrap();
    let second = app.terminal_watch(&initial.invocation_id).unwrap();
    drop(first);
    assert!(!fake.control_dropped.load(Ordering::SeqCst));
    drop(second);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(fake.control_dropped.load(Ordering::SeqCst));
}

#[tokio::test]
async fn availability_and_guard_answer_typed_codes() {
    let fake = Fake::new();
    let (app, initial) = registered(&fake);
    let id = &initial.invocation_id;

    let unknown = sessiontap_core::domain::InvocationId::new();
    assert_eq!(
        code(&app.terminal_input(&unknown, &keys("1")).unwrap_err()),
        error_code::NOT_FOUND
    );
    assert_eq!(
        code(&app.terminal_watch(&unknown).err().unwrap()),
        error_code::NOT_FOUND
    );

    let mut headless = interactive("claude");
    headless.capabilities.terminal = false;
    app.register(headless.clone(), "credential").unwrap();
    for error in [
        app.terminal_input(&headless.invocation_id, &keys("1"))
            .unwrap_err(),
        app.terminal_watch(&headless.invocation_id).err().unwrap(),
    ] {
        assert_eq!(code(&error), error_code::TERMINAL_UNAVAILABLE);
    }
    let mut outside = interactive("claude");
    outside.multiplexer = None;
    app.register(outside.clone(), "credential").unwrap();
    assert_eq!(
        code(&app.terminal_watch(&outside.invocation_id).err().unwrap()),
        error_code::TERMINAL_UNAVAILABLE
    );
    assert_eq!(fake.opened.load(Ordering::SeqCst), 0);

    assert_eq!(
        code(
            &app.terminal_input(id, &TerminalInput::Keys(vec![]))
                .unwrap_err()
        ),
        error_code::BAD_REQUEST
    );

    fake.set(|state| state.foreground_pgid = Some(1));
    assert_eq!(
        code(&app.terminal_input(id, &keys("1")).unwrap_err()),
        error_code::NOT_FOREGROUND
    );
    fake.set(|state| {
        state.foreground_pgid = Some(std::process::id());
        state.in_mode = true;
    });
    assert_eq!(
        code(&app.terminal_input(id, &keys("1")).unwrap_err()),
        error_code::PANE_IN_MODE
    );
    assert!(fake.sent.lock().unwrap().is_empty());

    fake.set(|state| state.in_mode = false);
    app.terminal_input(id, &keys("1")).unwrap();
    app.terminal_input(
        id,
        &TerminalInput::Paste {
            text: "hi".into(),
            enter: true,
        },
    )
    .unwrap();
    assert_eq!(*fake.sent.lock().unwrap(), vec!["1", "hi"]);
}

#[tokio::test]
async fn foreground_change_pushes_exactly_one_input_frame() {
    let fake = Fake::new();
    let (app, initial) = registered(&fake);
    let mut watcher = app.terminal_watch(&initial.invocation_id).unwrap();
    let Some(TerminalFrame::Snapshot { input, .. }) = next(&mut watcher).await else {
        panic!("snapshot first");
    };
    assert!(input.available);
    fake.set(|state| state.foreground_pgid = Some(1));
    let frames = drain(&mut watcher, Duration::from_millis(900)).await;
    let inputs: Vec<_> = frames
        .iter()
        .filter_map(|frame| match frame {
            TerminalFrame::Input(input) => Some(*input),
            _ => None,
        })
        .collect();
    assert_eq!(inputs.len(), 1, "{frames:?}");
    assert_eq!(inputs[0].reason, Some(InputUnavailable::NotForeground));

    fake.push(PaneEvent::ModeChanged(true));
    fake.set(|state| state.foreground_pgid = Some(std::process::id()));
    let frames = drain(&mut watcher, Duration::from_millis(600)).await;
    assert!(frames.iter().any(|frame| matches!(
        frame,
        TerminalFrame::Input(input) if input.reason == Some(InputUnavailable::PaneInMode)
    )));
}

#[tokio::test]
async fn agent_exit_ends_stream_and_drops_later_output() {
    let fake = Fake::new();
    let (app, initial) = registered(&fake);
    let id = &initial.invocation_id;
    let mut watcher = app.terminal_watch(id).unwrap();
    assert!(matches!(
        next(&mut watcher).await,
        Some(TerminalFrame::Snapshot { .. })
    ));
    fake.push(PaneEvent::Output(b"before".to_vec()));
    assert!(matches!(
        next(&mut watcher).await,
        Some(TerminalFrame::Output { .. })
    ));

    app.lifecycle_exit(id, "credential", Some(0), None).unwrap();
    fake.push(PaneEvent::Output(b"$ prompt".to_vec()));
    assert_eq!(
        next(&mut watcher).await,
        Some(TerminalFrame::Ended {
            reason: EndReason::AgentExited
        })
    );
    assert_eq!(next(&mut watcher).await, None);
    assert_eq!(
        code(&app.terminal_input(id, &keys("1")).unwrap_err()),
        error_code::TERMINAL_ENDED
    );
    assert!(fake.sent.lock().unwrap().is_empty());
}

#[tokio::test]
async fn backend_close_reason_is_forwarded() {
    let fake = Fake::new();
    let (app, initial) = registered(&fake);
    let mut watcher = app.terminal_watch(&initial.invocation_id).unwrap();
    assert!(matches!(
        next(&mut watcher).await,
        Some(TerminalFrame::Snapshot { .. })
    ));
    fake.push(PaneEvent::Resized);
    assert!(matches!(
        next(&mut watcher).await,
        Some(TerminalFrame::Snapshot { .. })
    ));
    fake.push(PaneEvent::Closed(EndReason::MultiplexerStopped));
    assert_eq!(
        next(&mut watcher).await,
        Some(TerminalFrame::Ended {
            reason: EndReason::MultiplexerStopped
        })
    );
}

#[tokio::test]
async fn public_view_carries_terminal_descriptor() {
    let fake = Fake::new();
    let app = app_with(&fake);
    let (_, _, mut updates) = app.subscribe().unwrap();
    let claude = interactive("claude");
    app.register(claude.clone(), "credential").unwrap();
    let registered = updates.recv().await.unwrap();
    assert_eq!(
        registered.view.terminal,
        Some(TerminalDescriptor {
            quick_pick: QuickPick::Digits
        })
    );
    let json = serde_json::to_string(&registered.view).unwrap();
    assert!(!json.contains("tmux") && !json.contains("%1"));

    for (provider, quick_pick) in [
        ("company-claude", QuickPick::Digits),
        ("codex", QuickPick::None),
    ] {
        app.register(interactive(provider), "credential").unwrap();
        assert_eq!(
            updates.recv().await.unwrap().view.terminal,
            Some(TerminalDescriptor { quick_pick })
        );
    }

    app.lifecycle_exit(&claude.invocation_id, "credential", Some(0), None)
        .unwrap();
    let stopped = updates.recv().await.unwrap();
    assert!(stopped.view.terminal.is_none());
    assert!(stopped.changed.contains(&PublicField::Terminal));

    let headless = headless_app(Storage::memory().unwrap());
    headless.register(snapshot(), "credential").unwrap();
    assert!(headless.status().unwrap().1[0].terminal.is_none());
}

#[tokio::test]
async fn finished_turn_keeps_terminal_open() {
    let fake = Fake::new();
    let (app, initial) = registered(&fake);
    let id = &initial.invocation_id;
    let mut watcher = app.terminal_watch(id).unwrap();
    assert!(matches!(
        next(&mut watcher).await,
        Some(TerminalFrame::Snapshot { .. })
    ));
    for (event_id, kind) in [("turn", EventKind::NewTurn), ("stop", EventKind::Completed)] {
        app.ingest_hook(
            initial.provider.clone(),
            id.clone(),
            "credential".into(),
            event(&initial, event_id, kind),
            None,
            None,
        )
        .unwrap();
    }
    let view = app.status().unwrap().1.remove(0);
    assert_eq!(view.status, PublicStatus::Stopped);
    assert!(view.terminal.is_some());

    app.reconcile(|_, _| true, 30).unwrap();
    fake.push(PaneEvent::Output(b"still here".to_vec()));
    assert!(matches!(
        next(&mut watcher).await,
        Some(TerminalFrame::Output { .. })
    ));
    let mut second = app.terminal_watch(id).unwrap();
    assert!(matches!(
        next(&mut second).await,
        Some(TerminalFrame::Snapshot { .. })
    ));
    app.terminal_input(id, &keys("1")).unwrap();
}

mod relay {
    use super::*;
    use crate::control::{Bridge, ControlEnabled};
    use sessiontap_core::protocol::{RelayMessage, RelayRequest};

    struct Harness {
        fake: Arc<Fake>,
        app: App,
        initial: InvocationSnapshot,
        enabled: Arc<AtomicBool>,
        bridge: Bridge,
        out: mpsc::Receiver<RelayMessage>,
    }

    fn harness() -> Harness {
        let fake = Fake::new();
        let (app, initial) = registered(&fake);
        let enabled = Arc::new(AtomicBool::new(true));
        let check: ControlEnabled = {
            let enabled = enabled.clone();
            Arc::new(move || enabled.load(Ordering::SeqCst))
        };
        let (sender, out) = mpsc::channel(64);
        Harness {
            bridge: Bridge::new(app.clone(), check, sender),
            fake,
            app,
            initial,
            enabled,
            out,
        }
    }

    async fn message(out: &mut mpsc::Receiver<RelayMessage>) -> RelayMessage {
        tokio::time::timeout(Duration::from_secs(5), out.recv())
            .await
            .expect("relay message")
            .expect("open")
    }

    /// Next frame of `stream`, skipping other streams and input frames.
    async fn frame_of(out: &mut mpsc::Receiver<RelayMessage>, stream: u64) -> TerminalFrame {
        loop {
            if let RelayMessage::Frame { stream: s, frame } = message(out).await
                && s == stream
                && !matches!(frame, TerminalFrame::Input(_))
            {
                return frame;
            }
        }
    }

    impl Harness {
        async fn open(&mut self, req: u64, stream: u64) {
            assert!(
                self.bridge
                    .handle(RelayRequest::Open {
                        req,
                        stream,
                        invocation_id: self.initial.invocation_id.to_string(),
                    })
                    .await
            );
            assert_eq!(
                message(&mut self.out).await,
                RelayMessage::Opened { req, stream }
            );
            assert!(matches!(
                frame_of(&mut self.out, stream).await,
                TerminalFrame::Snapshot { .. }
            ));
        }

        async fn input(&mut self, req: u64, stream: u64) -> (bool, Option<String>) {
            let open = self
                .bridge
                .handle(RelayRequest::Input {
                    req,
                    stream,
                    input: keys("1"),
                })
                .await;
            loop {
                if let RelayMessage::InputResult {
                    req: answered,
                    code,
                } = message(&mut self.out).await
                {
                    assert_eq!(answered, req);
                    return (open, code);
                }
            }
        }
    }

    #[tokio::test]
    async fn two_streams_share_one_pane_stream_and_resync_alone() {
        let mut h = harness();
        h.open(1, 10).await;
        h.open(2, 20).await;
        assert_eq!(h.fake.opened.load(Ordering::SeqCst), 1);
        h.fake.push(PaneEvent::Output(b"both".to_vec()));
        let mut seen = Vec::new();
        while seen.len() < 2 {
            if let RelayMessage::Frame {
                stream,
                frame: TerminalFrame::Output { data, .. },
            } = message(&mut h.out).await
            {
                assert_eq!(data, b"both");
                seen.push(stream);
            }
        }
        seen.sort_unstable();
        assert_eq!(seen, vec![10, 20]);
        // a lagging relay stream asks for a snapshot; the other keeps output
        let before = h.fake.snapshots.load(Ordering::SeqCst);
        assert!(h.bridge.handle(RelayRequest::Resync { stream: 20 }).await);
        assert!(matches!(
            frame_of(&mut h.out, 20).await,
            TerminalFrame::Snapshot { .. }
        ));
        assert!(h.fake.snapshots.load(Ordering::SeqCst) > before);
        h.fake.push(PaneEvent::Output(b"more".to_vec()));
        assert!(matches!(
            frame_of(&mut h.out, 10).await,
            TerminalFrame::Snapshot { .. } | TerminalFrame::Output { .. }
        ));
        assert!(h.bridge.handle(RelayRequest::Close { stream: 10 }).await);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!h.fake.control_dropped.load(Ordering::SeqCst));
        assert!(h.bridge.handle(RelayRequest::Close { stream: 20 }).await);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            h.fake.control_dropped.load(Ordering::SeqCst),
            "the last relay stream releases the pane stream"
        );
    }

    #[tokio::test]
    async fn input_follows_the_pane_guard() {
        let mut h = harness();
        h.open(1, 10).await;
        assert_eq!(h.input(2, 10).await, (true, None));
        assert_eq!(h.fake.sent.lock().unwrap().as_slice(), ["1"]);
        h.fake.set(|state| state.foreground_pgid = Some(1));
        assert_eq!(
            h.input(3, 10).await,
            (true, Some(error_code::NOT_FOREGROUND.into()))
        );
        assert_eq!(h.fake.sent.lock().unwrap().len(), 1, "nothing written");
    }

    #[tokio::test]
    async fn control_turned_off_refuses_and_ends_streams() {
        let mut h = harness();
        h.open(1, 10).await;
        h.enabled.store(false, Ordering::SeqCst);
        assert_eq!(
            h.input(2, 10).await,
            (false, Some(error_code::SOURCE_DISALLOWS_CONTROL.into()))
        );
        assert_eq!(
            frame_of(&mut h.out, 10).await,
            TerminalFrame::Ended {
                reason: EndReason::SourceDisallowsControl
            }
        );
        assert!(h.fake.sent.lock().unwrap().is_empty());
        let refused = h
            .bridge
            .handle(RelayRequest::Open {
                req: 3,
                stream: 30,
                invocation_id: h.initial.invocation_id.to_string(),
            })
            .await;
        assert!(!refused);
        assert!(matches!(
            message(&mut h.out).await,
            RelayMessage::Error { req: 3, code, .. } if code == error_code::SOURCE_DISALLOWS_CONTROL
        ));
    }

    #[tokio::test]
    async fn agent_exit_is_forwarded_unchanged() {
        let mut h = harness();
        h.open(1, 10).await;
        h.app
            .lifecycle_exit(&h.initial.invocation_id, "credential", Some(0), None)
            .unwrap();
        assert_eq!(
            frame_of(&mut h.out, 10).await,
            TerminalFrame::Ended {
                reason: EndReason::AgentExited
            }
        );
        assert_eq!(
            h.input(2, 10).await,
            (true, Some(error_code::TERMINAL_ENDED.into()))
        );
    }

    #[tokio::test]
    async fn unknown_invocation_is_not_found() {
        let mut h = harness();
        assert!(
            h.bridge
                .handle(RelayRequest::Open {
                    req: 1,
                    stream: 10,
                    invocation_id: "not-a-uuid".into(),
                })
                .await
        );
        assert!(matches!(
            message(&mut h.out).await,
            RelayMessage::Error { code, .. } if code == error_code::NOT_FOUND
        ));
    }
}
