mod stream;

use super::{MultiplexerAdapter, PaneState, PaneStream};
use crate::process::{parent_pid, terminal_foreground_pgid};
use anyhow::{Context, Result, bail};
use sessiontap_core::{
    domain::{MultiplexerBackend, MultiplexerMetadata},
    terminal::{Key, NamedKey},
};
use std::{
    env,
    io::Write,
    path::Path,
    process::{Command, Stdio},
};

#[derive(Debug, Default, Clone, Copy)]
pub struct TmuxAdapter;

impl TmuxAdapter {
    fn query(socket: &str, args: &[&str]) -> Result<String> {
        let output = Command::new("tmux")
            .args(["-S", socket])
            .args(args)
            .output()?;
        if !output.status.success() {
            bail!("tmux query failed");
        }
        Ok(String::from_utf8(output.stdout)?.trim_end().to_owned())
    }

    fn run(socket: &str, args: &[&str]) -> Result<()> {
        let output = Command::new("tmux")
            .args(["-S", socket])
            .args(args)
            .stdin(Stdio::null())
            .output()?;
        if !output.status.success() {
            bail!(
                "tmux {} failed: {}",
                args.first().copied().unwrap_or_default(),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(())
    }

    fn current(&self, socket: &str, pane: &str) -> Result<MultiplexerMetadata> {
        let format = "#{pid}\t#{session_id}\t#{session_name}\t#{window_id}\t#{window_index}\t#{pane_id}\t#{pane_tty}\t#{pane_pid}";
        let raw = Self::query(socket, &["display-message", "-p", "-t", pane, format])?;
        let fields: Vec<_> = raw.split('\t').collect();
        if fields.len() != 8 {
            bail!("unexpected tmux metadata");
        }
        Ok(MultiplexerMetadata {
            backend: MultiplexerBackend::Tmux,
            socket: socket.into(),
            server_pid: fields[0].parse().ok(),
            session_id: Some(fields[1].into()),
            session_name: Some(fields[2].into()),
            window_id: Some(fields[3].into()),
            window_index: fields[4].parse().ok(),
            pane_id: fields[5].into(),
            pane_tty: Some(fields[6].into()),
            pane_pid: fields[7].parse().ok(),
        })
    }

    fn validate(&self, expected: &MultiplexerMetadata, process_pid: u32) -> Result<()> {
        let current = self.current(&expected.socket, &expected.pane_id)?;
        if current.server_pid != expected.server_pid
            || current.pane_id != expected.pane_id
            || current.pane_tty != expected.pane_tty
            || current.pane_pid != expected.pane_pid
        {
            bail!("tmux server or pane identity changed");
        }
        let pane_pid = current.pane_pid.context("tmux pane PID unavailable")?;
        if !is_descendant(process_pid, pane_pid) {
            bail!("tracked process no longer belongs to the tmux pane");
        }
        Ok(())
    }
}

impl MultiplexerAdapter for TmuxAdapter {
    fn inspect(&self) -> Result<Option<MultiplexerMetadata>> {
        let Some(tmux) = env::var_os("TMUX") else {
            return Ok(None);
        };
        let pane = env::var("TMUX_PANE").context("TMUX_PANE missing")?;
        let socket = tmux
            .to_string_lossy()
            .split(',')
            .next()
            .filter(|s| Path::new(s).is_absolute())
            .context("invalid TMUX socket")?
            .to_owned();
        Ok(Some(self.current(&socket, &pane)?))
    }

    fn capture(&self, expected: &MultiplexerMetadata, process_pid: u32) -> Result<String> {
        self.validate(expected, process_pid)?;
        Self::query(
            &expected.socket,
            &["capture-pane", "-p", "-J", "-t", &expected.pane_id],
        )
    }

    fn open_stream(&self, expected: &MultiplexerMetadata, process_pid: u32) -> Result<PaneStream> {
        require_version(&expected.socket)?;
        self.validate(expected, process_pid)?;
        let current = self.current(&expected.socket, &expected.pane_id)?;
        stream::open(&current)
    }

    fn send_keys(
        &self,
        expected: &MultiplexerMetadata,
        process_pid: u32,
        keys: &[Key],
    ) -> Result<()> {
        self.validate(expected, process_pid)?;
        for key in keys {
            let mut args = vec!["send-keys"];
            let literal;
            match key {
                Key::Named(named) => args.extend(["-t", &expected.pane_id, key_name(*named)]),
                Key::Char(c) => {
                    // A lone `;` separates tmux commands; `\;` is the escape.
                    literal = if *c == ';' {
                        "\\;".to_owned()
                    } else {
                        c.to_string()
                    };
                    args.extend(["-l", "-t", &expected.pane_id, "--", &literal]);
                }
            }
            Self::run(&expected.socket, &args)?;
        }
        Ok(())
    }

    fn paste(
        &self,
        expected: &MultiplexerMetadata,
        process_pid: u32,
        text: &str,
        enter: bool,
    ) -> Result<()> {
        self.validate(expected, process_pid)?;
        let name = format!("sessiontap-{}", std::process::id());
        let mut child = Command::new("tmux")
            .args(["-S", &expected.socket, "load-buffer", "-b", &name, "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        child
            .stdin
            .take()
            .context("tmux stdin unavailable")?
            .write_all(text.as_bytes())?;
        if !child.wait()?.success() {
            bail!("tmux load-buffer failed");
        }
        Self::run(
            &expected.socket,
            &[
                "paste-buffer",
                "-p",
                "-d",
                "-b",
                &name,
                "-t",
                &expected.pane_id,
            ],
        )?;
        if enter {
            Self::run(
                &expected.socket,
                &["send-keys", "-t", &expected.pane_id, "Enter"],
            )?;
        }
        Ok(())
    }

    fn pane_state(&self, expected: &MultiplexerMetadata, process_pid: u32) -> Result<PaneState> {
        self.validate(expected, process_pid)?;
        let pane_pid = expected.pane_pid.context("tmux pane PID unavailable")?;
        let in_mode = Self::query(
            &expected.socket,
            &[
                "display-message",
                "-p",
                "-t",
                &expected.pane_id,
                "#{pane_in_mode}",
            ],
        )?;
        Ok(PaneState {
            foreground_pgid: terminal_foreground_pgid(pane_pid),
            in_mode: in_mode != "0",
        })
    }
}

/// tmux key names; tmux encodes them for the pane's current key modes.
const fn key_name(key: NamedKey) -> &'static str {
    match key {
        NamedKey::Up => "Up",
        NamedKey::Down => "Down",
        NamedKey::Left => "Left",
        NamedKey::Right => "Right",
        NamedKey::Escape => "Escape",
        NamedKey::Tab => "Tab",
        NamedKey::BackTab => "BTab",
        NamedKey::Enter => "Enter",
        NamedKey::Space => "Space",
        NamedKey::Backspace => "BSpace",
        NamedKey::CtrlC => "C-c",
    }
}

/// Streaming needs control-client flags added in tmux 3.2.
fn require_version(socket: &str) -> Result<()> {
    let raw = TmuxAdapter::query(socket, &["-V"]).unwrap_or_default();
    if let Some(version) = parse_version(&raw)
        && version < (3, 2)
    {
        bail!("live terminal requires tmux 3.2 or newer, found '{raw}'");
    }
    Ok(())
}

fn parse_version(raw: &str) -> Option<(u32, u32)> {
    let digits = raw.trim_start_matches(|c: char| !c.is_ascii_digit());
    let (major, rest) = digits.split_once('.')?;
    let minor: String = rest.chars().take_while(char::is_ascii_digit).collect();
    Some((major.parse().ok()?, minor.parse().ok()?))
}

fn is_descendant(mut child: u32, ancestor: u32) -> bool {
    for _ in 0..128 {
        if child == ancestor {
            return true;
        }
        let Some(parent) = parent_pid(child) else {
            return false;
        };
        if parent == child || parent == 0 {
            return false;
        }
        child = parent;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::multiplexer::PaneEvent;
    use std::time::{Duration, Instant};

    #[test]
    fn no_tmux_has_no_metadata() {
        if env::var_os("TMUX").is_none() {
            assert!(TmuxAdapter.inspect().unwrap().is_none());
            assert!(!TmuxAdapter.capabilities(false).send_input);
        }
    }

    #[test]
    fn versions_parse() {
        assert_eq!(parse_version("tmux 3.6b"), Some((3, 6)));
        assert_eq!(parse_version("tmux next-3.5"), Some((3, 5)));
        assert_eq!(parse_version("tmux 3.10a"), Some((3, 10)));
        assert_eq!(parse_version("tmux master"), None);
    }

    /// Isolated tmux server under a short temporary socket path; killed on
    /// drop. `None` (with a notice) when `tmux` is not on `PATH`.
    struct Server {
        _dir: tempfile::TempDir,
        socket: String,
    }

    impl Server {
        fn start(test: &str, width: u16, height: u16, command: &str) -> Option<Self> {
            if Command::new("tmux").arg("-V").output().is_err() {
                eprintln!("skipping {test}: tmux is not on PATH");
                return None;
            }
            let dir = tempfile::Builder::new()
                .prefix("st")
                .tempdir_in("/tmp")
                .unwrap();
            let socket = dir.path().join("s").to_string_lossy().into_owned();
            let status = Command::new("tmux")
                .args(["-S", &socket, "-f", "/dev/null", "new-session", "-d"])
                .args(["-x", &width.to_string(), "-y", &height.to_string()])
                .args(["-s", "sessiontap-test", command])
                .env_remove("TMUX")
                .status()
                .unwrap();
            assert!(status.success());
            Some(Self { _dir: dir, socket })
        }

        fn tmux(&self, args: &[&str]) -> String {
            TmuxAdapter::query(&self.socket, args).unwrap()
        }

        fn metadata(&self, pane: &str) -> (MultiplexerMetadata, u32) {
            let metadata = TmuxAdapter.current(&self.socket, pane).unwrap();
            let pid = metadata.pane_pid.unwrap();
            (metadata, pid)
        }

        fn wait_for(&self, pane: &str, needle: &str) -> String {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let captured = self.tmux(&["capture-pane", "-p", "-J", "-t", pane]);
                if captured.contains(needle) {
                    return captured;
                }
                assert!(Instant::now() < deadline, "{needle:?} not in {captured:?}");
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }

    impl Drop for Server {
        fn drop(&mut self) {
            let _ = Command::new("tmux")
                .args(["-S", &self.socket, "kill-server"])
                .status();
        }
    }

    const ECHO_INPUT: &str = "stty raw -echo; echo ready; exec cat -v";

    #[test]
    fn isolated_tmux_receives_literal_multiline_input() {
        let Some(server) = Server::start("literal_input", 80, 24, "sh") else {
            return;
        };
        let (metadata, pid) = server.metadata("sessiontap-test:0.0");
        TmuxAdapter
            .paste(
                &metadata,
                pid,
                "printf '%s\\n' '$HOME;\"quoted\"'\nprintf '%s\\n' second",
                true,
            )
            .unwrap();
        server.wait_for(&metadata.pane_id, "second\n");
        let captured = TmuxAdapter.capture(&metadata, pid).unwrap();
        assert!(captured.contains("$HOME;\"quoted\""));
    }

    #[test]
    fn keys_reach_the_pane_in_its_key_modes() {
        let command = format!("printf '\\033[?1h'; {ECHO_INPUT}");
        let Some(server) = Server::start("keys", 80, 24, &command) else {
            return;
        };
        let (metadata, pid) = server.metadata("sessiontap-test:0.0");
        server.wait_for(&metadata.pane_id, "ready");
        TmuxAdapter
            .send_keys(
                &metadata,
                pid,
                &[
                    Key::Char('E'),
                    Key::Char(';'),
                    Key::Char('-'),
                    Key::Named(NamedKey::Up),
                    Key::Named(NamedKey::Enter),
                ],
            )
            .unwrap();
        // Application cursor mode encodes Up as ESC O A.
        server.wait_for(&metadata.pane_id, "E;-^[OA^M");
    }

    #[test]
    fn paste_is_bracketed_when_enabled() {
        let command = format!("printf '\\033[?2004h'; {ECHO_INPUT}");
        let Some(server) = Server::start("bracketed_paste", 80, 24, &command) else {
            return;
        };
        let (metadata, pid) = server.metadata("sessiontap-test:0.0");
        server.wait_for(&metadata.pane_id, "ready");
        TmuxAdapter.paste(&metadata, pid, "hi", true).unwrap();
        server.wait_for(&metadata.pane_id, "^[[200~hi^[[201~^M");
    }

    fn next_event(stream: &mut PaneStream) -> PaneEvent {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match stream.events.try_recv() {
                Ok(event) => return event,
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                    assert!(Instant::now() < deadline, "no stream event");
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("stream closed: {error}"),
            }
        }
    }

    #[test]
    fn stream_yields_every_line_once_and_filters_other_panes() {
        let Some(server) = Server::start(
            "stream",
            80,
            24,
            "i=0; while [ $i -lt 1500 ]; do echo line$i; i=$((i+1)); sleep 0.002; done; echo done; exec sleep 60",
        ) else {
            return;
        };
        let (metadata, pid) = server.metadata("sessiontap-test:0.0");
        server.tmux(&[
            "split-window",
            "-d",
            "-t",
            &metadata.pane_id,
            "while :; do echo OTHER; sleep 0.01; done",
        ]);
        std::thread::sleep(Duration::from_millis(300));
        let mut stream = TmuxAdapter.open_stream(&metadata, pid).unwrap();
        let PaneEvent::Snapshot(snapshot) = next_event(&mut stream) else {
            panic!("first event is not a snapshot");
        };
        let mut text = String::from_utf8_lossy(&snapshot.data).into_owned();
        text.push('\n');
        let mut output = String::new();
        while !output.contains("done") {
            match next_event(&mut stream) {
                PaneEvent::Output(data) => output.push_str(&String::from_utf8_lossy(&data)),
                PaneEvent::Snapshot(_) | PaneEvent::Resync => panic!("unexpected resync"),
                _ => {}
            }
        }
        assert!(!output.contains("OTHER"));
        assert!(
            output.contains("line"),
            "no line arrived after the snapshot"
        );
        text.push_str(&output);
        let numbers: Vec<u32> = text
            .split(|c: char| !c.is_ascii_alphanumeric())
            .filter_map(|word| word.strip_prefix("line")?.parse().ok())
            .collect();
        let first = numbers[0];
        assert_eq!(numbers, (first..1500).collect::<Vec<_>>());
        drop(stream);
    }

    #[test]
    fn stream_never_resizes_the_pane() {
        let Some(server) = Server::start("no_resize", 160, 40, "exec sleep 60") else {
            return;
        };
        server.tmux(&["set-option", "-g", "window-size", "latest"]);
        let (metadata, pid) = server.metadata("sessiontap-test:0.0");
        let mut stream = TmuxAdapter.open_stream(&metadata, pid).unwrap();
        let PaneEvent::Snapshot(snapshot) = next_event(&mut stream) else {
            panic!("first event is not a snapshot");
        };
        assert_eq!((snapshot.cols, snapshot.rows), (160, 40));
        stream.control.request_snapshot().unwrap();
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(
            server.tmux(&[
                "display-message",
                "-p",
                "-t",
                &metadata.pane_id,
                "#{pane_width}"
            ]),
            "160"
        );
    }

    #[test]
    fn stream_ends_when_server_stops() {
        let Some(server) = Server::start("server_stop", 80, 24, "exec sleep 60") else {
            return;
        };
        let (metadata, pid) = server.metadata("sessiontap-test:0.0");
        let mut stream = TmuxAdapter.open_stream(&metadata, pid).unwrap();
        assert!(matches!(next_event(&mut stream), PaneEvent::Snapshot(_)));
        server.tmux(&["kill-server"]);
        loop {
            if let PaneEvent::Closed(reason) = next_event(&mut stream) {
                assert_eq!(
                    reason,
                    sessiontap_core::terminal::EndReason::MultiplexerStopped
                );
                break;
            }
        }
    }
}
