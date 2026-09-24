use std::{fs, os::unix::fs::PermissionsExt, path::Path, process::Command};

fn write_executable(path: &Path, body: &str) {
    fs::write(path, body).unwrap();
    let mut permissions = fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).unwrap();
}

// A job-control shell on a pseudo-terminal launches the wrapper as a pipeline
// job, the way a script driving headless provider runs does. After the
// provider exits, the wrapper hands the terminal back from a background
// process group; that must not stop the wrapper's job with SIGTTOU.
#[test]
fn returning_terminal_control_does_not_stop_the_parent_job() {
    let temp = tempfile::tempdir().unwrap();
    let provider = temp.path().join("fake-provider");
    write_executable(&provider, "#!/bin/sh\necho provider-ran\n");
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

    let job = format!(
        "set -m; '{}' fake | cat; echo \"job-status:$?\"",
        env!("CARGO_BIN_EXE_sessiontap")
    );
    let output = Command::new("timeout")
        .args(["20", "script", "-qec"])
        .arg(format!("bash -c {}", shell_quote(&job)))
        .arg("/dev/null")
        .env("HOME", temp.path().join("home"))
        .env("XDG_CONFIG_HOME", temp.path().join("config"))
        .env("XDG_STATE_HOME", temp.path().join("state"))
        .env("XDG_DATA_HOME", temp.path().join("data"))
        .env("XDG_RUNTIME_DIR", temp.path().join("runtime"))
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("provider-ran"), "stdout: {stdout}");
    // 150 = 128 + SIGTTOU: the job was stopped instead of finishing.
    assert!(stdout.contains("job-status:0"), "stdout: {stdout}");
}

// A caller that renders its own UI keeps reading the terminal while it runs
// the provider headless (stdin not a terminal). The wrapper must leave the
// terminal to the caller, or the caller's read stops it with SIGTTIN.
#[test]
fn headless_launch_leaves_the_terminal_to_the_caller() {
    let temp = tempfile::tempdir().unwrap();
    let provider = temp.path().join("fake-provider");
    write_executable(&provider, "#!/bin/sh\nsleep 2\necho provider-ran\n");
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

    let caller = format!(
        "'{}' fake </dev/null & sleep 0.5; read -t 1 line; wait; exit 0",
        env!("CARGO_BIN_EXE_sessiontap")
    );
    let job = format!(
        "set -m; bash -c {}; echo \"job-status:$?\"",
        shell_quote(&caller)
    );
    let output = Command::new("timeout")
        .args(["20", "script", "-qec"])
        .arg(format!("bash -c {}", shell_quote(&job)))
        .arg("/dev/null")
        .env("HOME", temp.path().join("home"))
        .env("XDG_CONFIG_HOME", temp.path().join("config"))
        .env("XDG_STATE_HOME", temp.path().join("state"))
        .env("XDG_DATA_HOME", temp.path().join("data"))
        .env("XDG_RUNTIME_DIR", temp.path().join("runtime"))
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("provider-ran"), "stdout: {stdout}");
    // 149 = 128 + SIGTTIN: the caller was stopped reading its own terminal.
    assert!(stdout.contains("job-status:0"), "stdout: {stdout}");
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}
