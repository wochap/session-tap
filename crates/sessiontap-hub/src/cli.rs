//! Thin unix-socket clients behind the administrative subcommands.

use anyhow::{Context, Result, anyhow, bail};
use qrcode::{QrCode, render::unicode::Dense1x2};
use sessiontap_infra::json::write_json_line;
use std::{
    io::Write,
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncBufReadExt, BufReader, Lines},
    net::{UnixStream, unix::OwnedReadHalf},
};

use crate::config::{ListenMode, RemoteConfig};
use crate::listen::{HubRequest, HubResponse};
use crate::scope::Scope;
use crate::store::Device;

async fn connect(socket: &Path) -> Result<UnixStream> {
    UnixStream::connect(socket).await.with_context(|| {
        format!(
            "the hub service is not running (cannot connect to {})",
            socket.display()
        )
    })
}

/// Sends one request and returns its single response.
pub async fn request_once(socket: &Path, request: &HubRequest) -> Result<HubResponse> {
    let mut stream = connect(socket).await?;
    write_json_line(&mut stream, request).await?;
    let mut lines = BufReader::new(stream).lines();
    next_response(&mut lines).await
}

async fn next_response(
    lines: &mut Lines<BufReader<impl tokio::io::AsyncRead + Unpin>>,
) -> Result<HubResponse> {
    let line = lines
        .next_line()
        .await?
        .ok_or_else(|| anyhow!("the hub service closed the connection"))?;
    Ok(serde_json::from_str(&line)?)
}

fn fail(response: HubResponse) -> anyhow::Error {
    match response {
        HubResponse::Error {
            message, matches, ..
        } => {
            let mut text = message;
            for device in matches {
                text.push_str(&format!("\n  {} {}", device.device_id, device.name));
            }
            anyhow!(text)
        }
        other => anyhow!("unexpected hub response: {other:?}"),
    }
}

/// `sessiontap-hub devices`
pub async fn devices(socket: &Path, out: &mut impl Write) -> Result<()> {
    match request_once(socket, &HubRequest::Devices).await? {
        HubResponse::Devices { devices, control } => {
            write_devices(out, &devices, control)?;
            Ok(())
        }
        other => Err(fail(other)),
    }
}

/// Lists stored scopes; terminal scopes are marked `(disabled)` while
/// `remote.control` is off.
fn write_devices(out: &mut impl Write, devices: &[Device], control: bool) -> Result<()> {
    if devices.is_empty() {
        writeln!(out, "no paired devices")?;
        return Ok(());
    }
    writeln!(
        out,
        "{:<16}  {:<20}  {:<12}  {:<25}  LAST SEEN",
        "DEVICE", "NAME", "SCOPES", "PAIRED"
    )?;
    for device in devices {
        writeln!(
            out,
            "{:<16}  {:<20}  {:<12}  {:<25}  {}",
            device.device_id,
            device.name,
            scope_column(&device.scopes, control),
            device.paired_at,
            device.last_seen_at.as_deref().unwrap_or("never")
        )?;
    }
    Ok(())
}

fn scope_column(scopes: &[String], control: bool) -> String {
    scopes
        .iter()
        .map(|name| match Scope::parse(name) {
            Some(scope) if scope.is_terminal() && !control => format!("{name}(disabled)"),
            _ => name.clone(),
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// `sessiontap-hub revoke <device>`
pub async fn revoke(socket: &Path, device: &str, out: &mut impl Write) -> Result<()> {
    let request = HubRequest::Revoke {
        device: device.to_owned(),
    };
    match request_once(socket, &request).await? {
        HubResponse::Revoked { device } => {
            writeln!(out, "revoked {} ({})", device.device_id, device.name)?;
            Ok(())
        }
        other => Err(fail(other)),
    }
}

/// `sessiontap-hub forget <source_id> <invocation_id>`
pub async fn forget(
    socket: &Path,
    source_id: &str,
    invocation_id: &str,
    out: &mut impl Write,
) -> Result<()> {
    let request = HubRequest::Forget {
        source_id: source_id.to_owned(),
        invocation_id: invocation_id.to_owned(),
    };
    match request_once(socket, &request).await? {
        HubResponse::Forgotten { hub_revision } => {
            writeln!(
                out,
                "forgot {source_id}/{invocation_id} (hub revision {hub_revision})"
            )?;
            Ok(())
        }
        other => Err(fail(other)),
    }
}

/// Renders a QR payload as Unicode half blocks.
pub fn render_qr(payload: &str) -> Result<String> {
    let code = QrCode::new(payload.as_bytes())?;
    Ok(code.render::<Dense1x2>().quiet_zone(true).build())
}

/// `sessiontap-hub pair`: shows the QR code with a countdown, asks
/// `confirm(name, fingerprint)` when a device proves the secret, and
/// succeeds only when the device is stored.
pub async fn pair<F>(
    socket: &Path,
    scopes: Vec<String>,
    out: &mut (impl Write + Send),
    show_countdown: bool,
    confirm: F,
) -> Result<()>
where
    F: FnOnce(&str, &str) -> bool + Send + 'static,
{
    let mut stream = connect(socket).await?;
    write_json_line(&mut stream, &HubRequest::Pair { scopes }).await?;
    let (read, mut write) = stream.into_split();
    let mut lines: Lines<BufReader<OwnedReadHalf>> = BufReader::new(read).lines();
    let (payload, expires_at) = match next_response(&mut lines).await? {
        HubResponse::PairWindow {
            payload,
            expires_at,
        } => (payload, expires_at),
        other => return Err(fail(other)),
    };
    writeln!(out, "{}", render_qr(&payload)?)?;
    writeln!(out, "Scan this code with the SessionTap app.")?;
    out.flush()?;
    let mut ticker = tokio::time::interval(Duration::from_secs(1));
    let response = loop {
        tokio::select! {
            line = lines.next_line() => {
                let line = line?.ok_or_else(|| anyhow!("the hub service closed the connection"))?;
                break serde_json::from_str::<HubResponse>(&line)?;
            }
            _ = ticker.tick(), if show_countdown => {
                let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
                let left = u64::try_from(expires_at).unwrap_or(0).saturating_sub(now);
                write!(out, "\rexpires in {left:>3}s ")?;
                out.flush()?;
            }
        }
    };
    if show_countdown {
        writeln!(out)?;
    }
    let response = match response {
        HubResponse::PairConfirm { name, fingerprint } => {
            writeln!(out, "Device \"{name}\" wants to pair.")?;
            writeln!(out, "Fingerprint: {fingerprint}")?;
            writeln!(out, "Check that the app shows the same fingerprint.")?;
            out.flush()?;
            let accept = tokio::task::spawn_blocking(move || confirm(&name, &fingerprint)).await?;
            write_json_line(&mut write, &HubRequest::Accept { accept }).await?;
            next_response(&mut lines).await?
        }
        other => other,
    };
    match response {
        HubResponse::PairDone { device_id, name } => {
            writeln!(out, "paired {device_id} ({name})")?;
            Ok(())
        }
        HubResponse::PairFailed { reason } => bail!("{reason}"),
        other => Err(fail(other)),
    }
}

/// Printed by `pair` when a wildcard bind cannot be found after a network
/// move.
pub const DISCOVERY_HINT: &str = "Note: devices will not find this hub on a new network unless \
     remote.discovery is enabled or a tailnet endpoint is in remote.advertise.";

/// The `pair` hint for `remote`: shown for a wildcard bind with discovery
/// off.
#[must_use]
pub fn discovery_hint(remote: Option<&RemoteConfig>) -> Option<&'static str> {
    let remote = remote?;
    (matches!(remote.listen_mode(), ListenMode::Wildcard(_)) && !remote.discovery)
        .then_some(DISCOVERY_HINT)
}

/// Reads a `[y/N]` answer from the terminal.
#[must_use]
pub fn prompt_yes_no(question: &str) -> bool {
    eprint!("{question} [y/N] ");
    let _ = std::io::stderr().flush();
    let mut answer = String::new();
    if std::io::stdin().read_line(&mut answer).is_err() {
        return false;
    }
    matches!(answer.trim(), "y" | "Y" | "yes" | "Yes")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovery_hint_only_for_wildcard_without_discovery() {
        let remote = |listen: &str, discovery| RemoteConfig {
            name: None,
            listen: vec![listen.into()],
            advertise: Vec::new(),
            control: false,
            discovery,
        };
        assert_eq!(
            discovery_hint(Some(&remote("0.0.0.0:8932", false))),
            Some(DISCOVERY_HINT)
        );
        assert_eq!(
            discovery_hint(Some(&remote("[::]:8932", false))),
            Some(DISCOVERY_HINT)
        );
        assert_eq!(discovery_hint(Some(&remote("0.0.0.0:8932", true))), None);
        assert_eq!(
            discovery_hint(Some(&remote("192.168.1.20:8932", false))),
            None
        );
        assert_eq!(discovery_hint(None), None);
    }

    #[test]
    fn devices_mark_terminal_scopes_disabled_while_control_is_off() {
        let device = Device {
            device_id: "ab12".into(),
            spki_sha256: "spki".into(),
            name: "Pixel".into(),
            scopes: vec!["read".into(), "watch".into(), "control".into()],
            paired_at: "2026-01-01T00:00:00Z".into(),
            last_seen_at: None,
        };
        let render = |control| {
            let mut out = Vec::new();
            write_devices(&mut out, std::slice::from_ref(&device), control).unwrap();
            String::from_utf8(out).unwrap()
        };
        assert!(render(false).contains("read,watch(disabled),control(disabled)"));
        assert!(render(true).contains("read,watch,control "));
    }
}
