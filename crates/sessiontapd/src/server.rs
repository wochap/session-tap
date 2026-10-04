//! Unix socket front end: per-connection request dispatch onto [`App`].

use crate::{app::App, terminal::TerminalError};
use anyhow::Result;
use sessiontap_core::{
    SCHEMA_VERSION,
    protocol::{ErrorEnvelope, Request, Response, StreamEnvelope},
    terminal::{TerminalFrame, error_code},
};
use sessiontap_infra::{json::write_json_line, multiplexer::UnsupportedBackend};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    net::UnixStream,
    sync::broadcast,
};

/// Serves one connection: a single request/response, or a listener stream.
pub async fn handle(stream: UnixStream, app: App) -> Result<()> {
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    let Some(line) = lines.next_line().await? else {
        return Ok(());
    };
    let request: Request = match serde_json::from_str(&line) {
        Ok(request) => request,
        Err(error) => {
            let response = error_response(error_code::BAD_REQUEST, error.to_string());
            return Ok(write_json_line(&mut write, &response).await?);
        }
    };
    if let Request::TerminalWatch { invocation_id } = &request {
        let mut watcher = match app.terminal_watch(invocation_id) {
            Ok(watcher) => watcher,
            Err(error) => return Ok(write_json_line(&mut write, &error_to_response(&error)).await?),
        };
        loop {
            tokio::select! {
                incoming = lines.next_line() => match incoming {
                    Ok(None) => break,
                    Ok(Some(_)) => anyhow::bail!("watch connection accepts only one request"),
                    Err(error) => return Err(error.into()),
                },
                frame = watcher.next() => match frame {
                    Some(frame) => {
                        let ended = matches!(frame, TerminalFrame::Ended { .. });
                        write_json_line(&mut write, &frame).await?;
                        if ended {
                            break;
                        }
                    }
                    None => break,
                },
            }
        }
        return Ok(());
    }
    if matches!(request, Request::Listen) {
        let (revision, views, mut rx) = app.subscribe()?;
        write_json_line(
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
                        write_json_line(
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
                        write_json_line(
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
    let response = process(request, &app).unwrap_or_else(|error| error_to_response(&error));
    Ok(write_json_line(&mut write, &response).await?)
}

fn error_response(code: &str, message: String) -> Response {
    Response::Error(ErrorEnvelope {
        code: code.into(),
        message,
    })
}

/// Typed terminal and backend failures keep their code; anything else is
/// `request_failed`.
pub fn error_to_response(error: &anyhow::Error) -> Response {
    if let Some(terminal) = error.downcast_ref::<TerminalError>() {
        return error_response(terminal.code, terminal.message.clone());
    }
    if error.downcast_ref::<UnsupportedBackend>().is_some() {
        return error_response(error_code::UNSUPPORTED_BACKEND, error.to_string());
    }
    error_response("request_failed", error.to_string())
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
        Request::TerminalInput {
            invocation_id,
            input,
        } => {
            app.terminal_input(&invocation_id, &input)?;
            Response::Ok
        }
        Request::Listen | Request::TerminalWatch { .. } => {
            anyhow::bail!("streaming request on the request path")
        }
    })
}
