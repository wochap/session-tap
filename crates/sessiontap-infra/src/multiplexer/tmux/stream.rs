//! Output-only tmux control-mode client backing a live pane stream.
//!
//! Only `refresh-client`, `capture-pane`, and `display` are ever written to
//! the client's stdin, so it can never send keys to a pane. Replies and
//! `%output` share one ordered queue per control client, which makes a
//! snapshot exact against the output that follows it.

use super::TmuxAdapter;
use crate::multiplexer::{
    PaneEvent, PaneSnapshot, PaneStream, PaneStreamControl,
    control::{Message, Notification, Parser},
};
use anyhow::{Context, Result};
use sessiontap_core::{
    domain::MultiplexerMetadata,
    terminal::{Cursor, EndReason, SNAPSHOT_SCROLLBACK_LINES},
};
use std::{
    collections::VecDeque,
    io::{BufRead, BufReader, Write},
    process::{Child, ChildStdin, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::sync::mpsc;

/// Seconds of backlog after which tmux pauses the pane for this client
/// instead of blocking the pane's program.
const PAUSE_AFTER_SECONDS: u32 = 5;

const SNAPSHOT_FORMAT: &str = "#{cursor_x} #{cursor_y} #{cursor_flag} #{alternate_on} #{pane_width} #{pane_height} #{pane_in_mode}";
const SIZE_FORMAT: &str = "#{pane_width} #{pane_height}";

/// What a pending command reply means, in write order.
#[derive(Debug, Clone, Copy)]
enum Pending {
    Ignore,
    Capture,
    Snapshot,
    Mode,
    Size,
}

struct Writer {
    stdin: ChildStdin,
    pending: VecDeque<Pending>,
}

impl Writer {
    /// Records the expected replies before writing, so the reader can never
    /// see a reply it has no entry for.
    fn send(&mut self, line: &str, replies: &[Pending]) -> std::io::Result<()> {
        self.pending.extend(replies);
        self.stdin.write_all(line.as_bytes())?;
        self.stdin.write_all(b"\n")?;
        self.stdin.flush()
    }
}

struct Target {
    socket: String,
    session: String,
    window: Option<String>,
    pane: String,
}

impl Target {
    fn snapshot_command(&self) -> String {
        format!(
            "capture-pane -p -e -J -S -{SNAPSHOT_SCROLLBACK_LINES} -t {pane} ; display -p -t {pane} '{SNAPSHOT_FORMAT}'",
            pane = quote(&self.pane)
        )
    }
}

/// Single-quotes an ID for a control-client command line; tmux IDs never
/// contain quotes.
fn quote(id: &str) -> String {
    format!("'{}'", id.replace('\'', ""))
}

struct TmuxStream {
    child: Mutex<Child>,
    writer: Arc<Mutex<Writer>>,
    snapshot_command: String,
    closed: Arc<AtomicBool>,
}

impl PaneStreamControl for TmuxStream {
    fn request_snapshot(&self) -> Result<()> {
        let mut writer = self.writer.lock().expect("tmux writer lock");
        writer.send(
            &self.snapshot_command,
            &[Pending::Capture, Pending::Snapshot],
        )?;
        Ok(())
    }
}

impl Drop for TmuxStream {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::SeqCst);
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

pub(super) fn open(current: &MultiplexerMetadata) -> Result<PaneStream> {
    let target = Target {
        socket: current.socket.clone(),
        session: current
            .session_id
            .clone()
            .context("tmux session ID unavailable")?,
        window: current.window_id.clone(),
        pane: current.pane_id.clone(),
    };
    let mut child = Command::new("tmux")
        .args([
            "-S",
            &target.socket,
            "-C",
            "attach-session",
            "-f",
            "ignore-size",
        ])
        .args(["-t", &target.session])
        .env_remove("TMUX")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("failed to start tmux control client")?;
    let stdin = child.stdin.take().context("tmux stdin unavailable")?;
    let stdout = child.stdout.take().context("tmux stdout unavailable")?;
    let writer = Arc::new(Mutex::new(Writer {
        stdin,
        pending: VecDeque::new(),
    }));
    {
        let mut writer = writer.lock().expect("tmux writer lock");
        writer.send(
            &format!("refresh-client -f ignore-size,pause-after={PAUSE_AFTER_SECONDS}"),
            &[Pending::Ignore],
        )?;
        writer.send(
            &target.snapshot_command(),
            &[Pending::Capture, Pending::Snapshot],
        )?;
    }
    let (events, receiver) = mpsc::unbounded_channel();
    let closed = Arc::new(AtomicBool::new(false));
    let control = Arc::new(TmuxStream {
        child: Mutex::new(child),
        writer: writer.clone(),
        snapshot_command: target.snapshot_command(),
        closed: closed.clone(),
    });
    std::thread::Builder::new()
        .name(format!("tmux-stream-{}", target.pane))
        .spawn(move || {
            let mut reader = Reader {
                target,
                writer,
                events,
                captured: None,
                size: None,
                snapshot_seen: false,
            };
            let reason = reader.run(BufReader::new(stdout));
            if !closed.load(Ordering::SeqCst) {
                let reason = reason.unwrap_or_else(|| reader.eof_reason());
                let _ = reader.events.send(PaneEvent::Closed(reason));
            }
        })?;
    Ok(PaneStream {
        events: receiver,
        control,
    })
}

struct Reader {
    target: Target,
    writer: Arc<Mutex<Writer>>,
    events: mpsc::UnboundedSender<PaneEvent>,
    captured: Option<Vec<Vec<u8>>>,
    size: Option<(u16, u16)>,
    snapshot_seen: bool,
}

impl Reader {
    /// Reads until EOF or a terminal condition; `Some` names a known end.
    fn run(&mut self, stdout: impl BufRead) -> Option<EndReason> {
        let mut parser = Parser::default();
        for line in stdout.split(b'\n') {
            let Ok(mut line) = line else { break };
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            let Some(message) = parser.feed(&line) else {
                continue;
            };
            match self.handle(message) {
                Ok(None) => {}
                Ok(Some(reason)) => return Some(reason),
                Err(_) => break,
            }
        }
        None
    }

    fn emit(&self, event: PaneEvent) -> Result<()> {
        self.events
            .send(event)
            .map_err(|_| anyhow::anyhow!("stream receiver dropped"))
    }

    fn send(&self, line: &str, reply: Pending) -> Result<()> {
        self.writer
            .lock()
            .expect("tmux writer lock")
            .send(line, &[reply])?;
        Ok(())
    }

    fn ours_window(&self, window: &str) -> bool {
        self.target.window.as_deref() == Some(window)
    }

    fn handle(&mut self, message: Message) -> Result<Option<EndReason>> {
        let pane = quote(&self.target.pane);
        match message {
            Message::Notification(notification) => match notification {
                // Output before the first snapshot is part of it.
                Notification::Output { pane: from, data }
                    if from == self.target.pane && self.snapshot_seen =>
                {
                    self.emit(PaneEvent::Output(data))?;
                }
                Notification::Pause { pane: from } if from == self.target.pane => {
                    self.send(
                        &format!("refresh-client -A '{}:continue'", self.target.pane),
                        Pending::Ignore,
                    )?;
                    self.emit(PaneEvent::Resync)?;
                }
                Notification::PaneModeChanged { pane: from } if from == self.target.pane => {
                    self.send(
                        &format!("display -p -t {pane} '#{{pane_in_mode}}'"),
                        Pending::Mode,
                    )?;
                }
                Notification::LayoutChange { window } if self.ours_window(&window) => {
                    self.send(
                        &format!("display -p -t {pane} '{SIZE_FORMAT}'"),
                        Pending::Size,
                    )?;
                }
                Notification::SessionChanged { session } if session != self.target.session => {
                    return Ok(Some(EndReason::SessionClosed));
                }
                Notification::WindowClose { window }
                | Notification::UnlinkedWindowClose { window }
                    if self.ours_window(&window) =>
                {
                    return Ok(Some(EndReason::PaneClosed));
                }
                _ => {}
            },
            Message::Reply { client: false, .. } => {}
            Message::Reply {
                client: true,
                ok,
                lines,
            } => {
                let pending = self
                    .writer
                    .lock()
                    .expect("tmux writer lock")
                    .pending
                    .pop_front()
                    .unwrap_or(Pending::Ignore);
                let first = lines
                    .first()
                    .map(|line| String::from_utf8_lossy(line).into_owned())
                    .unwrap_or_default();
                match pending {
                    Pending::Ignore => {}
                    Pending::Capture if ok => self.captured = Some(lines),
                    Pending::Capture | Pending::Snapshot | Pending::Size if !ok => {
                        return Ok(Some(EndReason::PaneClosed));
                    }
                    Pending::Capture => {}
                    Pending::Snapshot => {
                        let snapshot =
                            build_snapshot(&first, self.captured.take().unwrap_or_default())
                                .context("unexpected tmux snapshot reply")?;
                        self.size = Some((snapshot.cols, snapshot.rows));
                        self.snapshot_seen = true;
                        self.emit(PaneEvent::Snapshot(snapshot))?;
                    }
                    Pending::Mode => {
                        if ok {
                            self.emit(PaneEvent::ModeChanged(first.trim() != "0"))?;
                        }
                    }
                    Pending::Size => {
                        let mut parts = first.split_whitespace().map(str::parse::<u16>);
                        if let (Some(Ok(cols)), Some(Ok(rows))) = (parts.next(), parts.next())
                            && self.size != Some((cols, rows))
                        {
                            self.size = Some((cols, rows));
                            self.emit(PaneEvent::Resized)?;
                        }
                    }
                }
            }
        }
        Ok(None)
    }

    /// Why the client's output ended without a closing notification.
    fn eof_reason(&self) -> EndReason {
        if TmuxAdapter::query(&self.target.socket, &["display-message", "-p", "#{pid}"]).is_err() {
            return EndReason::MultiplexerStopped;
        }
        if TmuxAdapter::query(
            &self.target.socket,
            &[
                "display-message",
                "-p",
                "-t",
                &self.target.pane,
                "#{pane_id}",
            ],
        )
        .is_err()
        {
            return EndReason::PaneClosed;
        }
        EndReason::SessionClosed
    }
}

fn build_snapshot(info: &str, lines: Vec<Vec<u8>>) -> Option<PaneSnapshot> {
    let fields: Vec<_> = info.split_whitespace().collect();
    let [x, y, visible, alternate, cols, rows, in_mode] = fields.as_slice() else {
        return None;
    };
    Some(PaneSnapshot {
        cols: cols.parse().ok()?,
        rows: rows.parse().ok()?,
        cursor: Cursor {
            x: x.parse().ok()?,
            y: y.parse().ok()?,
            visible: *visible != "0",
        },
        alternate_screen: *alternate != "0",
        in_mode: *in_mode != "0",
        data: lines.join(&b"\r\n"[..]),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_reply_parses() {
        let snapshot = build_snapshot(
            "3 1 1 0 160 40 0",
            vec![b"a".to_vec(), b"\x1b[1mb".to_vec()],
        )
        .unwrap();
        assert_eq!((snapshot.cols, snapshot.rows), (160, 40));
        assert_eq!(
            snapshot.cursor,
            Cursor {
                x: 3,
                y: 1,
                visible: true
            }
        );
        assert!(!snapshot.alternate_screen && !snapshot.in_mode);
        assert_eq!(snapshot.data, b"a\r\n\x1b[1mb");
        assert!(build_snapshot("garbage", vec![]).is_none());
    }

    #[test]
    fn ids_are_single_quoted() {
        assert_eq!(quote("%3"), "'%3'");
        assert_eq!(quote("$1"), "'$1'");
    }
}
