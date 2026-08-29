use anyhow::{Context, Result, bail};
use chrono::Utc;
use fs2::FileExt;
use rand::RngCore;
use sessiontap_adapters::{
    ADAPTER_API_VERSION, AdapterRegistry, SetupAction, stamp_invocation_workspace,
};
use sessiontap_core::{
    SCHEMA_VERSION,
    config::Config,
    domain::{
        Activity, ActivityConfirmation, EventEvidence, EvidenceChannel, EvidenceTrust,
        InvocationId, InvocationSnapshot, Lifecycle, ProcessMetadata, derive_status,
    },
    multiplexer::{MultiplexerAdapter, TmuxAdapter},
    paths::AppPaths,
    protocol::{Request, Response},
};
use std::{
    env,
    fs::{self, OpenOptions},
    io::{Read, Write},
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixDatagram, UnixStream},
    process::Command,
    sync::mpsc,
};

const INSPECTION_MAX_INPUT_BYTES: usize = 32 * 1024;
const INSPECTION_MAX_DATAGRAM_BYTES: usize = 256 * 1024;
const INSPECTION_QUEUE_DEPTH: usize = 64;
const HOOK_TIMEOUT: Duration = Duration::from_millis(250);

#[derive(Debug, PartialEq, Eq)]
enum Cli {
    Status,
    Listen,
    InspectHooks,
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
            "usage: sessiontap <claude|codex|qwen> [args...] | status | listen | inspect-hooks | setup | doctor | hooks remove | completions <shell>"
        )
    };
    match first.as_str() {
        "--status" | "status" => Ok(Cli::Status),
        "--listen" | "listen" => Ok(Cli::Listen),
        "inspect-hooks" => Ok(Cli::InspectHooks),
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
            "usage: sessiontap <provider> [provider arguments...] | status | listen | inspect-hooks | setup | doctor | hooks remove | completions <shell>"
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
        Cli::InspectHooks => inspect_hooks(&paths).await,
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
    require_daemon(paths).await?;
    match request(paths, Request::Status).await? {
        Response::Status { views, .. } => {
            println!("{}", serde_json::to_string(&views)?);
            Ok(())
        }
        Response::Error(e) => bail!(e.message),
        _ => bail!("unexpected broker response"),
    }
}
async fn listen(paths: &AppPaths) -> Result<()> {
    require_daemon(paths).await?;
    let mut stream = UnixStream::connect(paths.socket()).await?;
    write_request(&mut stream, &Request::Listen).await?;
    let mut lines = BufReader::new(stream).lines();
    while let Some(line) = lines.next_line().await? {
        println!("{line}");
    }
    Ok(())
}

struct InspectionEndpoint {
    socket: PathBuf,
    _lock: std::fs::File,
}

impl Drop for InspectionEndpoint {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.socket);
    }
}

async fn inspect_hooks(paths: &AppPaths) -> Result<()> {
    AppPaths::prepare_private(&paths.runtime_dir)?;
    let lock = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(paths.hook_inspection_lock())?;
    lock.try_lock_exclusive()
        .context("another hook inspector is already running")?;
    let socket_path = paths.hook_inspection_socket();
    if fs::symlink_metadata(&socket_path).is_ok() {
        fs::remove_file(&socket_path).context("remove stale hook inspection endpoint")?;
    }
    eprintln!(
        "WARNING: raw hook payloads may contain prompts, tool inputs, paths, credentials, and other sensitive data. Terminal scrollback and explicit redirection may retain this output. SessionTap does not persist or forward it."
    );
    let socket = UnixDatagram::bind(&socket_path).context("bind hook inspection endpoint")?;
    let _endpoint = InspectionEndpoint {
        socket: socket_path,
        _lock: lock,
    };
    fs::set_permissions(&_endpoint.socket, fs::Permissions::from_mode(0o600))?;
    let (sender, mut receiver) = mpsc::channel::<Vec<u8>>(INSPECTION_QUEUE_DEPTH);
    let writer = tokio::task::spawn_blocking(move || {
        let stdout = std::io::stdout();
        let mut output = stdout.lock();
        while let Some(record) = receiver.blocking_recv() {
            if output.write_all(&record).is_err()
                || output.write_all(b"\n").is_err()
                || output.flush().is_err()
            {
                break;
            }
        }
    });
    let mut buffer = vec![0_u8; INSPECTION_MAX_DATAGRAM_BYTES];
    loop {
        tokio::select! {
            result = socket.recv(&mut buffer) => {
                let size = result?;
                let record = &buffer[..size];
                if !valid_inspection_record(record) {
                    eprintln!("sessiontap: dropped malformed hook inspection record");
                } else if sender.try_send(record.to_vec()).is_err() {
                    eprintln!("sessiontap: dropped hook inspection record because output is overloaded");
                }
            }
            signal = tokio::signal::ctrl_c() => {
                signal?;
                break;
            }
        }
    }
    drop(sender);
    let _ = writer.await;
    Ok(())
}

async fn launch(paths: &AppPaths, provider: &str, args: Vec<String>) -> Result<()> {
    let config = Config::load(&paths.config_file()).unwrap_or_default();
    let registry = AdapterRegistry::new(&config);
    let (adapter, executable) = registry
        .resolve(provider)
        .context("unknown provider; configure a custom inherited adapter")?;
    let daemon_ready = daemon_is_healthy(paths).await;
    if !daemon_ready {
        eprintln!(
            "sessiontap: sessiontapd is not running; start it with `sessiontapd`; launching untracked"
        );
    }
    let hook_ready = if daemon_ready && let Some(home) = env::var_os("HOME") {
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
    let mut tracked = daemon_ready && hook_ready;
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
            state_started_at: now,
            last_state_asserted_at: None,
            activity_confirmation: ActivityConfirmation::Live,
            last_evidence: None,
            source_ordering: vec![],
            current_tool_activity: None,
            status: derive_status(Lifecycle::Starting, Activity::Unknown),
            provider_session: None,
            provider_metadata: None,
            usage: None,
            repository: repository_metadata(&cwd),
            multiplexer: TmuxAdapter.inspect().unwrap_or(None),
            capabilities: TmuxAdapter.capabilities(std::env::var_os("TMUX").is_some()),
            turn_generation: 0,
            completed_generation: None,
        };
        match request(
            paths,
            Request::Register {
                snapshot: Box::new(snapshot),
                credential: credential.clone(),
            },
        )
        .await
        {
            Ok(Response::Ok) => {}
            Ok(Response::Error(error)) => {
                eprintln!(
                    "sessiontap: tracking unavailable; launching untracked: {}",
                    error.message
                );
                tracked = false;
                prep = sessiontap_adapters::LaunchPreparation::default();
            }
            Ok(_) => {
                eprintln!(
                    "sessiontap: tracking unavailable; launching untracked: unexpected broker response"
                );
                tracked = false;
                prep = sessiontap_adapters::LaunchPreparation::default();
            }
            Err(error) => {
                eprintln!("sessiontap: tracking unavailable; launching untracked: {error}");
                tracked = false;
                prep = sessiontap_adapters::LaunchPreparation::default();
            }
        }
    }
    let mut command = Command::new(&executable);
    command
        .args(&args)
        .args(&prep.extra_args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    if tracked {
        command
            .env("SESSIONTAP_INVOCATION_ID", id.to_string())
            .env("SESSIONTAP_CREDENTIAL", &credential)
            .env("SESSIONTAP_PROVIDER", provider)
            .env("SESSIONTAP_WORKSPACE", &cwd);
    } else {
        remove_tracking_environment(&mut command);
    }
    for (k, v) in &prep.environment {
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
    let side_channel_task = if tracked {
        prep.side_channel.map(|side_channel| {
            tokio::spawn(tail_provider_side_channel(
                paths.clone(),
                provider.to_owned(),
                id.clone(),
                credential.clone(),
                side_channel,
                cwd.clone(),
            ))
        })
    } else {
        None
    };
    let wait_result = wait_with_signal_forwarding(&mut child, pid).await;
    if let Some(task) = side_channel_task {
        task.abort();
        let _ = task.await;
    }
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

async fn tail_provider_side_channel(
    paths: AppPaths,
    provider: String,
    invocation_id: InvocationId,
    credential: String,
    path: PathBuf,
    workspace: PathBuf,
) {
    let config = Config::load(&paths.config_file()).unwrap_or_default();
    let registry = AdapterRegistry::new(&config);
    let Some((adapter, _)) = registry.resolve(&provider) else {
        return;
    };
    let mut tail = sessiontap_adapters::qwen::QwenJsonlTail::new(path, 64 * 1024);
    let mut source_sequence = 0_u64;
    loop {
        match tail.poll() {
            Ok(values) => {
                for mut value in values {
                    stamp_invocation_workspace(&mut value, Some(&workspace));
                    source_sequence = source_sequence.saturating_add(1);
                    let evidence = EventEvidence {
                        channel: EvidenceChannel::SideChannel,
                        trust: EvidenceTrust::LocalObservation,
                        collector_revision: Some(ADAPTER_API_VERSION.into()),
                        collector_instance_id: Some(invocation_id.to_string()),
                        source_sequence: Some(source_sequence),
                    };
                    if let Ok(Some(mut normalized)) = adapter
                        .normalize_with_evidence(&invocation_id, &value, evidence)
                        .map(|outcome| outcome.into_event())
                    {
                        normalized.event.provider = provider.clone();
                        let _ = request(
                            &paths,
                            Request::HookIngest {
                                provider: provider.clone(),
                                invocation_id: invocation_id.clone(),
                                credential: credential.clone(),
                                event: Box::new(normalized.event),
                                status_reason: normalized.status_reason,
                            },
                        )
                        .await;
                    }
                }
            }
            Err(error) => {
                eprintln!("sessiontap: provider side channel disabled: {error}");
                return;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
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
    std::io::stdin()
        .take((INSPECTION_MAX_INPUT_BYTES + 1) as u64)
        .read_to_end(&mut raw)?;
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
    let config = Config::load(&paths.config_file()).unwrap_or_default();
    let registry = AdapterRegistry::new(&config);
    let Some((adapter, _)) = registry.resolve(provider) else {
        return Ok(());
    };
    inspect_hook_best_effort(paths, provider, &raw).await;
    let Ok(mut value) = serde_json::from_slice(&raw) else {
        return Ok(());
    };
    let workspace = env::var_os("SESSIONTAP_WORKSPACE").map(PathBuf::from);
    stamp_invocation_workspace(&mut value, workspace.as_deref());
    let Ok(Some(mut normalized)) = adapter
        .normalize_with_evidence(
            &uuid,
            &value,
            EventEvidence::managed_hook(ADAPTER_API_VERSION.into()),
        )
        .map(|outcome| outcome.into_event())
    else {
        return Ok(());
    };
    // The adapter selects a dialect; the authenticated invocation selects the
    // configured provider identity exposed by internal and public state.
    normalized.event.provider = provider.to_owned();
    let future = request(
        paths,
        Request::HookIngest {
            provider: provider.into(),
            invocation_id: uuid,
            credential,
            event: Box::new(normalized.event),
            status_reason: normalized.status_reason,
        },
    );
    let _ = tokio::time::timeout(HOOK_TIMEOUT, future).await;
    Ok(())
}

fn hook_type(value: &serde_json::Value) -> Option<&str> {
    value
        .get("hook_event_name")
        .or_else(|| value.get("event_name"))
        .or_else(|| value.get("type"))
        .and_then(serde_json::Value::as_str)
}

fn inspection_record(provider: &str, raw: &[u8]) -> Vec<u8> {
    let (hook_type, payload) = if raw.len() > INSPECTION_MAX_INPUT_BYTES {
        (
            None,
            serde_json::json!({
                "inspection_error": "oversized_input",
                "at_least_bytes": raw.len(),
                "maximum_bytes": INSPECTION_MAX_INPUT_BYTES
            }),
        )
    } else {
        match serde_json::from_slice::<serde_json::Value>(raw) {
            Ok(value) => (hook_type(&value).map(str::to_owned), value),
            Err(_) => (
                None,
                serde_json::json!({
                    "inspection_error": "invalid_json",
                    "encoding": "hex",
                    "bytes": hex::encode(raw)
                }),
            ),
        }
    };
    serde_json::to_vec(&serde_json::json!({
        "provider": provider,
        "hook_type": hook_type,
        "payload": payload
    }))
    .expect("inspection record is serializable")
}

fn valid_inspection_record(record: &[u8]) -> bool {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(record) else {
        return false;
    };
    let Some(object) = value.as_object() else {
        return false;
    };
    object.len() == 3
        && object
            .get("provider")
            .is_some_and(serde_json::Value::is_string)
        && object
            .get("hook_type")
            .is_some_and(|value| value.is_null() || value.is_string())
        && object.contains_key("payload")
}

async fn inspect_hook_best_effort(paths: &AppPaths, provider: &str, raw: &[u8]) {
    let record = inspection_record(provider, raw);
    let future = async {
        let socket = UnixDatagram::unbound()?;
        socket.connect(paths.hook_inspection_socket())?;
        socket.send(&record).await?;
        std::io::Result::Ok(())
    };
    let _ = tokio::time::timeout(Duration::from_millis(20), future).await;
}

async fn daemon_is_healthy(paths: &AppPaths) -> bool {
    matches!(
        request(paths, Request::Health).await,
        Ok(Response::Health { .. })
    )
}

async fn require_daemon(paths: &AppPaths) -> Result<()> {
    if daemon_is_healthy(paths).await {
        Ok(())
    } else {
        bail!("sessiontapd is not running; start it with `sessiontapd`")
    }
}

fn remove_tracking_environment(command: &mut Command) {
    for key in [
        "SESSIONTAP_INVOCATION_ID",
        "SESSIONTAP_CREDENTIAL",
        "SESSIONTAP_PROVIDER",
        "SESSIONTAP_WORKSPACE",
    ] {
        command.env_remove(key);
    }
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
    fn inspect_hooks_parses() {
        assert_eq!(
            parse(vec!["inspect-hooks".into()].into_iter()).unwrap(),
            Cli::InspectHooks
        );
    }

    #[test]
    fn inspection_record_preserves_known_and_unknown_json() {
        let raw = br#"{"hook_event_name":"FutureEvent","nested":{"unknown":[1,true]}}"#;
        let record: serde_json::Value =
            serde_json::from_slice(&inspection_record("claude", raw)).unwrap();
        assert_eq!(record["provider"], "claude");
        assert_eq!(record["hook_type"], "FutureEvent");
        assert_eq!(record["payload"]["nested"]["unknown"][1], true);
    }

    #[test]
    fn inspection_record_supports_public_discriminators_and_missing_type() {
        for (raw, expected) in [
            (
                serde_json::json!({"event_name":"turn.started"}),
                Some("turn.started"),
            ),
            (
                serde_json::json!({"type":"notification"}),
                Some("notification"),
            ),
            (serde_json::json!({"future":true}), None),
        ] {
            let encoded = serde_json::to_vec(&raw).unwrap();
            let record: serde_json::Value =
                serde_json::from_slice(&inspection_record("codex", &encoded)).unwrap();
            assert_eq!(record["hook_type"].as_str(), expected);
            assert_eq!(record["payload"], raw);
        }
    }

    #[test]
    fn inspection_record_represents_invalid_bytes_losslessly() {
        let raw = b"{not-json:\xff}";
        let record: serde_json::Value =
            serde_json::from_slice(&inspection_record("qwen", raw)).unwrap();
        assert_eq!(record["hook_type"], serde_json::Value::Null);
        assert_eq!(record["payload"]["inspection_error"], "invalid_json");
        assert_eq!(record["payload"]["encoding"], "hex");
        assert_eq!(record["payload"]["bytes"], hex::encode(raw));
    }

    #[test]
    fn inspection_record_reports_oversize_without_truncation() {
        let raw = vec![b'x'; INSPECTION_MAX_INPUT_BYTES + 1];
        let record: serde_json::Value =
            serde_json::from_slice(&inspection_record("claude", &raw)).unwrap();
        assert_eq!(record["payload"]["inspection_error"], "oversized_input");
        assert_eq!(
            record["payload"]["at_least_bytes"],
            INSPECTION_MAX_INPUT_BYTES + 1
        );
        assert!(record["payload"].get("bytes").is_none());
    }

    #[test]
    fn inspection_envelope_rejects_malformed_or_extended_records() {
        assert!(!valid_inspection_record(b"not json"));
        assert!(!valid_inspection_record(br#"{"provider":"codex"}"#));
        assert!(!valid_inspection_record(
            br#"{"provider":"codex","hook_type":7,"payload":{}}"#
        ));
        assert!(!valid_inspection_record(
            br#"{"provider":"codex","hook_type":null,"payload":{},"raw":"leak"}"#
        ));
        assert!(valid_inspection_record(&inspection_record(
            "codex",
            br#"{"type":"Stop"}"#
        )));
    }

    #[test]
    fn inspection_lock_allows_only_one_inspector() {
        let temp = tempfile::tempdir().unwrap();
        let lock_path = temp.path().join("inspect.lock");
        let first = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .unwrap();
        let second = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&lock_path)
            .unwrap();
        first.try_lock_exclusive().unwrap();
        assert!(second.try_lock_exclusive().is_err());
    }

    #[test]
    fn inspection_output_queue_is_bounded() {
        let (sender, _receiver) = mpsc::channel::<Vec<u8>>(1);
        sender.try_send(vec![1]).unwrap();
        assert!(matches!(
            sender.try_send(vec![2]),
            Err(mpsc::error::TrySendError::Full(_))
        ));
    }

    #[test]
    fn raw_inspection_envelope_is_not_a_broker_request() {
        let marker = "raw-secret-that-must-not-enter-storage-or-sinks";
        let record = inspection_record(
            "claude",
            serde_json::json!({"hook_event_name":"Unknown", "private": marker})
                .to_string()
                .as_bytes(),
        );
        assert!(serde_json::from_slice::<Request>(&record).is_err());
        assert!(String::from_utf8(record).unwrap().contains(marker));
    }

    #[tokio::test]
    async fn inspection_delivery_is_ephemeral_and_provider_independent() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths {
            config_dir: temp.path().join("config"),
            state_dir: temp.path().join("state"),
            data_dir: temp.path().join("data"),
            runtime_dir: temp.path().join("runtime"),
        };
        AppPaths::prepare_private(&paths.runtime_dir).unwrap();
        let listener = match UnixDatagram::bind(paths.hook_inspection_socket()) {
            Ok(listener) => listener,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                eprintln!("skipping: Unix datagrams are blocked by the test sandbox");
                return;
            }
            Err(error) => panic!("bind inspection endpoint: {error}"),
        };
        for (provider, raw) in [
            ("claude", br#"{"hook_event_name":"Stop"}"#.as_slice()),
            (
                "codex",
                br#"{"type":"unknown-event","extra":{"x":1}}"#.as_slice(),
            ),
        ] {
            inspect_hook_best_effort(&paths, provider, raw).await;
            let mut buffer = vec![0_u8; INSPECTION_MAX_DATAGRAM_BYTES];
            let size = tokio::time::timeout(Duration::from_millis(100), listener.recv(&mut buffer))
                .await
                .unwrap()
                .unwrap();
            let value: serde_json::Value = serde_json::from_slice(&buffer[..size]).unwrap();
            assert_eq!(value["provider"], provider);
        }
        drop(listener);
        fs::remove_file(paths.hook_inspection_socket()).unwrap();
        tokio::time::timeout(
            Duration::from_millis(100),
            inspect_hook_best_effort(&paths, "qwen", br#"{"type":"Stop"}"#),
        )
        .await
        .unwrap();
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
            "inspect-hooks",
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
