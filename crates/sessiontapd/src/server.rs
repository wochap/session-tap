//! Unix socket front end: daemon lock, private socket, and per-connection
//! request dispatch onto [`App`].

use crate::app::App;
use anyhow::{Context, Result};
use fs2::FileExt;
use sessiontap_core::{
    SCHEMA_VERSION,
    protocol::{ErrorEnvelope, Request, Response, StreamEnvelope},
};
use std::{
    fs::{self, File, OpenOptions},
    os::unix::fs::PermissionsExt,
    path::Path,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::broadcast,
};

/// Takes the exclusive daemon lock. The returned file holds the lock until
/// dropped.
pub fn acquire_daemon_lock(path: &Path) -> Result<File> {
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)?;
    lock.try_lock_exclusive()
        .context("sessiontapd is already running")?;
    Ok(lock)
}

/// Binds the control socket with owner-only permissions, replacing a stale
/// socket file but refusing to displace a live listener.
pub async fn bind_private_socket(path: &Path) -> Result<UnixListener> {
    if path.exists() {
        if UnixStream::connect(path).await.is_ok() {
            anyhow::bail!("sessiontapd is already listening");
        }
        fs::remove_file(path).context("remove stale socket")?;
    }
    let listener = UnixListener::bind(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

/// Serves one connection: a single request/response, or a listener stream.
pub async fn handle(stream: UnixStream, app: App) -> Result<()> {
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    let Some(line) = lines.next_line().await? else {
        return Ok(());
    };
    let request: Request = serde_json::from_str(&line)?;
    if matches!(request, Request::Listen) {
        let (revision, views, mut rx) = app.subscribe()?;
        write_json(
            &mut write,
            &StreamEnvelope::Snapshot {
                schema_version: SCHEMA_VERSION,
                revision,
                views,
            },
        )
        .await?;
        loop {
            tokio::select! {
                incoming = lines.next_line() => match incoming {
                    Ok(None) => break,
                    Ok(Some(_)) => anyhow::bail!("listener connection accepts only one request"),
                    Err(error) => return Err(error.into()),
                },
                received = rx.recv() => match received {
                    Ok(update) if update.revision > revision => {
                        write_json(
                            &mut write,
                            &StreamEnvelope::Update {
                                schema_version: SCHEMA_VERSION,
                                revision: update.revision,
                                delivery_id: update.delivery_id,
                                changed: update.changed,
                                view: Box::new(update.view),
                            },
                        )
                        .await?
                    }
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        let (r, views) = app.status()?;
                        write_json(
                            &mut write,
                            &StreamEnvelope::Snapshot {
                                schema_version: SCHEMA_VERSION,
                                revision: r,
                                views,
                            },
                        )
                        .await?;
                    }
                    Err(_) => break,
                }
            }
        }
        return Ok(());
    }
    let response = process(request, &app).unwrap_or_else(|e| {
        Response::Error(ErrorEnvelope {
            code: "request_failed".into(),
            message: e.to_string(),
        })
    });
    write_json(&mut write, &response).await
}

/// Dispatches one non-streaming request.
pub fn process(request: Request, app: &App) -> Result<Response> {
    Ok(match request {
        Request::Health => Response::Health {
            version: SCHEMA_VERSION,
        },
        Request::Status => {
            let (revision, views) = app.status()?;
            Response::Status { revision, views }
        }
        Request::Register {
            snapshot,
            credential,
        } => {
            app.register(*snapshot, &credential)?;
            Response::Ok
        }
        Request::BindChild {
            invocation_id,
            credential,
            child_pid,
            start_identity,
        } => {
            app.bind_child(&invocation_id, &credential, child_pid, start_identity)?;
            Response::Ok
        }
        Request::LifecycleExit {
            invocation_id,
            credential,
            exit_code,
            signal,
        } => {
            app.lifecycle_exit(&invocation_id, &credential, exit_code, signal)?;
            Response::Ok
        }
        Request::HookIngest {
            provider,
            invocation_id,
            credential,
            event,
            status_reason,
            collection_context,
        } => {
            app.ingest_hook(
                provider,
                invocation_id,
                credential,
                *event,
                status_reason,
                collection_context,
            )?;
            Response::Ok
        }
        Request::Capture { invocation_id } => Response::Captured {
            text: app.capture(&invocation_id)?,
        },
        Request::SendInput {
            invocation_id,
            text,
        } => {
            app.send_input(&invocation_id, &text)?;
            Response::Ok
        }
        Request::Listen => anyhow::bail!("listen is a streaming request"),
    })
}

async fn write_json<T: serde::Serialize>(
    write: &mut tokio::net::unix::OwnedWriteHalf,
    value: &T,
) -> Result<()> {
    write.write_all(&serde_json::to_vec(value)?).await?;
    write.write_all(b"\n").await?;
    Ok(())
}

/// Returns whether `pid` is alive and, when given, still has the recorded
/// start identity (guards against PID reuse).
#[must_use]
pub fn process_alive(pid: u32, identity: Option<&str>) -> bool {
    let path = format!("/proc/{pid}");
    if Path::new(&path).exists() {
        return identity.is_none_or(|expected| {
            fs::read_to_string(format!("/proc/{pid}/stat"))
                .ok()
                .and_then(|stat| {
                    stat.rsplit_once(')')?
                        .1
                        .split_whitespace()
                        .nth(19)
                        .map(str::to_owned)
                })
                .as_deref()
                == Some(expected)
        });
    }
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None).is_ok()
}
