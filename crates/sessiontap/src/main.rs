use anyhow::{Context, Result, bail};
use chrono::Utc;
use rand::RngCore;
use sessiontap_adapters::{AdapterRegistry, SetupAction};
use sessiontap_core::{
    SCHEMA_VERSION,
    config::Config,
    domain::{
        Activity, InvocationId, InvocationSnapshot, Lifecycle, ProcessMetadata, derive_status,
    },
    multiplexer::{MultiplexerAdapter, TmuxAdapter},
    paths::AppPaths,
    protocol::{Request, Response},
};
use std::{env, io::Read, path::PathBuf, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    process::Command,
};

#[derive(Debug, PartialEq, Eq)]
enum Cli {
    Status,
    Listen,
    Setup {
        provider: Option<String>,
        action: SetupAction,
    },
    HookEmit {
        provider: String,
    },
    Completions {
        shell: Option<String>,
    },
    Launch {
        provider: String,
        args: Vec<String>,
    },
}
fn parse(mut args: impl Iterator<Item = String>) -> Result<Cli> {
    let Some(first) = args.next() else {
        bail!(
            "usage: sessiontap <claude|codex|qwen> [args...] | status | listen | setup | doctor | hooks remove | completions <shell>"
        )
    };
    match first.as_str() {
        "--status" | "status" => Ok(Cli::Status),
        "--listen" | "listen" => Ok(Cli::Listen),
        "setup" => Ok(Cli::Setup {
            provider: args.next(),
            action: SetupAction::Ensure,
        }),
        "doctor" => Ok(Cli::Setup {
            provider: args.next(),
            action: SetupAction::Doctor,
        }),
        "hooks" if args.next().as_deref() == Some("remove") => Ok(Cli::Setup {
            provider: args.next(),
            action: SetupAction::Remove,
        }),
        "hook" if args.next().as_deref() == Some("emit") => Ok(Cli::HookEmit {
            provider: args.next().context("missing provider")?,
        }),
        "completions" => Ok(Cli::Completions { shell: args.next() }),
        "--help" | "-h" => bail!(
            "usage: sessiontap <provider> [provider arguments...] | status | listen | setup | doctor | hooks remove | completions <shell>"
        ),
        provider => Ok(Cli::Launch {
            provider: provider.into(),
            args: args.collect(),
        }),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = parse(env::args().skip(1))?;
    if let Cli::Completions { shell } = cli {
        return completions(shell);
    }
    let paths = AppPaths::discover()?;
    match cli {
        Cli::Status => status(&paths).await,
        Cli::Listen => listen(&paths).await,
        Cli::Setup { provider, action } => setup(&paths, provider, action).await,
        Cli::HookEmit { provider } => hook_emit(&paths, &provider).await,
        Cli::Launch { provider, args } => launch(&paths, &provider, args).await,
        Cli::Completions { .. } => unreachable!(),
    }
}

fn completions(shell: Option<String>) -> Result<()> {
    match shell.as_deref() {
        Some("zsh") => {
            print!("{}", include_str!("../../../completions/zsh/_sessiontap"));
            Ok(())
        }
        Some(other) => bail!("unsupported shell: {other}"),
        None => bail!("usage: sessiontap completions <shell>"),
    }
}

async fn setup(paths: &AppPaths, provider: Option<String>, action: SetupAction) -> Result<()> {
    let home = PathBuf::from(env::var_os("HOME").context("HOME missing")?);
    let executable = env::current_exe()?;
    let config = Config::load(&paths.config_file()).unwrap_or_default();
    let registry = AdapterRegistry::new(&config);
    let providers = provider.map_or_else(
        || vec!["claude".into(), "codex".into(), "qwen".into()],
        |p| vec![p],
    );
    for p in providers {
        let (adapter, _) = registry.resolve(&p).context("unknown provider")?;
        let report = adapter.setup(&home, &executable, action).await?;
        eprintln!("{p}: {}", report.message);
    }
    Ok(())
}
async fn status(paths: &AppPaths) -> Result<()> {
    ensure_daemon(paths).await?;
    match request(paths, Request::Status).await? {
        Response::Status { invocations, .. } => {
            println!("{}", serde_json::to_string(&invocations)?);
            Ok(())
        }
        Response::Error(e) => bail!(e.message),
        _ => bail!("unexpected broker response"),
    }
}
async fn listen(paths: &AppPaths) -> Result<()> {
    ensure_daemon(paths).await?;
    let mut stream = UnixStream::connect(paths.socket()).await?;
    write_request(&mut stream, &Request::Listen).await?;
    let mut lines = BufReader::new(stream).lines();
    while let Some(line) = lines.next_line().await? {
        println!("{line}");
    }
    Ok(())
}

async fn launch(paths: &AppPaths, provider: &str, args: Vec<String>) -> Result<()> {
    let config = Config::load(&paths.config_file()).unwrap_or_default();
    let registry = AdapterRegistry::new(&config);
    let (adapter, executable) = registry
        .resolve(provider)
        .context("unknown provider; configure a custom inherited adapter")?;
    let hook_ready = if let Some(home) = env::var_os("HOME") {
        match adapter
            .setup(
                &PathBuf::from(home),
                &env::current_exe()?,
                SetupAction::Ensure,
            )
            .await
        {
            Ok(_) => true,
            Err(error) => {
                eprintln!("sessiontap: {provider} hooks unavailable; launching untracked: {error}");
                false
            }
        }
    } else {
        false
    };
    let tracked = if hook_ready {
        match ensure_daemon(paths).await {
            Ok(()) => true,
            Err(error) => {
                eprintln!("sessiontap: broker unavailable; launching untracked: {error}");
                false
            }
        }
    } else {
        false
    };
    let id = InvocationId::new();
    let credential = random_credential();
    let now = Utc::now();
    let cwd = env::current_dir()?;
    let sanitized = adapter.redact_args(&args);
    let mut prep = sessiontap_adapters::LaunchPreparation::default();
    if tracked {
        AppPaths::prepare_private(&paths.runtime_dir.join(id.to_string()))?;
        prep = adapter.prepare_launch(&args, &paths.runtime_dir.join(id.to_string()))?;
        let snapshot = InvocationSnapshot {
            schema_version: SCHEMA_VERSION,
            revision: 0,
            invocation_id: id.clone(),
            provider: provider.into(),
            executable: executable.clone(),
            args: sanitized,
            cwd: cwd.to_string_lossy().into_owned(),
            process: ProcessMetadata {
                wrapper_pid: std::process::id(),
                ..Default::default()
            },
            created_at: now,
            updated_at: now,
            lifecycle: Lifecycle::Starting,
            activity: Activity::Unknown,
            status: derive_status(Lifecycle::Starting, Activity::Unknown),
            provider_session: None,
            usage: None,
            repository: repository_metadata(&cwd),
            multiplexer: TmuxAdapter.inspect().unwrap_or(None),
            capabilities: TmuxAdapter.capabilities(std::env::var_os("TMUX").is_some()),
            turn_generation: 0,
            completed_generation: None,
        };
        if let Err(e) = request(
            paths,
            Request::Register {
                snapshot: Box::new(snapshot),
                credential: credential.clone(),
            },
        )
        .await
        {
            eprintln!("sessiontap: tracking unavailable: {e}");
        }
    }
    let mut command = Command::new(&executable);
    command
        .args(&args)
        .args(&prep.extra_args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .env("SESSIONTAP_INVOCATION_ID", id.to_string())
        .env("SESSIONTAP_CREDENTIAL", &credential)
        .env("SESSIONTAP_PROVIDER", provider);
    for (k, v) in prep.environment {
        command.env(k, v);
    }
    command.process_group(0);
    let mut child = command
        .spawn()
        .with_context(|| format!("launch {executable}"))?;
    let pid = child.id().context("child PID unavailable")?;
    let terminal = std::fs::File::open("/dev/tty").ok();
    if let Some(tty) = &terminal {
        match nix::unistd::tcsetpgrp(tty, nix::unistd::Pid::from_raw(pid as i32)) {
            Ok(()) | Err(nix::errno::Errno::EINVAL) => {}
            Err(error) => {
                eprintln!("sessiontap: provider may not control the terminal: {error}")
            }
        }
    }
    if tracked {
        let _ = request(
            paths,
            Request::BindChild {
                invocation_id: id.clone(),
                credential: credential.clone(),
                child_pid: pid,
                start_identity: process_start_identity(pid),
            },
        )
        .await;
    }
    let wait_result = wait_with_signal_forwarding(&mut child, pid).await;
    if let Some(tty) = &terminal {
        let _ = nix::unistd::tcsetpgrp(tty, nix::unistd::getpgrp());
    }
    let status = wait_result?;
    let code = status.code();
    let signal = std::os::unix::process::ExitStatusExt::signal(&status);
    if tracked {
        let _ = request(
            paths,
            Request::LifecycleExit {
                invocation_id: id,
                credential,
                exit_code: code,
                signal,
            },
        )
        .await;
    }
    if let Some(code) = code {
        std::process::exit(code)
    } else {
        std::process::exit(128 + signal.unwrap_or(1))
    }
}

async fn wait_with_signal_forwarding(
    child: &mut tokio::process::Child,
    pid: u32,
) -> Result<std::process::ExitStatus> {
    use nix::{
        sys::signal::{Signal, killpg},
        unistd::Pid,
    };
    use tokio::signal::unix::{SignalKind, signal};
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;
    let mut hangup = signal(SignalKind::hangup())?;
    let group = Pid::from_raw(pid as i32);
    loop {
        tokio::select! {
            status = child.wait() => return Ok(status?),
            _ = interrupt.recv() => { let _ = killpg(group, Signal::SIGINT); }
            _ = terminate.recv() => { let _ = killpg(group, Signal::SIGTERM); }
            _ = hangup.recv() => { let _ = killpg(group, Signal::SIGHUP); }
        }
    }
}

async fn hook_emit(paths: &AppPaths, provider: &str) -> Result<()> {
    let mut raw = Vec::new();
    std::io::stdin().read_to_end(&mut raw)?;
    let Ok(id) = env::var("SESSIONTAP_INVOCATION_ID") else {
        return Ok(());
    };
    let Ok(credential) = env::var("SESSIONTAP_CREDENTIAL") else {
        return Ok(());
    };
    if env::var("SESSIONTAP_PROVIDER").as_deref() != Ok(provider) {
        return Ok(());
    }
    let Ok(uuid) = uuid_parse(&id) else {
        return Ok(());
    };
    let config = Config::default();
    let registry = AdapterRegistry::new(&config);
    let Some((adapter, _)) = registry.resolve(provider) else {
        return Ok(());
    };
    let Ok(value) = serde_json::from_slice(&raw) else {
        return Ok(());
    };
    let Ok(normalized) = adapter.normalize(&uuid, &value) else {
        return Ok(());
    };
    let future = request(
        paths,
        Request::HookIngest {
            provider: provider.into(),
            invocation_id: uuid,
            credential,
            event: Box::new(normalized.event),
            attention: normalized.attention,
            failure: normalized.failure,
        },
    );
    let _ = tokio::time::timeout(Duration::from_millis(250), future).await;
    Ok(())
}

async fn ensure_daemon(paths: &AppPaths) -> Result<()> {
    if request(paths, Request::Health).await.is_ok() {
        return Ok(());
    }
    AppPaths::prepare_private(&paths.runtime_dir)?;
    let daemon = env::var_os("SESSIONTAPD")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            env::current_exe()
                .unwrap_or_else(|_| PathBuf::from("sessiontap"))
                .with_file_name("sessiontapd")
        });
    Command::new(daemon)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    for _ in 0..40 {
        if request(paths, Request::Health).await.is_ok() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    bail!("broker did not become ready")
}
async fn request(paths: &AppPaths, request: Request) -> Result<Response> {
    let mut stream = UnixStream::connect(paths.socket()).await?;
    write_request(&mut stream, &request).await?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).await?;
    Ok(serde_json::from_str(&line)?)
}
async fn write_request(stream: &mut UnixStream, request: &Request) -> Result<()> {
    stream.write_all(&serde_json::to_vec(request)?).await?;
    stream.write_all(b"\n").await?;
    Ok(())
}
fn random_credential() -> String {
    let mut bytes = [0_u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn uuid_parse(value: &str) -> Result<InvocationId> {
    Ok(InvocationId(uuid::Uuid::parse_str(value)?))
}
fn process_start_identity(pid: u32) -> Option<String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat.rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(19)
        .map(str::to_owned)
}
fn repository_metadata(cwd: &std::path::Path) -> Option<sessiontap_core::domain::Repository> {
    let cwd = cwd.to_owned();
    let (send, receive) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let _ = send.send(repository_metadata_inner(&cwd));
    });
    receive
        .recv_timeout(Duration::from_millis(75))
        .ok()
        .flatten()
}
fn repository_metadata_inner(cwd: &std::path::Path) -> Option<sessiontap_core::domain::Repository> {
    fn git(cwd: &std::path::Path, args: &[&str]) -> Option<String> {
        let o = std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .ok()?;
        o.status
            .success()
            .then(|| String::from_utf8_lossy(&o.stdout).trim().to_owned())
    }
    let root = git(cwd, &["rev-parse", "--show-toplevel"])?;
    Some(sessiontap_core::domain::Repository {
        root,
        branch: git(cwd, &["branch", "--show-current"]),
        head: git(cwd, &["rev-parse", "HEAD"]),
        dirty: git(cwd, &["status", "--porcelain"]).map(|s| !s.is_empty()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn provider_boundary_is_exact() {
        assert_eq!(
            parse(
                vec![
                    "codex".into(),
                    "--help".into(),
                    "a b".into(),
                    "$HOME".into()
                ]
                .into_iter()
            )
            .unwrap(),
            Cli::Launch {
                provider: "codex".into(),
                args: vec!["--help".into(), "a b".into(), "$HOME".into()]
            }
        );
    }

    #[test]
    fn completions_zsh_parses() {
        assert_eq!(
            parse(vec!["completions".into(), "zsh".into()].into_iter()).unwrap(),
            Cli::Completions {
                shell: Some("zsh".into())
            }
        );
    }

    #[test]
    fn completions_missing_shell_parses_none() {
        assert_eq!(
            parse(vec!["completions".into()].into_iter()).unwrap(),
            Cli::Completions { shell: None }
        );
    }

    #[test]
    fn embedded_completion_script_is_complete() {
        let script = include_str!("../../../completions/zsh/_sessiontap");
        assert!(script.starts_with("#compdef sessiontap"));
        for token in [
            "setup",
            "doctor",
            "hooks",
            "status",
            "listen",
            "completions",
            "claude",
            "codex",
            "qwen",
        ] {
            assert!(script.contains(token), "missing {token}");
        }
    }

    #[test]
    fn completion_scripts_pass_zsh_syntax_check() {
        let zsh_available = std::process::Command::new("which")
            .arg("zsh")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !zsh_available {
            eprintln!("skipping: zsh not on PATH");
            return;
        }
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        for script in [
            "completions/zsh/_sessiontap",
            "completions/zsh/_sessiontapd",
        ] {
            let path = root.join(script);
            let output = std::process::Command::new("zsh")
                .args(["-n", &path.to_string_lossy()])
                .output()
                .expect("failed to run zsh");
            assert!(
                output.status.success(),
                "{script} failed zsh -n: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
}
