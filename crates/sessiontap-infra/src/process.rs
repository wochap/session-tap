use std::{fs, path::Path, process::Command};

/// Returns the whitespace-separated field `index` of `/proc/<pid>/stat`
/// counted after the parenthesized command name. The command name may itself
/// contain `)` and spaces, so parsing starts after the last `)`.
fn stat_field(stat: &str, index: usize) -> Option<&str> {
    stat.rsplit_once(')')?.1.split_whitespace().nth(index)
}

fn read_stat(pid: u32) -> Option<String> {
    fs::read_to_string(format!("/proc/{pid}/stat")).ok()
}

/// Process start time in clock ticks since boot, used to detect PID reuse.
#[must_use]
pub fn process_start_identity(pid: u32) -> Option<String> {
    stat_field(&read_stat(pid)?, 19).map(str::to_owned)
}

/// Parent PID from `/proc`, falling back to `ps` where `/proc` is absent.
#[must_use]
pub fn parent_pid(pid: u32) -> Option<u32> {
    if let Some(stat) = read_stat(pid) {
        return stat_field(&stat, 1)?.parse().ok();
    }
    let output = Command::new("ps")
        .args(["-o", "ppid=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    output.status.success().then_some(())?;
    String::from_utf8(output.stdout).ok()?.trim().parse().ok()
}

/// Returns whether `pid` is alive and, when given, still has the recorded
/// start identity (guards against PID reuse).
#[must_use]
pub fn process_alive(pid: u32, identity: Option<&str>) -> bool {
    if Path::new(&format!("/proc/{pid}")).exists() {
        return identity
            .is_none_or(|expected| process_start_identity(pid).as_deref() == Some(expected));
    }
    i32::try_from(pid)
        .is_ok_and(|raw| nix::sys::signal::kill(nix::unistd::Pid::from_raw(raw), None).is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stat_parsing_skips_command_containing_paren() {
        let stat =
            "4242 (evil) S 1 (x) R 77 4242 4242 0 -1 4194560 0 0 0 0 0 0 0 0 20 0 1 0 987654 0";
        assert_eq!(stat_field(stat, 0), Some("R"));
        assert_eq!(stat_field(stat, 1), Some("77"));
        assert_eq!(stat_field(stat, 19), Some("987654"));
    }

    #[test]
    fn current_process_identity_and_parent() {
        let pid = std::process::id();
        let identity = process_start_identity(pid);
        if Path::new("/proc/self/stat").exists() {
            assert!(identity.is_some());
            assert_eq!(parent_pid(pid), Some(std::os::unix::process::parent_id()));
        }
        assert!(process_alive(pid, identity.as_deref()));
        assert!(process_alive(pid, None));
    }

    #[test]
    fn reused_pid_is_not_alive() {
        let pid = std::process::id();
        if Path::new("/proc/self/stat").exists() {
            assert!(!process_alive(pid, Some("not-a-start-time")));
        }
    }
}
