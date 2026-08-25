use crate::domain::{Capabilities, MultiplexerMetadata};
use anyhow::{Context, Result, bail};
use std::{
    env,
    io::Write,
    path::Path,
    process::{Command, Stdio},
};

pub trait MultiplexerAdapter {
    fn inspect(&self) -> Result<Option<MultiplexerMetadata>>;
    fn capture(&self, expected: &MultiplexerMetadata, process_pid: u32) -> Result<String>;
    fn send_input(
        &self,
        expected: &MultiplexerMetadata,
        process_pid: u32,
        text: &[u8],
    ) -> Result<()>;
    fn capabilities(&self, present: bool) -> Capabilities {
        Capabilities {
            capture: present,
            send_input: present,
            usage: false,
        }
    }
}

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

    fn current(&self, socket: &str, pane: &str) -> Result<MultiplexerMetadata> {
        let format = "#{pid}\t#{session_id}\t#{session_name}\t#{window_id}\t#{window_index}\t#{pane_id}\t#{pane_tty}\t#{pane_pid}";
        let raw = Self::query(socket, &["display-message", "-p", "-t", pane, format])?;
        let fields: Vec<_> = raw.split('\t').collect();
        if fields.len() != 8 {
            bail!("unexpected tmux metadata");
        }
        Ok(MultiplexerMetadata {
            backend: "tmux".into(),
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

    fn send_input(
        &self,
        expected: &MultiplexerMetadata,
        process_pid: u32,
        text: &[u8],
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
            .write_all(text)?;
        if !child.wait()?.success() {
            bail!("tmux load-buffer failed");
        }
        let status = Command::new("tmux")
            .args([
                "-S",
                &expected.socket,
                "paste-buffer",
                "-d",
                "-b",
                &name,
                "-t",
                &expected.pane_id,
            ])
            .status()?;
        if !status.success() {
            bail!("tmux paste-buffer failed");
        }
        Ok(())
    }
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

fn parent_pid(pid: u32) -> Option<u32> {
    if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        return stat
            .rsplit_once(')')?
            .1
            .split_whitespace()
            .nth(1)?
            .parse()
            .ok();
    }
    let output = Command::new("ps")
        .args(["-o", "ppid=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    output.status.success().then_some(())?;
    String::from_utf8(output.stdout).ok()?.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn no_tmux_has_no_metadata() {
        if env::var_os("TMUX").is_none() {
            assert!(TmuxAdapter.inspect().unwrap().is_none());
            assert!(!TmuxAdapter.capabilities(false).send_input);
        }
    }

    #[test]
    #[ignore = "requires an installed tmux and Unix socket support"]
    fn isolated_tmux_receives_literal_multiline_input() {
        let temp = tempfile::tempdir().unwrap();
        let socket = temp.path().join("socket");
        let socket_text = socket.to_string_lossy().into_owned();
        assert!(
            Command::new("tmux")
                .args([
                    "-S",
                    &socket_text,
                    "new-session",
                    "-d",
                    "-s",
                    "sessiontap-test",
                    "sh"
                ])
                .status()
                .unwrap()
                .success()
        );
        let adapter = TmuxAdapter;
        let metadata = adapter
            .current(&socket_text, "sessiontap-test:0.0")
            .unwrap();
        let pid = metadata.pane_pid.unwrap();
        adapter
            .send_input(
                &metadata,
                pid,
                b"printf '%s\\n' '$HOME;\"quoted\"'\nprintf '%s\\n' second\n",
            )
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(100));
        let captured = adapter.capture(&metadata, pid).unwrap();
        assert!(captured.contains("$HOME;\"quoted\""));
        assert!(captured.contains("second"));
        let _ = Command::new("tmux")
            .args(["-S", &socket_text, "kill-server"])
            .status();
    }
}
