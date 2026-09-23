use anyhow::{Context, Result, bail};
use chrono::Utc;
use rand::RngCore;
use sessiontap_adapters::{
    ADAPTER_API_VERSION, AdapterRegistry, NormalizeContext, SetupAction, SideChannelSource,
};
use sessiontap_core::{
    SCHEMA_VERSION,
    domain::{
        Activity, ActivityConfirmation, EventEvidence, EvidenceChannel, EvidenceTrust,
        InvocationId, InvocationSnapshot, Lifecycle, ProcessMetadata, derive_status,
    },
    paths::AppPaths,
    protocol::{Request, Response},
};
use sessiontap_infra::{
    config::load_config,
    fs::prepare_private_dir,
    multiplexer::MultiplexerRegistry,
    process::process_start_identity,
    socket::{bind_error, bind_private_unix_datagram},
};
use std::{
    env, fs,
    io::{Read, Write},
    path::{Path, PathBuf},
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
        bail!("{}", usage("args..."))
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
        "--help" | "-h" => bail!("{}", usage("provider arguments...")),
        provider => Ok(Cli::Launch {
            provider: provider.into(),
            args: args.collect(),
        }),
    }
}

/// Usage text; the provider list comes from the adapter registry.
fn usage(provider_args: &str) -> String {
    let providers = AdapterRegistry::builtin_ids()
        .iter()
        .map(|id| id.as_str())
        .collect::<Vec<_>>()
        .join("|");
    format!(
        "usage: sessiontap <{providers}> [{provider_args}] | status | listen | inspect-hooks | setup | doctor | hooks remove | completions <shell>"
    )
}

fn unknown_provider(registry: &AdapterRegistry, provider: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "unknown provider '{provider}'; expected one of {}",
        registry.provider_names().join(", ")
    )
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
    let config = load_config(&paths.config_file()).unwrap_or_default();
    let registry = AdapterRegistry::new(&config);
    let providers = provider.map_or_else(
        || {
            AdapterRegistry::builtin_ids()
                .iter()
                .map(ToString::to_string)
                .collect()
        },
        |p| vec![p],
    );
    for p in providers {
        let (adapter, _) = registry
            .resolve(&p)
            .ok_or_else(|| unknown_provider(&registry, &p))?;
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

/// Binds the private hook-inspection datagram socket under its exclusive
/// lock. A stale socket file is replaced; a live one is left alone.
fn bind_inspection_endpoint(paths: &AppPaths) -> Result<(UnixDatagram, InspectionEndpoint)> {
    prepare_private_dir(&paths.runtime_dir)?;
    let socket_path = paths.hook_inspection_socket();
    let (socket, lock) = bind_private_unix_datagram(&socket_path, &paths.hook_inspection_lock())
        .map_err(|error| bind_error("hook inspector", &socket_path, error))?;
    Ok((
        socket,
        InspectionEndpoint {
            socket: socket_path,
            _lock: lock,
        },
    ))
}

async fn inspect_hooks(paths: &AppPaths) -> Result<()> {
    let (socket, _endpoint) = bind_inspection_endpoint(paths)?;
    eprintln!(
        "WARNING: raw hook payloads may contain prompts, tool inputs, paths, credentials, and other sensitive data. Terminal scrollback and explicit redirection may retain this output. SessionTap does not persist or forward it."
    );
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
    let config = load_config(&paths.config_file()).unwrap_or_default();
    let registry = AdapterRegistry::new(&config);
    let (adapter, executable) = registry.resolve(provider).ok_or_else(|| {
        unknown_provider(&registry, provider)
            .context("configure a custom adapter that inherits a built-in provider")
    })?;
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
        prepare_private_dir(&paths.runtime_dir.join(id.to_string()))?;
        prep = adapter.prepare_launch(
            &args,
            &paths.runtime_dir.join(id.to_string()),
            Path::new(&executable),
        )?;
        let multiplexers = MultiplexerRegistry::new();
        let multiplexer = multiplexers.detect().unwrap_or(None);
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
            multiplexer: multiplexer.clone(),
            capabilities: multiplexers.capabilities(multiplexer.as_ref()),
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
        match set_terminal_foreground(tty, nix::unistd::Pid::from_raw(pid as i32)) {
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
        let _ = set_terminal_foreground(tty, nix::unistd::getpgrp());
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
    mut source: Box<dyn SideChannelSource>,
    workspace: PathBuf,
) {
    let config = load_config(&paths.config_file()).unwrap_or_default();
    let registry = AdapterRegistry::new(&config);
    let Some((adapter, _)) = registry.resolve(&provider) else {
        return;
    };
    let mut source_sequence = 0_u64;
    loop {
        match source.poll() {
            Ok(values) => {
                for value in values {
                    source_sequence = source_sequence.saturating_add(1);
                    let evidence = EventEvidence {
                        channel: EvidenceChannel::SideChannel,
                        trust: EvidenceTrust::LocalObservation,
                        collector_revision: Some(ADAPTER_API_VERSION.into()),
                        collector_instance_id: Some(invocation_id.to_string()),
                        source_sequence: Some(source_sequence),
                    };
                    let context = NormalizeContext {
                        workspace: Some(&workspace),
                    };
                    if let Ok(Some(mut normalized)) = adapter
                        .normalize_with_evidence(&invocation_id, &value, evidence, &context)
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
                                collection_context: normalized.collection_context,
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

/// Makes `group` the terminal's foreground process group with SIGTTOU blocked.
///
/// Once the provider owns the terminal, the wrapper's own group is in the
/// background. A background `tcsetpgrp` raises SIGTTOU on the caller's whole
/// process group, which would stop the wrapper and any parent sharing that
/// group, such as a script that launched it. Blocking the signal for the call,
/// as job-control shells do, lets the kernel apply the change instead.
fn set_terminal_foreground(tty: &std::fs::File, group: nix::unistd::Pid) -> nix::Result<()> {
    use nix::sys::signal::{SigSet, SigmaskHow, Signal, pthread_sigmask};
    let mut ttou = SigSet::empty();
    ttou.add(Signal::SIGTTOU);
    let mut previous = SigSet::empty();
    pthread_sigmask(SigmaskHow::SIG_BLOCK, Some(&ttou), Some(&mut previous))?;
    let result = nix::unistd::tcsetpgrp(tty, group);
    let _ = pthread_sigmask(SigmaskHow::SIG_SETMASK, Some(&previous), None);
    result
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
    let config = load_config(&paths.config_file()).unwrap_or_default();
    let registry = AdapterRegistry::new(&config);
    let Some((adapter, _)) = registry.resolve(provider) else {
        return Ok(());
    };
    inspect_hook_best_effort(paths, provider, &raw).await;
    let Ok(value) = serde_json::from_slice(&raw) else {
        return Ok(());
    };
    let workspace = env::var_os("SESSIONTAP_WORKSPACE").map(PathBuf::from);
    let Ok(Some(mut normalized)) = adapter
        .normalize_with_evidence(
            &uuid,
            &value,
            EventEvidence::managed_hook(ADAPTER_API_VERSION.into()),
            &NormalizeContext {
                workspace: workspace.as_deref(),
            },
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
            collection_context: normalized.collection_context,
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
    fn usage_lists_registry_providers() {
        let help = parse(vec!["--help".into()].into_iter()).unwrap_err();
        assert_eq!(
            help.to_string(),
            "usage: sessiontap <claude|codex|pi|qwen> [provider arguments...] | status | listen | inspect-hooks | setup | doctor | hooks remove | completions <shell>"
        );
        let empty = parse(std::iter::empty()).unwrap_err();
        assert!(
            empty
                .to_string()
                .starts_with("usage: sessiontap <claude|codex|pi|qwen> [args...] |")
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

    fn temp_paths(temp: &tempfile::TempDir) -> AppPaths {
        AppPaths {
            config_dir: temp.path().join("config"),
            state_dir: temp.path().join("state"),
            data_dir: temp.path().join("data"),
            runtime_dir: temp.path().join("runtime"),
        }
    }

    #[tokio::test]
    async fn inspection_lock_allows_only_one_inspector() {
        let temp = tempfile::tempdir().unwrap();
        let paths = temp_paths(&temp);
        let (_socket, endpoint) = bind_inspection_endpoint(&paths).unwrap();
        let error = bind_inspection_endpoint(&paths).err().unwrap();
        assert!(error.to_string().contains("already running"));
        assert!(paths.hook_inspection_socket().exists());
        drop(endpoint);
        assert!(!paths.hook_inspection_socket().exists());
    }

    #[tokio::test]
    async fn live_inspection_socket_is_not_removed() {
        let temp = tempfile::tempdir().unwrap();
        let paths = temp_paths(&temp);
        prepare_private_dir(&paths.runtime_dir).unwrap();
        // A live endpoint whose owner does not hold the lock (for example a
        // lock file removed out from under it) must still not be displaced.
        let live = std::os::unix::net::UnixDatagram::bind(paths.hook_inspection_socket()).unwrap();
        assert!(bind_inspection_endpoint(&paths).is_err());
        let probe = std::os::unix::net::UnixDatagram::unbound().unwrap();
        probe.connect(paths.hook_inspection_socket()).unwrap();
        probe.send(b"still-live").unwrap();
        let mut buffer = [0_u8; 16];
        let size = live.recv(&mut buffer).unwrap();
        assert_eq!(&buffer[..size], b"still-live");
        // Once the owner is gone the stale file is replaced.
        drop(live);
        let (_socket, _endpoint) = bind_inspection_endpoint(&paths).unwrap();
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
        let paths = temp_paths(&temp);
        prepare_private_dir(&paths.runtime_dir).unwrap();
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
