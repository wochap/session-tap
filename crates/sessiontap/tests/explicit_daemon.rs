use std::{
    fs,
    io::Write,
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Command, Stdio},
};

fn write_executable(path: &Path, body: &str) {
    fs::write(path, body).unwrap();
    let mut permissions = fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).unwrap();
}

fn isolated_command(temp: &tempfile::TempDir) -> Command {
    // Keep the wrapper from taking foreground control of the test runner's TTY.
    let mut command = Command::new("setsid");
    command
        .arg(env!("CARGO_BIN_EXE_sessiontap"))
        .env("HOME", temp.path().join("home"))
        .env("XDG_CONFIG_HOME", temp.path().join("config"))
        .env("XDG_STATE_HOME", temp.path().join("state"))
        .env("XDG_DATA_HOME", temp.path().join("data"))
        .env("XDG_RUNTIME_DIR", temp.path().join("runtime"));
    command
}

#[test]
fn daemon_absent_launch_is_untracked_and_preserves_process_contract() {
    let temp = tempfile::tempdir().unwrap();
    let provider = temp.path().join("fake-provider");
    let daemon_marker = temp.path().join("daemon-was-spawned");
    let fake_daemon = temp.path().join("fake-sessiontapd");
    write_executable(
        &provider,
        "#!/bin/sh\nprintf 'args:%s|%s\\n' \"$1\" \"$2\"\nprintf 'tracking:%s|%s|%s\\n' \"${SESSIONTAP_INVOCATION_ID-unset}\" \"${SESSIONTAP_CREDENTIAL-unset}\" \"${SESSIONTAP_PROVIDER-unset}\"\nIFS= read -r line\nprintf 'stdin:%s\\n' \"$line\"\nexit 7\n",
    );
    write_executable(
        &fake_daemon,
        &format!("#!/bin/sh\ntouch '{}'\n", daemon_marker.display()),
    );
    let config_dir = temp.path().join("config/sessiontap");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(
        config_dir.join("config.toml"),
        format!(
            "version = 1\n\n[adapters.fake]\nexecutable = {:?}\ninherits = \"claude\"\n",
            provider.to_string_lossy()
        ),
    )
    .unwrap();

    let mut child = isolated_command(&temp)
        .args(["fake", "two words", "$literal"])
        .env("SESSIONTAPD", &fake_daemon)
        .env("SESSIONTAP_INVOCATION_ID", "stale-id")
        .env("SESSIONTAP_CREDENTIAL", "stale-credential")
        .env("SESSIONTAP_PROVIDER", "stale-provider")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"interactive input\n")
        .unwrap();
    let output = child.wait_with_output().unwrap();

    assert_eq!(output.status.code(), Some(7));
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "args:two words|$literal\ntracking:unset|unset|unset\nstdin:interactive input\n"
    );
    assert!(
        String::from_utf8(output.stderr).unwrap().contains(
            "sessiontapd is not running; start it with `sessiontapd`; launching untracked"
        )
    );
    assert!(!daemon_marker.exists(), "the client spawned SESSIONTAPD");
}

#[test]
fn observation_commands_fail_without_starting_daemon() {
    let temp = tempfile::tempdir().unwrap();
    let daemon_marker = temp.path().join("daemon-was-spawned");
    let fake_daemon = temp.path().join("fake-sessiontapd");
    write_executable(
        &fake_daemon,
        &format!("#!/bin/sh\ntouch '{}'\n", daemon_marker.display()),
    );

    for subcommand in ["status", "listen"] {
        let output = isolated_command(&temp)
            .arg(subcommand)
            .env("SESSIONTAPD", &fake_daemon)
            .output()
            .unwrap();
        assert!(
            !output.status.success(),
            "{subcommand} unexpectedly succeeded"
        );
        assert!(
            String::from_utf8(output.stderr)
                .unwrap()
                .contains("sessiontapd is not running; start it with `sessiontapd`")
        );
    }
    assert!(!daemon_marker.exists(), "the client spawned SESSIONTAPD");
}

#[test]
fn hook_without_tracking_context_is_a_successful_no_op() {
    let temp = tempfile::tempdir().unwrap();
    let mut child = isolated_command(&temp)
        .args(["hook", "emit", "claude"])
        .env_remove("SESSIONTAP_INVOCATION_ID")
        .env_remove("SESSIONTAP_CREDENTIAL")
        .env_remove("SESSIONTAP_PROVIDER")
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(br#"{"hook_event_name":"Stop"}"#)
        .unwrap();
    assert!(child.wait().unwrap().success());
    assert!(
        !temp
            .path()
            .join("runtime/sessiontap/sessiontap.sock")
            .exists()
    );
}
