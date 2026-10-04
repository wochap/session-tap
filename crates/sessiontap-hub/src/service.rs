//! Shared hub service state and the unix socket request dispatcher.

use anyhow::Result;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use hmac::{Hmac, Mac};
use sessiontap_infra::json::write_json_line;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    net::IpAddr,
    sync::{Arc, Mutex, RwLock},
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
use crate::remote::AddressKey;
use crate::store::{Device, DeviceLookup, ForgetOutcome, HubStore};

/// Pairing MAC label; bumping it invalidates older clients.
pub const PAIR_LABEL: &[u8] = b"sessiontap-pair-v1";
pub const PAIR_TTL: Duration = Duration::from_secs(120);
/// Invalid MACs one client key may send per window.
pub const PAIR_TRIES_PER_CLIENT: u32 = 3;
/// Invalid MACs one peer address may send per window.
pub const PAIR_TRIES_PER_ADDRESS: u32 = 5;
/// Invalid MACs from anyone that burn the window.
pub const PAIR_TRIES_TOTAL: u32 = 20;
/// `pair.*` calls one peer address may make in a burst.
pub const PAIR_RATE_BURST: u32 = 5;
/// Time to regain one `pair.*` call.
pub const PAIR_RATE_REFILL: Duration = Duration::from_secs(2);
/// Rate limiter entries kept before full buckets are pruned.
const PAIR_RATE_MAX_ENTRIES: usize = 1024;
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
    events: mpsc::Sender<PairEvent>,
}

/// Invalid MACs since the last window opened. Kept after the window closes
/// so a locked-out client stays locked out until the next window.
#[derive(Default)]
struct PairFailures {
    by_spki: HashMap<[u8; 32], u32>,
    by_address: HashMap<AddressKey, u32>,
    total: u32,
}

#[derive(Default)]
struct PairingState {
    window: Option<PairingWindow>,
    failures: PairFailures,
}

/// Token bucket per peer address for `pair.*` calls.
pub struct PairRateLimiter {
    burst: f64,
    per_second: f64,
    buckets: Mutex<HashMap<AddressKey, Bucket>>,
}

struct Bucket {
    tokens: f64,
    updated: Instant,
}

impl PairRateLimiter {
    #[must_use]
    pub fn new(burst: u32, refill: Duration) -> Self {
        Self {
            burst: f64::from(burst),
            per_second: 1.0 / refill.as_secs_f64(),
            buckets: Mutex::new(HashMap::new()),
        }
    }

    fn refilled(&self, bucket: &Bucket, now: Instant) -> f64 {
        let elapsed = now.saturating_duration_since(bucket.updated).as_secs_f64();
        (bucket.tokens + elapsed * self.per_second).min(self.burst)
    }

    /// Takes one call from the address's bucket. Fails closed when the
    /// table is full of addresses that are still limited.
    pub fn allow(&self, key: AddressKey, now: Instant) -> bool {
        let mut buckets = self.buckets.lock().expect("rate mutex poisoned");
        if !buckets.contains_key(&key) && buckets.len() >= PAIR_RATE_MAX_ENTRIES {
            buckets.retain(|_, bucket| self.refilled(bucket, now) < self.burst);
            if buckets.len() >= PAIR_RATE_MAX_ENTRIES {
                return false;
            }
        }
        let bucket = buckets.entry(key).or_insert(Bucket {
            tokens: self.burst,
            updated: now,
        });
        let tokens = self.refilled(bucket, now);
        bucket.updated = now;
        if tokens >= 1.0 {
            bucket.tokens = tokens - 1.0;
            true
        } else {
            bucket.tokens = tokens;
            false
        }
    }
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
    /// This client key or address used up its tries; the MAC was not checked.
    Locked,
}

pub struct Hub {
    pub store: Arc<HubStore>,
    pub updates: broadcast::Sender<HubPublication>,
    pub remote: Option<RemoteInfo>,
    pair_ttl: Duration,
    connections: Mutex<HashMap<String, Vec<CancellationToken>>>,
    pairing: Mutex<PairingState>,
    generation: Mutex<u64>,
    pair_rate: PairRateLimiter,
    /// Revocation takes it for writing; remote requests that act for a
    /// device hold it for reading from the device re-read to the action.
    pub device_gate: RwLock<()>,
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
            pairing: Mutex::new(PairingState::default()),
            generation: Mutex::new(0),
            pair_rate: PairRateLimiter::new(PAIR_RATE_BURST, PAIR_RATE_REFILL),
            device_gate: RwLock::new(()),
        }
    }

    /// Replaces the `pair.*` rate limit, for tests.
    #[doc(hidden)]
    #[must_use]
    pub fn with_pair_rate(mut self, burst: u32, refill: Duration) -> Self {
        self.pair_rate = PairRateLimiter::new(burst, refill);
        self
    }

    /// Takes one `pair.*` call from the peer address's budget.
    pub fn allow_pair_call(&self, peer: IpAddr) -> bool {
        self.pair_rate.allow(AddressKey::from(peer), Instant::now())
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
    /// Once it returns, no request from the device changes state.
    pub fn revoke(&self, prefix: &str) -> Result<DeviceLookup> {
        let _gate = self.device_gate.write().expect("device gate poisoned");
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
        let previous = {
            let mut pairing = self.pairing.lock().expect("pairing mutex poisoned");
            pairing.failures = PairFailures::default();
            pairing.window.replace(PairingWindow {
                generation,
                secret,
                scopes,
                expires,
                events,
            })
        };
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
            .window
            .as_ref()
            .map(|window| window.secret.to_vec())
    }

    /// Closes the window if it is still the given generation.
    pub fn close_pairing(&self, generation: u64) {
        let mut pairing = self.pairing.lock().expect("pairing mutex poisoned");
        if pairing
            .window
            .as_ref()
            .is_some_and(|window| window.generation == generation)
        {
            pairing.window = None;
        }
    }

    /// Checks a pairing MAC. Locked-out clients are refused before the
    /// window is looked at. A valid MAC consumes the window; invalid MACs
    /// count per client key, per address, and in total, and the total limit
    /// burns the window.
    pub fn attempt_pairing(
        &self,
        device_spki: &[u8],
        peer: IpAddr,
        nonce: &[u8],
        mac: &[u8],
    ) -> PairAttempt {
        let Some(remote) = &self.remote else {
            return PairAttempt::Closed;
        };
        let spki_key: [u8; 32] = Sha256::digest(device_spki).into();
        let address = AddressKey::from(peer);
        let mut pairing = self.pairing.lock().expect("pairing mutex poisoned");
        let failures = &pairing.failures;
        if failures.by_spki.get(&spki_key).copied().unwrap_or(0) >= PAIR_TRIES_PER_CLIENT
            || failures.by_address.get(&address).copied().unwrap_or(0) >= PAIR_TRIES_PER_ADDRESS
        {
            return PairAttempt::Locked;
        }
        let Some(window) = pairing.window.as_ref() else {
            return PairAttempt::Closed;
        };
        if Instant::now() >= window.expires {
            pairing.window = None;
            return PairAttempt::Closed;
        }
        if verify_pair_mac(&window.secret, &remote.hub_spki, device_spki, nonce, mac) {
            let window = pairing.window.take().expect("window checked above");
            return PairAttempt::Claimed {
                scopes: window.scopes,
                events: window.events,
            };
        }
        let failures = &mut pairing.failures;
        *failures.by_spki.entry(spki_key).or_default() += 1;
        *failures.by_address.entry(address).or_default() += 1;
        failures.total += 1;
        if failures.total >= PAIR_TRIES_TOTAL {
            let window = pairing.window.take().expect("window checked above");
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

    fn addr(last: u8) -> IpAddr {
        IpAddr::from([10, 0, 0, last])
    }

    #[tokio::test]
    async fn window_expires() {
        let hub = hub(Duration::from_millis(10));
        let (_, secret, _, _events) = hub.open_pairing(vec!["read".into()]);
        tokio::time::sleep(Duration::from_millis(20)).await;
        let mac = pair_mac(&secret, b"hub-spki", b"dev", b"n");
        assert!(matches!(
            hub.attempt_pairing(b"dev", addr(1), b"n", &mac),
            PairAttempt::Closed
        ));
    }

    #[tokio::test]
    async fn client_key_locks_out_after_three_failures() {
        let hub = hub(PAIR_TTL);
        let (_, secret, _, mut events) = hub.open_pairing(vec!["read".into()]);
        for last in 0..3 {
            assert!(matches!(
                hub.attempt_pairing(b"dev", addr(last), b"n", b"bad"),
                PairAttempt::Failed
            ));
        }
        let mac = pair_mac(&secret, b"hub-spki", b"dev", b"n");
        assert!(matches!(
            hub.attempt_pairing(b"dev", addr(9), b"n", &mac),
            PairAttempt::Locked
        ));
        assert!(events.try_recv().is_err());
        // the window stays open for everyone else
        let mac = pair_mac(&secret, b"hub-spki", b"other", b"n");
        assert!(matches!(
            hub.attempt_pairing(b"other", addr(9), b"n", &mac),
            PairAttempt::Claimed { .. }
        ));
    }

    #[tokio::test]
    async fn address_locks_out_after_five_failures_across_keys() {
        let hub = hub(PAIR_TTL);
        let (_, secret, _, _events) = hub.open_pairing(vec!["read".into()]);
        for key in 0..5u8 {
            assert!(matches!(
                hub.attempt_pairing(&[key], addr(1), b"n", b"bad"),
                PairAttempt::Failed
            ));
        }
        let mac = pair_mac(&secret, b"hub-spki", b"fresh", b"n");
        assert!(matches!(
            hub.attempt_pairing(b"fresh", addr(1), b"n", &mac),
            PairAttempt::Locked
        ));
        assert!(matches!(
            hub.attempt_pairing(b"fresh", addr(2), b"n", &mac),
            PairAttempt::Claimed { .. }
        ));
    }

    #[tokio::test]
    async fn twenty_failures_burn_the_window() {
        let hub = hub(PAIR_TTL);
        let (_, secret, _, mut events) = hub.open_pairing(vec!["read".into()]);
        for attempt in 0..PAIR_TRIES_TOTAL {
            let key = u8::try_from(attempt).unwrap();
            let address = addr(key / 5);
            assert!(matches!(
                hub.attempt_pairing(&[key], address, b"n", b"bad"),
                PairAttempt::Failed
            ));
            if attempt + 1 < PAIR_TRIES_TOTAL {
                assert!(events.try_recv().is_err());
            }
        }
        assert!(
            matches!(events.try_recv(), Ok(PairEvent::Failed(reason)) if reason == "too many failed pairing attempts")
        );
        let mac = pair_mac(&secret, b"hub-spki", b"dev", b"n");
        assert!(matches!(
            hub.attempt_pairing(b"dev", addr(200), b"n", &mac),
            PairAttempt::Closed
        ));
    }

    #[tokio::test]
    async fn locked_client_stays_locked_without_a_window_until_a_new_one() {
        let hub = hub(PAIR_TTL);
        let (generation, _, _, _events) = hub.open_pairing(vec!["read".into()]);
        for _ in 0..3 {
            hub.attempt_pairing(b"dev", addr(1), b"n", b"bad");
        }
        hub.close_pairing(generation);
        assert!(matches!(
            hub.attempt_pairing(b"dev", addr(1), b"n", b"bad"),
            PairAttempt::Locked
        ));
        assert!(matches!(
            hub.attempt_pairing(b"other", addr(2), b"n", b"bad"),
            PairAttempt::Closed
        ));
        let (_, secret, _, _events) = hub.open_pairing(vec!["read".into()]);
        let mac = pair_mac(&secret, b"hub-spki", b"dev", b"n");
        assert!(matches!(
            hub.attempt_pairing(b"dev", addr(1), b"n", &mac),
            PairAttempt::Claimed { .. }
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
            hub.attempt_pairing(b"dev", addr(1), b"n", &old_mac),
            PairAttempt::Failed
        ));
        let mac = pair_mac(&secret, b"hub-spki", b"dev", b"n");
        assert!(matches!(
            hub.attempt_pairing(b"dev", addr(1), b"n", &mac),
            PairAttempt::Claimed { scopes, .. } if scopes == vec!["manage".to_owned()]
        ));
        assert!(matches!(
            hub.attempt_pairing(b"dev", addr(1), b"n", &mac),
            PairAttempt::Closed
        ));
    }

    #[test]
    fn rate_limiter_refills_and_prunes() {
        let limiter = PairRateLimiter::new(5, Duration::from_secs(2));
        let start = Instant::now();
        let key = AddressKey::from(addr(1));
        for _ in 0..5 {
            assert!(limiter.allow(key, start));
        }
        assert!(!limiter.allow(key, start));
        assert!(!limiter.allow(key, start + Duration::from_secs(1)));
        assert!(limiter.allow(key, start + Duration::from_secs(3)));
        assert!(!limiter.allow(key, start + Duration::from_secs(3)));
        // other addresses have their own bucket
        assert!(limiter.allow(AddressKey::from(addr(2)), start));

        let limiter = PairRateLimiter::new(1, Duration::from_secs(2));
        let fill = |limiter: &PairRateLimiter| {
            for n in 0..PAIR_RATE_MAX_ENTRIES {
                let n = u32::try_from(n).unwrap();
                assert!(limiter.allow(AddressKey::from(IpAddr::from(n.to_be_bytes())), start));
            }
        };
        fill(&limiter);
        let newcomer = AddressKey::from(IpAddr::from([192, 168, 0, 1]));
        // every bucket is still empty: fail closed
        assert!(!limiter.allow(newcomer, start));
        // once they refill they are pruned and the newcomer gets a bucket
        assert!(limiter.allow(newcomer, start + Duration::from_secs(5)));
        assert!(limiter.buckets.lock().unwrap().len() == 1);
    }

    /// Shared with the Android app's `PinningTest`; both sides must agree.
    #[test]
    fn pairing_vector_matches_android() {
        let secret: Vec<u8> = (0..32).collect();
        let mac = pair_mac(&secret, b"hub-spki", b"device-spki", &[0xaa; 32]);
        assert_eq!(
            hex::encode(mac),
            "e7bfbc2f5bb0ccb26433c010b012b0d2cb34cf81b76b2f80c7328371011ff41f"
        );
        assert_eq!(
            fingerprint(b"device-spki"),
            "781b1751 7a877c9a 5199a93c 45e3a9b7"
        );
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
