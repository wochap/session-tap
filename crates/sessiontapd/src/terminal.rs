//! Live terminal sessions: one backend stream per watched invocation,
//! fanned out to every watcher, plus the best-effort input guard.

use sessiontap_core::{
    domain::InvocationId,
    terminal::{EndReason, InputState, InputUnavailable, TerminalFrame},
};
use sessiontap_infra::{
    multiplexer::{PaneEvent, PaneState, PaneStream, PaneStreamControl},
    process::{process_alive, process_start_identity},
};
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    sync::{Notify, broadcast},
    task::JoinHandle,
};

/// Frames buffered per session before a slow watcher lags and resyncs.
const FRAME_BUFFER: usize = 256;
/// Interval of the foreground poll while a pane is watched; foreground
/// changes have no multiplexer event.
pub const GUARD_POLL: Duration = Duration::from_millis(250);

/// Typed failure answered to watch and input requests with its code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalError {
    pub code: &'static str,
    pub message: String,
}

impl TerminalError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for TerminalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for TerminalError {}

/// Tracked agent identity the guard checks the pane's foreground against.
#[derive(Debug, Clone)]
pub struct AgentProcess {
    pub child_pid: u32,
    pub start_identity: Option<String>,
}

impl AgentProcess {
    /// Input is allowed only while the agent's own group (the wrapper made
    /// the child a group leader) is in the foreground, the child still has
    /// its recorded start identity, and the pane is not in a mode.
    #[must_use]
    pub fn guard(&self, state: PaneState) -> Option<InputUnavailable> {
        let same_process = || {
            self.start_identity.as_deref().is_none_or(|expected| {
                process_start_identity(self.child_pid).as_deref() == Some(expected)
            })
        };
        if state.foreground_pgid != Some(self.child_pid) || !same_process() {
            return Some(InputUnavailable::NotForeground);
        }
        state.in_mode.then_some(InputUnavailable::PaneInMode)
    }

    fn alive(&self) -> bool {
        process_alive(self.child_pid, self.start_identity.as_deref())
    }
}

/// Reads the pane's guard inputs; runs on a blocking thread.
pub type PaneStateFn = Arc<dyn Fn() -> anyhow::Result<PaneState> + Send + Sync>;

struct SessionState {
    seq: u64,
    input: InputState,
    foreground: Option<InputUnavailable>,
    in_mode: bool,
    ended: bool,
}

pub struct TerminalSession {
    frames: broadcast::Sender<TerminalFrame>,
    control: Arc<dyn PaneStreamControl>,
    state: Mutex<SessionState>,
    poll_now: Notify,
    watchers: Mutex<usize>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
}

impl TerminalSession {
    /// Assigns the next sequence number and broadcasts, unless ended.
    fn publish(&self, frame: impl FnOnce(u64, InputState) -> TerminalFrame) {
        let mut state = self.state.lock().expect("terminal state");
        if state.ended {
            return;
        }
        let frame = frame(state.seq, state.input);
        state.seq += 1;
        let _ = self.frames.send(frame);
    }

    /// Recomputes input availability and pushes a frame when it changed.
    fn update_input(&self, change: impl FnOnce(&mut SessionState)) {
        let mut state = self.state.lock().expect("terminal state");
        if state.ended {
            return;
        }
        change(&mut state);
        let next = InputState::from_guard(
            state
                .foreground
                .or(state.in_mode.then_some(InputUnavailable::PaneInMode)),
        );
        if next != state.input {
            state.input = next;
            let _ = self.frames.send(TerminalFrame::Input(next));
        }
    }

    fn end(&self, reason: EndReason) -> bool {
        let mut state = self.state.lock().expect("terminal state");
        if state.ended {
            return false;
        }
        state.ended = true;
        let _ = self.frames.send(TerminalFrame::Ended { reason });
        true
    }

    fn abort(&self) {
        for task in self.tasks.lock().expect("terminal tasks").drain(..) {
            task.abort();
        }
    }
}

/// Every live terminal session of the daemon.
#[derive(Default)]
pub struct Terminals {
    sessions: Mutex<HashMap<InvocationId, Arc<TerminalSession>>>,
    ended: Mutex<HashSet<InvocationId>>,
}

impl Terminals {
    #[must_use]
    pub fn has_ended(&self, invocation: &InvocationId) -> bool {
        self.ended
            .lock()
            .expect("terminal ended")
            .contains(invocation)
    }

    /// Joins the invocation's session, opening it with `open` when none is
    /// live. The watcher sees a fresh snapshot first.
    pub fn watch(
        self: &Arc<Self>,
        invocation: &InvocationId,
        agent: AgentProcess,
        pane_state: PaneStateFn,
        open: impl FnOnce() -> anyhow::Result<PaneStream>,
    ) -> anyhow::Result<Watcher> {
        let mut sessions = self.sessions.lock().expect("terminal sessions");
        if let Some(session) = sessions.get(invocation) {
            let receiver = session.frames.subscribe();
            *session.watchers.lock().expect("terminal watchers") += 1;
            session.control.request_snapshot()?;
            return Ok(self.watcher(invocation, session.clone(), receiver));
        }
        let foreground = pane_state().map_or(Some(InputUnavailable::NotForeground), |state| {
            agent.guard(PaneState {
                in_mode: false,
                ..state
            })
        });
        let PaneStream {
            mut events,
            control,
        } = open()?;
        let (frames, receiver) = broadcast::channel(FRAME_BUFFER);
        let session = Arc::new(TerminalSession {
            frames,
            control,
            state: Mutex::new(SessionState {
                seq: 0,
                input: InputState::from_guard(foreground),
                foreground,
                in_mode: false,
                ended: false,
            }),
            poll_now: Notify::new(),
            watchers: Mutex::new(1),
            tasks: Mutex::new(Vec::new()),
        });
        let reader = {
            let terminals = Arc::downgrade(self);
            let session = session.clone();
            let invocation = invocation.clone();
            tokio::spawn(async move {
                while let Some(event) = events.recv().await {
                    match event {
                        PaneEvent::Snapshot(snapshot) => {
                            session.update_input(|state| state.in_mode = snapshot.in_mode);
                            session.publish(|seq, input| TerminalFrame::Snapshot {
                                seq,
                                cols: snapshot.cols,
                                rows: snapshot.rows,
                                cursor: snapshot.cursor,
                                alternate_screen: snapshot.alternate_screen,
                                data: snapshot.data,
                                input,
                            });
                        }
                        PaneEvent::Output(data) => {
                            session.publish(|seq, _| TerminalFrame::Output { seq, data });
                        }
                        PaneEvent::Resync | PaneEvent::Resized => {
                            let _ = session.control.request_snapshot();
                        }
                        PaneEvent::ModeChanged(in_mode) => {
                            session.update_input(|state| state.in_mode = in_mode);
                        }
                        PaneEvent::Closed(reason) => {
                            if let Some(terminals) = terminals.upgrade() {
                                terminals.end(&invocation, reason);
                            }
                            return;
                        }
                    }
                }
            })
        };
        let poller = {
            let terminals = Arc::downgrade(self);
            let session = session.clone();
            let invocation = invocation.clone();
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        () = tokio::time::sleep(GUARD_POLL) => {}
                        () = session.poll_now.notified() => {}
                    }
                    let agent = agent.clone();
                    let pane_state = pane_state.clone();
                    let polled = tokio::task::spawn_blocking(move || {
                        if !agent.alive() {
                            return None;
                        }
                        Some(
                            pane_state().map_or(Some(InputUnavailable::NotForeground), |state| {
                                agent.guard(PaneState {
                                    in_mode: false,
                                    ..state
                                })
                            }),
                        )
                    })
                    .await;
                    match polled {
                        Ok(Some(foreground)) => {
                            session.update_input(|state| state.foreground = foreground);
                        }
                        Ok(None) => {
                            if let Some(terminals) = terminals.upgrade() {
                                terminals.end(&invocation, EndReason::AgentExited);
                            }
                            return;
                        }
                        Err(_) => return,
                    }
                }
            })
        };
        session
            .tasks
            .lock()
            .expect("terminal tasks")
            .extend([reader, poller]);
        sessions.insert(invocation.clone(), session.clone());
        Ok(self.watcher(invocation, session, receiver))
    }

    fn watcher(
        self: &Arc<Self>,
        invocation: &InvocationId,
        session: Arc<TerminalSession>,
        receiver: broadcast::Receiver<TerminalFrame>,
    ) -> Watcher {
        Watcher {
            terminals: self.clone(),
            invocation: invocation.clone(),
            session,
            receiver,
            synced: false,
            done: false,
        }
    }

    /// Ends the invocation's session with `reason`; later output is dropped
    /// and later input answers `terminal_ended`.
    pub fn end(&self, invocation: &InvocationId, reason: EndReason) {
        let session = self
            .sessions
            .lock()
            .expect("terminal sessions")
            .remove(invocation);
        if let Some(session) = session {
            if session.end(reason) {
                self.ended
                    .lock()
                    .expect("terminal ended")
                    .insert(invocation.clone());
            }
            session.abort();
        }
    }

    /// Ends sessions whose invocation `is_stopped` reports as stopped.
    pub fn end_stopped(&self, is_stopped: impl Fn(&InvocationId) -> bool) {
        let ids: Vec<_> = self
            .sessions
            .lock()
            .expect("terminal sessions")
            .keys()
            .filter(|id| is_stopped(id))
            .cloned()
            .collect();
        for id in ids {
            self.end(&id, EndReason::AgentExited);
        }
    }

    /// Wakes the session's guard poll, e.g. right after input was refused.
    pub fn poll_guard(&self, invocation: &InvocationId) {
        if let Some(session) = self
            .sessions
            .lock()
            .expect("terminal sessions")
            .get(invocation)
        {
            session.poll_now.notify_one();
        }
    }

    #[must_use]
    pub fn is_open(&self, invocation: &InvocationId) -> bool {
        self.sessions
            .lock()
            .expect("terminal sessions")
            .contains_key(invocation)
    }

    fn leave(&self, invocation: &InvocationId, session: &Arc<TerminalSession>) {
        let mut sessions = self.sessions.lock().expect("terminal sessions");
        let mut watchers = session.watchers.lock().expect("terminal watchers");
        *watchers -= 1;
        if *watchers == 0
            && sessions
                .get(invocation)
                .is_some_and(|current| Arc::ptr_eq(current, session))
        {
            sessions.remove(invocation);
            session.abort();
        }
    }
}

/// One watcher's view of a session. Dropping it leaves the session; the
/// last watcher closes the backend connection.
pub struct Watcher {
    terminals: Arc<Terminals>,
    invocation: InvocationId,
    session: Arc<TerminalSession>,
    receiver: broadcast::Receiver<TerminalFrame>,
    synced: bool,
    done: bool,
}

impl Watcher {
    /// Drops this watcher's pending output and asks for a fresh snapshot,
    /// e.g. when the relay downstream fell behind.
    pub fn resync(&mut self) {
        self.synced = false;
        let _ = self.session.control.request_snapshot();
    }

    /// Next frame for this watcher: a snapshot first and after any lag,
    /// then output and input changes, ending with `ended`.
    pub async fn next(&mut self) -> Option<TerminalFrame> {
        if self.done {
            return None;
        }
        loop {
            match self.receiver.recv().await {
                Ok(frame @ TerminalFrame::Ended { .. }) => {
                    self.done = true;
                    return Some(frame);
                }
                Ok(frame @ TerminalFrame::Snapshot { .. }) => {
                    self.synced = true;
                    return Some(frame);
                }
                Ok(frame) if self.synced => return Some(frame),
                Ok(_) => {}
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    self.synced = false;
                    let _ = self.session.control.request_snapshot();
                }
                Err(broadcast::error::RecvError::Closed) => {
                    self.done = true;
                    return None;
                }
            }
        }
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.terminals.leave(&self.invocation, &self.session);
    }
}
