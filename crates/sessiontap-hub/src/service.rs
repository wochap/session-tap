//! Shared hub service state and the unix socket request dispatcher.

use anyhow::Result;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use hmac::{Hmac, Mac};
use sessiontap_infra::json::write_json_line;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::{broadcast, mpsc, oneshot},
};
use tokio_util::sync::CancellationToken;

use crate::ingest::HubPublication;
use crate::listen::{HubRequest, HubResponse, serve_unix_stream};
use crate::store::{Device, DeviceLookup, ForgetOutcome, HubStore};

/// Pairing MAC label; bumping it invalidates older clients.
pub const PAIR_LABEL: &[u8] = b"sessiontap-pair-v1";
pub const PAIR_TTL: Duration = Duration::from_secs(120);
pub const PAIR_TRIES: u32 = 3;
pub const SCOPE_READ: &str = "read";
pub const SCOPE_MANAGE: &str = "manage";
pub const SCOPES: [&str; 2] = [SCOPE_READ, SCOPE_MANAGE];

/// Remote identity facts the service needs for pairing and `hub.info`.
#[derive(Debug, Clone)]
pub struct RemoteInfo {
    pub hub_id: String,
    pub hub_name: String,
    pub hub_spki: Vec<u8>,
    pub endpoints: Vec<String>,
}

/// Messages from the remote side to the `pair` conversation.
#[derive(Debug)]
pub enum PairEvent {
    /// A device proved the secret; the operator decides.
    Confirm {
        name: String,
        fingerprint: String,
        spki_sha256: String,
        reply: oneshot::Sender<Option<Device>>,
    },
    /// The window closed without a pairing.
    Failed(String),
}

struct PairingWindow {
    generation: u64,
    secret: [u8; 32],
    scopes: Vec<String>,
    expires: Instant,
    tries_left: u32,
    events: mpsc::Sender<PairEvent>,
}

/// Result of checking a pairing MAC against the open window.
#[derive(Debug)]
pub enum PairAttempt {
    /// The MAC verified and the window is consumed.
    Claimed {
        scopes: Vec<String>,
        events: mpsc::Sender<PairEvent>,
    },
    /// No open window, or it expired.
    Closed,
    /// The MAC did not verify.
    Failed,
}

pub struct Hub {
    pub store: Arc<HubStore>,
    pub updates: broadcast::Sender<HubPublication>,
    pub remote: Option<RemoteInfo>,
    pair_ttl: Duration,
    connections: Mutex<HashMap<String, Vec<CancellationToken>>>,
    pairing: Mutex<Option<PairingWindow>>,
    generation: Mutex<u64>,
}

impl Hub {
    #[must_use]
    pub fn new(
        store: Arc<HubStore>,
        updates: broadcast::Sender<HubPublication>,
        remote: Option<RemoteInfo>,
    ) -> Self {
        Self::with_pair_ttl(store, updates, remote, PAIR_TTL)
    }

    #[must_use]
    pub fn with_pair_ttl(
        store: Arc<HubStore>,
        updates: broadcast::Sender<HubPublication>,
        remote: Option<RemoteInfo>,
        pair_ttl: Duration,
    ) -> Self {
        Self {
            store,
            updates,
            remote,
            pair_ttl,
            connections: Mutex::new(HashMap::new()),
            pairing: Mutex::new(None),
            generation: Mutex::new(0),
        }
    }

    /// Forgets a stopped agent and re-baselines every listener.
    pub fn forget(&self, source_id: &str, invocation_id: &str) -> Result<ForgetOutcome> {
        let outcome = self.store.forget(source_id, invocation_id)?;
        if let ForgetOutcome::Forgotten { hub_revision } = outcome {
            let _ = self
                .updates
                .send(HubPublication::SnapshotApplied { hub_revision });
        }
        Ok(outcome)
    }

    /// Deletes a device and closes its live remote connections.
    pub fn revoke(&self, prefix: &str) -> Result<DeviceLookup> {
        let lookup = self.store.delete_device(prefix)?;
        if let DeviceLookup::Found(device) = &lookup {
            let tokens = self
                .connections
                .lock()
                .expect("connections mutex poisoned")
                .remove(&device.device_id)
                .unwrap_or_default();
            for token in tokens {
                token.cancel();
            }
        }
        Ok(lookup)
    }

    /// Tracks a live connection so revocation can close it.
    pub fn register_connection(&self, device_id: &str, token: CancellationToken) {
        let mut connections = self.connections.lock().expect("connections mutex poisoned");
        let tokens = connections.entry(device_id.to_owned()).or_default();
        tokens.retain(|token| !token.is_cancelled());
        tokens.push(token);
    }

    /// Drops a finished connection; its token must already be cancelled.
    pub fn unregister_connection(&self, device_id: &str) {
        let mut connections = self.connections.lock().expect("connections mutex poisoned");
        if let Some(tokens) = connections.get_mut(device_id) {
            tokens.retain(|known| !known.is_cancelled());
            if tokens.is_empty() {
                connections.remove(device_id);
            }
        }
    }

    /// Opens a pairing window, replacing (and failing) any open one.
    /// Returns the generation, secret, expiry, and the event receiver.
    pub fn open_pairing(
        &self,
        scopes: Vec<String>,
    ) -> (u64, [u8; 32], Instant, mpsc::Receiver<PairEvent>) {
        let generation = {
            let mut generation = self.generation.lock().expect("generation mutex poisoned");
            *generation += 1;
            *generation
        };
        let secret: [u8; 32] = rand::random();
        let expires = Instant::now() + self.pair_ttl;
        let (events, receiver) = mpsc::channel(4);
        let previous = self
            .pairing
            .lock()
            .expect("pairing mutex poisoned")
            .replace(PairingWindow {
                generation,
                secret,
                scopes,
                expires,
                tries_left: PAIR_TRIES,
                events,
            });
        if let Some(previous) = previous {
            let _ = previous.events.try_send(PairEvent::Failed(
                "replaced by a newer pairing window".into(),
            ));
        }
        (generation, secret, expires, receiver)
    }

    /// Secret of the open window, for in-process test devices.
    #[doc(hidden)]
    #[must_use]
    pub fn pairing_secret(&self) -> Option<Vec<u8>> {
        self.pairing
            .lock()
            .expect("pairing mutex poisoned")
            .as_ref()
            .map(|window| window.secret.to_vec())
    }

    /// Closes the window if it is still the given generation.
    pub fn close_pairing(&self, generation: u64) {
        let mut pairing = self.pairing.lock().expect("pairing mutex poisoned");
        if pairing
            .as_ref()
            .is_some_and(|window| window.generation == generation)
        {
            *pairing = None;
        }
    }

    /// Checks a pairing MAC. A valid MAC consumes the window; the third
    /// invalid one closes it.
    pub fn attempt_pairing(&self, device_spki: &[u8], nonce: &[u8], mac: &[u8]) -> PairAttempt {
        let Some(remote) = &self.remote else {
            return PairAttempt::Closed;
        };
        let mut pairing = self.pairing.lock().expect("pairing mutex poisoned");
        let Some(window) = pairing.as_mut() else {
            return PairAttempt::Closed;
        };
        if Instant::now() >= window.expires {
            *pairing = None;
            return PairAttempt::Closed;
        }
        if verify_pair_mac(&window.secret, &remote.hub_spki, device_spki, nonce, mac) {
            let window = pairing.take().expect("window checked above");
            return PairAttempt::Claimed {
                scopes: window.scopes,
                events: window.events,
            };
        }
        window.tries_left -= 1;
        if window.tries_left == 0 {
            let window = pairing.take().expect("window checked above");
            let _ = window
                .events
                .try_send(PairEvent::Failed("too many failed pairing attempts".into()));
        }
        PairAttempt::Failed
    }
}

/// HMAC-SHA256 over the label, hub SPKI, device SPKI, and nonce.
#[must_use]
pub fn pair_mac(secret: &[u8], hub_spki: &[u8], device_spki: &[u8], nonce: &[u8]) -> Vec<u8> {
    pair_hmac(secret, hub_spki, device_spki, nonce)
        .finalize()
        .into_bytes()
        .to_vec()
}

fn pair_hmac(secret: &[u8], hub_spki: &[u8], device_spki: &[u8], nonce: &[u8]) -> Hmac<Sha256> {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).expect("hmac accepts any key length");
    for part in [PAIR_LABEL, hub_spki, device_spki, nonce] {
        mac.update(part);
    }
    mac
}

fn verify_pair_mac(
    secret: &[u8],
    hub_spki: &[u8],
    device_spki: &[u8],
    nonce: &[u8],
    provided: &[u8],
) -> bool {
    pair_hmac(secret, hub_spki, device_spki, nonce)
        .verify_slice(provided)
        .is_ok()
}

/// Operator fingerprint: the first 16 bytes of `sha256(device SPKI)` as four
/// groups of eight hex characters.
#[must_use]
pub fn fingerprint(device_spki: &[u8]) -> String {
    let digest = Sha256::digest(device_spki);
    digest[..16]
        .chunks(4)
        .map(hex::encode)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Stable device ID derived from the device key hash.
#[must_use]
pub fn device_id(spki_sha256: &str) -> String {
    spki_sha256.chars().take(16).collect()
}

fn unix_seconds(at: Instant) -> i64 {
    let remaining = at.saturating_duration_since(Instant::now());
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    i64::try_from((now + remaining).as_secs()).unwrap_or(i64::MAX)
}

/// Accepts unix connections until the listener fails.
pub async fn serve_unix_listener(listener: UnixListener, hub: Arc<Hub>) -> Result<()> {
    loop {
        let (stream, _) = listener.accept().await?;
        let hub = Arc::clone(&hub);
        tokio::spawn(async move {
            if let Err(error) = serve_unix(stream, hub).await {
                eprintln!("sessiontap-hub: unix request failed: {error}");
            }
        });
    }
}

/// Serves one unix connection: reads one request and answers it.
pub async fn serve_unix(stream: UnixStream, hub: Arc<Hub>) -> Result<()> {
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    let Some(line) = lines.next_line().await? else {
        return Ok(());
    };
    let request: HubRequest = match serde_json::from_str(&line) {
        Ok(request) => request,
        Err(error) => {
            write_json_line(
                &mut write,
                &HubResponse::error("bad_request", error.to_string()),
            )
            .await?;
            return Ok(());
        }
    };
    let response = match request {
        HubRequest::Listen => {
            let receiver = hub.updates.subscribe();
            return serve_unix_stream(lines, write, Arc::clone(&hub.store), receiver).await;
        }
        HubRequest::Pair { scopes } => {
            return pair_conversation(&hub, scopes, &mut lines, &mut write).await;
        }
        HubRequest::Accept { .. } => {
            HubResponse::error("bad_request", "no pairing confirmation is pending")
        }
        HubRequest::Devices => HubResponse::Devices {
            devices: hub.store.devices()?,
        },
        HubRequest::Revoke { device } => match hub.revoke(&device)? {
            DeviceLookup::Found(device) => HubResponse::Revoked { device },
            DeviceLookup::NotFound => {
                HubResponse::error("not_found", format!("no device matches '{device}'"))
            }
            DeviceLookup::Ambiguous(matches) => HubResponse::Error {
                code: "ambiguous".into(),
                message: format!("'{device}' matches {} devices", matches.len()),
                matches,
            },
        },
        HubRequest::Forget {
            source_id,
            invocation_id,
        } => forget_response(&hub, &source_id, &invocation_id)?,
    };
    write_json_line(&mut write, &response).await?;
    Ok(())
}

fn forget_response(hub: &Hub, source_id: &str, invocation_id: &str) -> Result<HubResponse> {
    Ok(match hub.forget(source_id, invocation_id)? {
        ForgetOutcome::Forgotten { hub_revision } => HubResponse::Forgotten { hub_revision },
        ForgetOutcome::NotFound => {
            HubResponse::error("not_found", format!("no agent {source_id}/{invocation_id}"))
        }
        ForgetOutcome::NotStopped => HubResponse::error(
            "not_stopped",
            "only stopped agents can be forgotten".to_owned(),
        ),
    })
}

/// Validates requested scopes; an empty request means every scope.
pub fn normalize_scopes(requested: &[String]) -> Result<Vec<String>, String> {
    if requested.is_empty() {
        return Ok(SCOPES.iter().map(|scope| (*scope).to_owned()).collect());
    }
    if let Some(unknown) = requested
        .iter()
        .find(|scope| !SCOPES.contains(&scope.as_str()))
    {
        return Err(format!(
            "unknown scope '{unknown}' (expected read or manage)"
        ));
    }
    Ok(SCOPES
        .iter()
        .filter(|scope| requested.iter().any(|requested| requested == *scope))
        .map(|scope| (*scope).to_owned())
        .collect())
}

/// The `pair` conversation: open a window, send the QR payload, relay one
/// operator confirmation, and report the outcome.
async fn pair_conversation<R, W>(
    hub: &Hub,
    scopes: Vec<String>,
    lines: &mut tokio::io::Lines<BufReader<R>>,
    write: &mut W,
) -> Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let Some(remote) = &hub.remote else {
        write_json_line(
            write,
            &HubResponse::error(
                "remote_disabled",
                "remote access is not configured; add a remote section to the hub configuration",
            ),
        )
        .await?;
        return Ok(());
    };
    let scopes = match normalize_scopes(&scopes) {
        Ok(scopes) => scopes,
        Err(message) => {
            write_json_line(write, &HubResponse::error("bad_request", message)).await?;
            return Ok(());
        }
    };
    let (generation, secret, expires, mut events) = hub.open_pairing(scopes.clone());
    let expires_at = unix_seconds(expires);
    let payload = serde_json::json!({
        "v": 1,
        "hub": remote.hub_name,
        "id": remote.hub_id,
        "ep": remote.endpoints,
        "sc": scopes,
        "s": URL_SAFE_NO_PAD.encode(secret),
        "exp": expires_at,
    });
    write_json_line(
        write,
        &HubResponse::PairWindow {
            payload: payload.to_string(),
            expires_at,
        },
    )
    .await?;
    let event = tokio::select! {
        () = tokio::time::sleep_until(expires.into()) => None,
        event = events.recv() => event,
        // the operator went away: close the window
        _ = lines.next_line() => {
            hub.close_pairing(generation);
            return Ok(());
        }
    };
    let response = match event {
        None => {
            hub.close_pairing(generation);
            HubResponse::PairFailed {
                reason: "pairing expired".into(),
            }
        }
        Some(PairEvent::Failed(reason)) => HubResponse::PairFailed { reason },
        Some(PairEvent::Confirm {
            name,
            fingerprint,
            spki_sha256,
            reply,
        }) => {
            write_json_line(
                write,
                &HubResponse::PairConfirm {
                    name: name.clone(),
                    fingerprint,
                },
            )
            .await?;
            let accepted = match lines.next_line().await? {
                Some(line) => matches!(
                    serde_json::from_str(&line),
                    Ok(HubRequest::Accept { accept: true })
                ),
                None => false,
            };
            if accepted {
                let device = hub.store.upsert_device(
                    &device_id(&spki_sha256),
                    &spki_sha256,
                    &name,
                    &scopes,
                )?;
                let response = HubResponse::PairDone {
                    device_id: device.device_id.clone(),
                    name: device.name.clone(),
                };
                let _ = reply.send(Some(device));
                response
            } else {
                let _ = reply.send(None);
                HubResponse::PairFailed {
                    reason: "pairing rejected".into(),
                }
            }
        }
    };
    write_json_line(write, &response).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hub(ttl: Duration) -> Hub {
        let (updates, _) = broadcast::channel(8);
        Hub::with_pair_ttl(
            Arc::new(HubStore::memory().unwrap()),
            updates,
            Some(RemoteInfo {
                hub_id: "id".into(),
                hub_name: "hub".into(),
                hub_spki: b"hub-spki".to_vec(),
                endpoints: vec![],
            }),
            ttl,
        )
    }

    #[tokio::test]
    async fn window_expires() {
        let hub = hub(Duration::from_millis(10));
        let (_, secret, _, _events) = hub.open_pairing(vec!["read".into()]);
        tokio::time::sleep(Duration::from_millis(20)).await;
        let mac = pair_mac(&secret, b"hub-spki", b"dev", b"n");
        assert!(matches!(
            hub.attempt_pairing(b"dev", b"n", &mac),
            PairAttempt::Closed
        ));
    }

    #[tokio::test]
    async fn three_bad_macs_close_the_window() {
        let hub = hub(PAIR_TTL);
        let (_, secret, _, mut events) = hub.open_pairing(vec!["read".into()]);
        for _ in 0..2 {
            assert!(matches!(
                hub.attempt_pairing(b"dev", b"n", b"bad"),
                PairAttempt::Failed
            ));
        }
        assert!(events.try_recv().is_err());
        assert!(matches!(
            hub.attempt_pairing(b"dev", b"n", b"bad"),
            PairAttempt::Failed
        ));
        assert!(matches!(events.try_recv(), Ok(PairEvent::Failed(_))));
        let mac = pair_mac(&secret, b"hub-spki", b"dev", b"n");
        assert!(matches!(
            hub.attempt_pairing(b"dev", b"n", &mac),
            PairAttempt::Closed
        ));
    }

    #[tokio::test]
    async fn valid_mac_claims_once_and_newer_window_replaces() {
        let hub = hub(PAIR_TTL);
        let (_, old_secret, _, mut old_events) = hub.open_pairing(vec!["read".into()]);
        let (_, secret, _, _events) = hub.open_pairing(vec!["manage".into()]);
        assert!(matches!(old_events.try_recv(), Ok(PairEvent::Failed(_))));
        let old_mac = pair_mac(&old_secret, b"hub-spki", b"dev", b"n");
        assert!(matches!(
            hub.attempt_pairing(b"dev", b"n", &old_mac),
            PairAttempt::Failed
        ));
        let mac = pair_mac(&secret, b"hub-spki", b"dev", b"n");
        assert!(matches!(
            hub.attempt_pairing(b"dev", b"n", &mac),
            PairAttempt::Claimed { scopes, .. } if scopes == vec!["manage".to_owned()]
        ));
        assert!(matches!(
            hub.attempt_pairing(b"dev", b"n", &mac),
            PairAttempt::Closed
        ));
    }

    #[test]
    fn fingerprint_has_four_groups() {
        let fp = fingerprint(b"spki");
        assert_eq!(fp.split(' ').count(), 4);
        assert!(fp.split(' ').all(|group| group.len() == 8));
    }

    #[test]
    fn scopes_default_and_validate() {
        assert_eq!(normalize_scopes(&[]).unwrap(), vec!["read", "manage"]);
        assert_eq!(
            normalize_scopes(&["read".into()]).unwrap(),
            vec!["read".to_owned()]
        );
        assert!(normalize_scopes(&["write".into()]).is_err());
    }
}
