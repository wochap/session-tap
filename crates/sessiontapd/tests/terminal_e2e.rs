//! End-to-end live terminal: daemon, wrapper, and `sessiontap terminal`
//! against an isolated tmux server. Runs when `tmux` is on `PATH`.

use sessiontap_core::terminal::{EndReason, InputUnavailable, TerminalFrame};
use std::{
    fs,
    io::{BufRead, BufReader},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};

const PROVIDER: &str = r#"#!/bin/sh
echo "provider-ready:$$"
while IFS= read -r line; do
  echo "got:$line"
  [ "$line" = quit ] && exit 0
done
"#;

struct Env {
    dir: tempfile::TempDir,
    wrapper: PathBuf,
    socket: String,
    daemon: Option<Child>,
}

impl Env {
    fn vars(&self) -> Vec<(&'static str, PathBuf)> {
        let root = self.dir.path();
        vec![
            ("HOME", root.join("home")),
            ("XDG_CONFIG_HOME", root.join("config")),
            ("XDG_STATE_HOME", root.join("state")),
            ("XDG_DATA_HOME", root.join("data")),
            ("XDG_RUNTIME_DIR", root.join("run")),
        ]
    }

    fn command(&self, program: impl AsRef<std::ffi::OsStr>) -> Command {
        let mut command = Command::new(program);
        command
            .envs(self.vars())
            .env_remove("TMUX")
            .env_remove("TMUX_PANE");
        command
    }

    fn sessiontap(&self, args: &[&str]) -> Output {
        self.command(&self.wrapper)
            .args(args)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    fn tmux(&self, args: &[&str]) -> String {
        let output = self
            .command("tmux")
            .args(["-S", &self.socket])
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "tmux {args:?}: {output:?}");
        String::from_utf8_lossy(&output.stdout)
            .trim_end()
            .to_owned()
    }

    fn type_line(&self, line: &str) {
        self.tmux(&["send-keys", "-t", "e2e", "-l", line]);
        self.tmux(&["send-keys", "-t", "e2e", "Enter"]);
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = Command::new("tmux")
            .args(["-S", &self.socket, "kill-server"])
            .status();
        if let Some(daemon) = &mut self.daemon {
            let _ = daemon.kill();
            let _ = daemon.wait();
        }
    }
}

fn wait_until<T>(what: &str, mut probe: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(value) = probe() {
            return value;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Collects watch frames and the pane text they carry.
struct Frames {
    receiver: mpsc::Receiver<TerminalFrame>,
    text: String,
    inputs: Vec<(bool, Option<InputUnavailable>)>,
    ended: Option<EndReason>,
}

impl Frames {
    fn pump(&mut self, timeout: Duration) {
        if let Ok(frame) = self.receiver.recv_timeout(timeout) {
            match frame {
                TerminalFrame::Snapshot { data, .. } | TerminalFrame::Output { data, .. } => {
                    self.text.push_str(&String::from_utf8_lossy(&data));
                }
                TerminalFrame::Input(input) => self.inputs.push((input.available, input.reason)),
                TerminalFrame::Ended { reason } => self.ended = Some(reason),
            }
        }
    }

    fn wait_text(&mut self, needle: &str) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while !self.text.contains(needle) {
            assert!(
                Instant::now() < deadline,
                "{needle:?} not in {:?}",
                self.text
            );
            self.pump(Duration::from_millis(100));
        }
    }

    fn wait_input(&mut self, available: bool, reason: Option<InputUnavailable>) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while self.inputs.last() != Some(&(available, reason)) {
            assert!(Instant::now() < deadline, "input frames: {:?}", self.inputs);
            self.pump(Duration::from_millis(100));
        }
    }
}

fn refused_with(output: &Output, code: &str) {
    assert!(!output.status.success(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains(code),
        "{output:?}"
    );
}

#[test]
fn live_terminal_streams_guards_input_and_ends_on_exit() {
    if Command::new("tmux").arg("-V").output().is_err() {
        eprintln!("skipping terminal end-to-end test: tmux is not on PATH");
        return;
    }
    let daemon_bin = Path::new(env!("CARGO_BIN_EXE_sessiontapd"));
    let wrapper = daemon_bin.with_file_name("sessiontap");
    if !wrapper.exists() {
        eprintln!(
            "skipping terminal end-to-end test: build the sessiontap binary first (cargo test --workspace)"
        );
        return;
    }
    let dir = tempfile::Builder::new()
        .prefix("ste")
        .tempdir_in("/tmp")
        .unwrap();
    let socket = dir.path().join("t").to_string_lossy().into_owned();
    let mut env = Env {
        dir,
        wrapper,
        socket,
        daemon: None,
    };
    let root = env.dir.path().to_owned();
    let provider = root.join("fake-provider");
    fs::write(&provider, PROVIDER).unwrap();
    fs::set_permissions(&provider, fs::Permissions::from_mode(0o755)).unwrap();
    fs::create_dir_all(root.join("config/sessiontap")).unwrap();
    fs::create_dir_all(root.join("home")).unwrap();
    fs::write(
        root.join("config/sessiontap/config.toml"),
        format!(
            "version = 1\n\n[adapters.fake]\nexecutable = {:?}\ninherits = \"claude\"\n",
            provider.to_string_lossy()
        ),
    )
    .unwrap();

    env.daemon = Some(
        env.command(daemon_bin)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    wait_until("daemon", || {
        env.sessiontap(&["status"]).status.success().then_some(())
    });

    env.tmux(&[
        "-f",
        "/dev/null",
        "new-session",
        "-d",
        "-x",
        "120",
        "-y",
        "30",
        "-s",
        "e2e",
        "env PS1='e2e-shell$ ' bash --norc -i",
    ]);
    wait_until("shell prompt", || {
        env.tmux(&["capture-pane", "-p", "-t", "e2e"])
            .contains("e2e-shell$")
            .then_some(())
    });
    env.type_line(&format!("'{}' fake", env.wrapper.display()));

    let id = wait_until("tracked terminal", || {
        let output = env.sessiontap(&["status"]);
        let views: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
        views.as_array()?.iter().find_map(|view| {
            (view["provider"] == "fake" && view["terminal"]["quick_pick"] == "digits")
                .then(|| view["invocation_id"].as_str().unwrap().to_owned())
        })
    });

    let mut watch = env
        .command(&env.wrapper)
        .args(["terminal", "watch", &id[..8]])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let (sender, receiver) = mpsc::channel();
    let stdout = watch.stdout.take().unwrap();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if let Ok(frame) = serde_json::from_str(&line) {
                let _ = sender.send(frame);
            }
        }
    });
    let first = receiver.recv_timeout(Duration::from_secs(15)).unwrap();
    assert!(matches!(first, TerminalFrame::Snapshot { .. }), "{first:?}");
    let mut frames = Frames {
        receiver,
        text: String::new(),
        inputs: Vec::new(),
        ended: None,
    };
    if let TerminalFrame::Snapshot { data, .. } = first {
        frames.text.push_str(&String::from_utf8_lossy(&data));
    }
    frames.wait_text("provider-ready:");
    let pid: u32 = frames
        .text
        .split("provider-ready:")
        .nth(1)
        .unwrap()
        .split(|c: char| !c.is_ascii_digit())
        .next()
        .unwrap()
        .parse()
        .unwrap();

    assert!(
        env.sessiontap(&["terminal", "send", &id[..8], "--key", "x", "--key", "enter"])
            .status
            .success()
    );
    frames.wait_text("got:x");

    assert!(
        Command::new("kill")
            .args(["-TSTP", &pid.to_string()])
            .status()
            .unwrap()
            .success()
    );
    frames.wait_input(false, Some(InputUnavailable::NotForeground));
    refused_with(
        &env.sessiontap(&["terminal", "send", &id, "--key", "y"]),
        "not_foreground",
    );

    env.type_line("fg");
    frames.wait_input(true, None);
    assert!(
        env.sessiontap(&["terminal", "send", &id, "--text", "z", "--enter"])
            .status
            .success()
    );
    frames.wait_text("got:z");

    env.tmux(&["copy-mode", "-t", "e2e"]);
    refused_with(
        &env.sessiontap(&["terminal", "send", &id, "--key", "1"]),
        "pane_in_mode",
    );
    env.tmux(&["send-keys", "-t", "e2e", "-X", "cancel"]);
    frames.wait_input(true, None);

    assert!(
        env.sessiontap(&["terminal", "send", &id, "--text", "quit", "--enter"])
            .status
            .success()
    );
    let deadline = Instant::now() + Duration::from_secs(15);
    while frames.ended.is_none() {
        assert!(
            Instant::now() < deadline,
            "stream did not end: {:?}",
            frames.text
        );
        frames.pump(Duration::from_millis(100));
    }
    assert_eq!(frames.ended, Some(EndReason::AgentExited));
    let after_quit = frames.text.rsplit("got:quit").next().unwrap().to_owned();
    assert!(!after_quit.contains("e2e-shell"), "{after_quit:?}");
    assert!(watch.wait().unwrap().success());
    refused_with(
        &env.sessiontap(&["terminal", "send", &id, "--key", "1"]),
        "terminal_ended",
    );
}
