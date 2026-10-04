# Hub guide

`sessiontap-hub` merges canonical state streams from multiple SessionTap
daemons into one persisted, live view. It exists for deployments where agents
run on both a host and an isolated NixOS container: each daemon stays the
authority for its own namespace, and both push normalized state to one hub on
the host. The hub performs transport idempotency and state materialization
only; provider normalization and semantic deduplication stay in `sessiontapd`.

```
host sessiontapd ──hub sink──▶ sessiontap-hub ◀──hub sink── sandbox sessiontapd
                                     │
                       sessiontap-hub listen (Quickshell, scripts)
```

## Running the hub

```sh
sessiontap-hub            # run the service (same as `sessiontap-hub run`)
sessiontap-hub listen     # merged snapshot, then JSONL updates
sessiontap-hub pair       # pair a remote device (see Remote access)
sessiontap-hub devices    # list paired devices
sessiontap-hub revoke <device>
sessiontap-hub forget <source_id> <invocation_id>
```

The service reads `$XDG_CONFIG_HOME/sessiontap-hub/config.yaml` (falling back
to `$HOME/.config/sessiontap-hub/config.yaml`). Configuration is versioned and
strict: unknown fields, unsupported versions, empty commands, and unknown
`changes` fields, statuses, or reasons are reported and the service refuses to
start rather than running a partial or broadened rule set. Field, status, and
reason names are parsed directly into the canonical public schema types, so the
hub accepts exactly the names a public view can carry; names are lowercase and
case-sensitive (`blocked`, not `Blocked`), and an error names the subscription
index and the accepted values.

```yaml
version: 1
listen: "127.0.0.1:8931"        # HTTP ingestion bind address
retention_days: 7               # stopped agents + accepted-event identities
max_body_bytes: 1048576         # largest accepted request body
max_concurrent_commands: 4      # subscription commands running at once
command_timeout_secs: 30        # a command running longer is killed
subscriptions: []
```

A hub that accepts sources from containers binds a non-loopback address and
must configure per-source tokens (see [Token authentication](#token-authentication)):

```yaml
version: 1
listen: "0.0.0.0:8931"
sources:
  host: { token_file: /run/keys/sessiontap-hub-host }
  sandbox: { token_file: /run/keys/sessiontap-hub-sandbox }
```

The database lives at `$XDG_STATE_HOME/sessiontap-hub/hub.sqlite3` (mode
0600). The merged live stream is served from a private Unix socket at
`$XDG_RUNTIME_DIR/sessiontap-hub/sessiontap-hub.sock`. On restart the hub
restores the merged view from SQLite before accepting consumers or updates.

## Configuring sources

Every daemon that delivers to a hub needs a stable `source_id` (and optionally
`source_name`) in `$XDG_CONFIG_HOME/sessiontap/config.toml`, plus one enabled
hub sink:

```toml
version = 1
source_id = "host"
source_name = "Host machine"

[sinks.hub]
type = "hub"
enabled = true
url = "http://127.0.0.1:8931/ingest"
# token_file = "/run/keys/sessiontap-hub-token"
timeout_ms = 3000
max_payload_bytes = 262144
```

A hub sink requires `source_id`; configuration validation fails without it.
Invocations are keyed by `(source_id, invocation_id)`, so the same invocation
UUID from two sources remains two distinct agents.

For a daemon inside a NixOS container delivering to a hub on the container
host, cleartext HTTP is still limited to loopback unless the host address is
explicitly trusted. Cleartext delivery to a trusted non-loopback address also
requires `token_env` or `token_file`; validation fails without one:

```toml
source_id = "sandbox"
source_name = "NixOS sandbox"

[sinks.hub]
type = "hub"
enabled = true
url = "http://10.233.0.1:8931/ingest"
trusted_addresses = ["10.233.0.1"]
token_file = "/run/keys/sessiontap-hub-sandbox"
```

`trusted_addresses` is an intentional deployment choice; SessionTap never
treats a non-loopback address as safe by default. HTTPS sinks are permitted
anywhere without a trusted list.

## Delivery semantics

- When a hub sink is enabled, the daemon first delivers a complete versioned
  source snapshot at a consistent revision, then incremental updates ordered
  after that revision. Updates committed before the snapshot are subsumed and
  not redelivered.
- If the hub has no baseline for a source (fresh hub, wiped storage, newly
  enabled sink), it answers updates with `409 snapshot_required` and the
  daemon re-establishes the snapshot automatically.
- Delivery is at-least-once. The hub deduplicates by `(source_id, delivery_id)`
  and acknowledges duplicates and stale revisions without changing state, so
  lost acknowledgements never double-apply or double-trigger scripts.
- Registration, child binding, normalized hook changes, lifecycle exit, and
  reconciliation are all sink-visible when they change public state.

Hub updates carry only the complete resulting `PublicAgentView` and typed
changed public fields. The hub never receives or interprets provider hooks,
internal events, lifecycle/activity state, credentials, process-control data,
or multiplexer metadata. Bounded blocked or stopped reasons and fields such as
cwd, repository paths, and session names remain potentially sensitive. Hub
sinks are trusted, operator-controlled observers of these selected fields.

## Ingestion responses

Sources `POST` canonical envelopes to the ingestion address. Every rejection
body is JSON with a structured `error` code:

| Status | `error` | Meaning |
| --- | --- | --- |
| 200 | — | `{"status": "applied" \| "duplicate" \| "stale"}` |
| 400 | `malformed_request` | Request line or headers cannot be parsed, or the connection ended early |
| 400 | `malformed_envelope` | Body is not a valid canonical envelope |
| 400 | `unsupported_schema_version` | Envelope schema version is not supported |
| 401 | `unauthorized` | Bearer token missing or matches no configured source token |
| 403 | `source_not_permitted` | Envelope source ID is not bound to the presented token |
| 405 | `method_not_allowed` | Not a `POST` (except `GET /health`) |
| 409 | `snapshot_required` | Hub has no baseline for this source |
| 411 | `length_required` | `content-length` missing or not a number |
| 413 | `payload_too_large` | Body exceeds `max_body_bytes` |
| 431 | `headers_too_large` | Request headers exceed 64 KiB |

Daemon hub sinks re-send a source snapshot on `409 snapshot_required`, retry
`401` and `403` with a diagnostic (a credential the operator must fix), treat
every other 4xx as a permanent rejection, and retry 5xx and transport failures.

`GET /health` needs no token and answers exactly `{"status":"ok"}`; it reveals
no stored state.

## Token authentication

The hub `sources` map binds each source ID to a private token file. A token
authorizes writes only for the source IDs whose token file yields it, so one
source can never write another source's agents. Sharing one token file across
several sources deliberately binds that token to all of them.

- The hub authenticates every ingestion `POST` before parsing its body. A
  missing bearer or one that matches no configured token gets
  `401 unauthorized`.
- A valid token whose envelope names a source ID outside its bindings
  (including a source with no `sources` entry) gets `403 source_not_permitted`.
- Rejected requests change no state, publish nothing, and run no subscription.
- Token files are read at request time, so rotating a token needs no restart.
  They must be regular files, not symlinks, with no group/other permissions; a
  file that fails these checks authorizes nothing.
- A `listen` address that is not loopback (including `0.0.0.0` and `::`)
  without `sources` is a configuration error naming the address. The hub then
  runs with defaults and never binds that address unauthenticated.
- Loopback ingestion without `sources` accepts any source without a token.

Daemons send the bearer from `token_env` or `token_file` on the hub sink. Keep a
sandbox's token file outside paths its agents can read where possible; a leaked
token can only write its own source.

## Routing: subscriptions and scripts

Subscriptions match normalized changes and run commands. Different match
fields combine with logical AND; values inside one field combine with OR. An
omitted field matches everything.

```yaml
version: 1
subscriptions:
  - name: waiting-notify
    match:
      sources: [sandbox]
      providers: [codex, claude]
      statuses: [blocked]
      reasons: [input, approval]
      repositories: [/home/me/projects/agents]
    changes: [status, reason]
    commands:
      - [notify-send, "Agent is waiting"]
      - [/home/me/bin/agent-page.sh]
  - name: completed-notify
    match:
      statuses: [stopped]
      reasons: [completed]
    changes: [status, reason]
    commands:
      - [notify-send, "Agent responded"]
```

`changes` compares the previously persisted state with the accepted resulting
state. Canonical field names are: `invocation_id`, `provider`, `status`,
`reason`, `cwd`, `created_at`, `updated_at`, `session`, `metadata`, `usage`, and
`repository`. A subscription with `changes: [status, reason]` runs when either field
materially changed; it does not run for unrelated enrichment such as a usage
update. A previously unknown invocation reports every canonical field as
changed.

Commands are argument arrays executed directly, without shell evaluation. An
argument containing spaces or metacharacters is passed as one literal process
argument. Subscriptions are evaluated only after an update is durably
accepted; rejected, stale, and transport-duplicate deliveries never invoke
commands, and each `(source_id, delivery_id)` runs matching subscriptions at most
once.

### Script input contract

Matching commands receive the accepted canonical envelope on stdin (the same
versioned update shape delivered by the source):

```json
{
  "type": "update",
  "schema_version": 1,
  "source_id": "sandbox",
  "delivery_id": "...",
  "revision": 42,
  "changed": ["status", "reason"],
  "view": { "invocation_id": "...", "provider": "codex", "status": "blocked", "reason": {"kind":"input","summary":"..."} }
}
```

Scalar conveniences are exported as environment variables. They are
conveniences, not an alternative schema — read stdin for anything richer:

| Variable | Meaning |
| --- | --- |
| `SESSIONTAP_SOURCE` | Source ID (`host`, `sandbox`, ...) |
| `SESSIONTAP_DELIVERY_ID` | Stable delivery identity |
| `SESSIONTAP_HUB_REVISION` | Hub revision of this accepted update |
| `SESSIONTAP_SOURCE_REVISION` | Source revision of this accepted update |
| `SESSIONTAP_PROVIDER` | Provider (`claude`, `codex`, `qwen`, ...) |
| `SESSIONTAP_STATUS` | Public status (`running`, `idle`, `blocked`, `stopped`) |
| `SESSIONTAP_INVOCATION_ID` | Invocation UUID |
| `SESSIONTAP_CHANGED` | Comma-separated changed canonical fields |
| `SESSIONTAP_SESSION_ID` / `SESSIONTAP_SESSION_NAME` | Provider session when known |
| `SESSIONTAP_REPOSITORY_ROOT` / `SESSIONTAP_REPOSITORY_BRANCH` | Repository when known |
| `SESSIONTAP_REASON_KIND` / `SESSIONTAP_REASON_SUMMARY` | Bounded compatible blocked or stopped reason when present |

Commands are bounded. At most `max_concurrent_commands` (default 4) run at once
across all deliveries; excess commands queue and run as slots free, and no
accepted delivery is dropped. Commands for one delivery run in configuration
order. A command still running after `command_timeout_secs` (default 30) is
killed, and the hub logs the command and delivery identity. Command duration
never delays the ingestion response.

Command failures and timeouts are logged to the hub's stderr and never reject
or redeliver an already accepted ingestion. Scripts that need stronger than best-effort
guarantees should persist their own idempotency keys keyed by
`SESSIONTAP_DELIVERY_ID` and `SESSIONTAP_SOURCE`.

## Consuming the merged stream

```sh
sessiontap-hub listen
```

The first line is the persisted merged baseline:

```json
{"type":"snapshot","hub_revision":57,"sources":[{"source_id":"host","display_name":"Host machine","revision":120}],"agents":[{"source_id":"host","view":{"invocation_id":"...","provider":"codex","status":"idle"}}]}
```

Each later line is one accepted update with the hub revision, source identity,
complete resulting public view, and changed public field names:

```json
{"type":"update","hub_revision":58,"source_id":"sandbox","delivery_id":"...","source_revision":42,"changed":["status","reason"],"view":{"invocation_id":"...","provider":"codex","status":"blocked","reason":{"kind":"input","summary":"..."}}}
```

Consumers receive updates strictly after their baseline revision; a reconnect
(after a hub or consumer restart) receives a fresh complete baseline first.
Notify on post-baseline updates only.

Reason filters accept exactly `input`, `approval`, `completed`, and `failed`.
A `stopped + completed` rule does not match lifecycle-only stopped views because
those contain no reason. Public stopped does not expose whether the supervised
process is alive; scripts that need completion notifications should match the
reason rather than treating every stopped view as a response.

### Migrating Quickshell from broker listen

The per-broker `sessiontap listen` provides the same public view model for a
single daemon. To migrate a Quickshell surface (for example
`SAgents.qml`) to the merged hub:

1. Run the hub on the host and configure both daemons with hub sinks.
2. Validate merged state: compare `sessiontap-hub listen` output against
   `sessiontap listen` on each daemon.
3. Replace the listener command (`sessiontap listen` → `sessiontap-hub
   listen`).
4. Replace the complete prior view with each update's `view`; use `changed`,
   `status`, and optional `reason` for notification decisions.
5. Rollback if needed: switch the command back to `sessiontap listen`; broker
   state and behavior are unaffected.

## Forgetting stopped agents

```sh
sessiontap-hub forget host 7f3c2a1e-...
```

`forget` asks the running service to delete one stopped agent from the merged
state. The hub records a tombstone for that source and invocation, increments
the hub revision, and sends every live listener (local and remote) a fresh
snapshot without the agent. Only agents whose status is `stopped` can be
forgotten; an unknown agent or any other status exits non-zero and changes
nothing.

While the tombstone exists, updates for that invocation are acknowledged with
`{"status":"suppressed"}` but are not stored, published, or routed to
subscriptions, and source snapshots leave the invocation out. A new run of the
same project has a new invocation ID and is ingested normally. Tombstones are
deleted after twice `retention_days`. Forgetting changes only hub state; nothing
is sent to the source daemon or the agent.

## Remote access

Paired devices (the SessionTap Android app) can observe the merged state and
forget stopped agents over the LAN or a tailnet. The channel is TLS 1.3 with
WebSocket framing and mutual authentication: the device pins the hub key from
the pairing QR code, and the hub pins each device's client certificate.
Without a `remote` section the hub opens no remote port.

```yaml
version: 1
remote:
  name: MacBook                                 # default: host name
  listen: ["100.64.0.7:8932", "192.168.1.20:8932"]
  advertise: ["macbook.tailnet.ts.net:8932"]    # extra QR endpoint hints
```

Every `remote.listen` entry must be a concrete IP and port. Wildcard addresses
(`0.0.0.0`, `::`), an empty list, and unparsable entries make the configuration
invalid, so the hub runs with defaults and no remote listener. An address that
fails to bind (for example a LAN address after switching networks) is logged
with its address and skipped; the other addresses keep serving.

On first remote start the hub creates an ECDSA P-256 identity at
`$XDG_STATE_HOME/sessiontap-hub/remote-identity.pem` (mode 0600) and reuses it
afterwards. The hub ID is the lowercase hex SHA-256 of its
SubjectPublicKeyInfo. Deleting the file creates a new identity with a new hub
ID, and every device must pair again.

If Tailscale's "block connections without VPN" (or another always-on VPN
lockdown) is enabled on the phone, the LAN endpoints are unreachable; the app
then needs the tailnet address.

### Pairing

```sh
sessiontap-hub pair                 # scopes read and manage
sessiontap-hub pair --scope read    # read-only device
```

`pair` asks the running service to open a pairing window and prints a QR code
with a countdown. The window lasts 120 seconds and allows one successful
pairing. A newer `pair` replaces an open window. The QR payload is JSON:

```json
{"v":1,"hub":"MacBook","id":"<hub_id>","ep":["100.64.0.7:8932","192.168.1.20:8932","macbook.tailnet.ts.net:8932"],"sc":["read","manage"],"s":"<base64url secret>","exp":1767225600}
```

`ep` lists every `remote.listen` address and then every `remote.advertise`
entry. `s` is a 32-byte secret; `exp` is the expiry in Unix seconds.

When a device proves the secret, the terminal shows its name and fingerprint
(the first 16 bytes of the SHA-256 of the device key, as four groups of eight
hex characters) and asks `Trust this device? [y/N]`. Compare the fingerprint
with the one the app shows. Only `y` stores the device. `pair` exits zero only
when a device was paired; expiry, rejection, too many failures, or a replaced
window exit non-zero. `pair` also fails when the service is not running or
remote access is not configured.

Pairing an already paired device key again updates its name and scopes.

Failed proofs are counted per window in three ways: at most 3 per client key,
at most 5 per peer address (an IPv6 address counts by its /64 prefix), and at
most 20 in total. A key or address that reaches its limit is locked out for
the rest of the window: its later `pair.complete` calls get `pairing_failed`
without the proof being checked, even after the window closed. A locked-out
client does not affect other clients, so the operator's device can still pair.
The 20th failed proof in total burns the window, and `pair` reports too many
failed attempts. Opening a new window clears every count.

### Devices and revocation

```sh
sessiontap-hub devices
sessiontap-hub revoke 3f9a
```

`devices` prints each device ID, name, scopes, pairing time, and last-seen
time. `revoke` takes a device ID or unique prefix, deletes the device, and
closes its open connections with WebSocket close code 4401. An ambiguous prefix
lists the matches and revokes nothing; both cases exit non-zero.

### Protocol

Connect with TLS 1.3 to an endpoint, present the device client certificate,
and accept the server only if the SHA-256 of its SubjectPublicKeyInfo equals
the hub ID. Then open a WebSocket (any path). All messages are JSON text
frames.

- Request: `{"id":1,"method":"hub.info","params":{}}` (`params` optional)
- Response: `{"id":1,"result":{...}}` or
  `{"id":1,"error":{"code":"forbidden","message":"..."}}`
- Stream event (no `id`): `{"event":"stream","data":<envelope>}`, where `data`
  is exactly one `sessiontap-hub listen` line.

| Method | Scope | Params | Result |
|---|---|---|---|
| `hub.info` | paired | none | `{"hub_id","hub_name","protocol":1,"scopes"}` |
| `listen` | `read` | none | `{}`, then stream events |
| `forget` | `manage` | `source_id`, `invocation_id` | `{"hub_revision"}` |
| `pair.begin` | none | none | `{"nonce"}` (base64url) |
| `pair.complete` | none | `name`, `mac` (base64url) | `{"device_id","hub_name"}` |

`listen` sends a complete snapshot, then each update after it, and a new
snapshot whenever the hub re-baselines (a source snapshot, a forget, or a
lagging consumer). Requests are still answered while a stream runs. One
connection carries at most one stream. The hub pings every 60 seconds and
closes a connection that missed the previous pong.

Pairing runs on a connection that presents the device certificate: call
`pair.begin`, then `pair.complete` with
`mac = HMAC-SHA256(secret, "sessiontap-pair-v1" || hub_spki || device_spki || nonce)`,
where both SPKIs are DER bytes and the secret and nonce are the decoded bytes.
The response waits for the operator. After success the same connection is
authenticated.

Error codes: `bad_request`, `unknown_method`, `unauthorized` (unknown or
absent client certificate), `forbidden` (missing scope), `not_found`,
`not_stopped`, `pairing_closed` (no open window), `pairing_failed` (bad proof,
no client certificate, or a locked-out client), `pairing_rejected`,
`rate_limited` (too many `pair.*` calls from this address), `busy` (too many
requests in flight on this connection), `internal`.

The hub checks the device's stored record and scopes on every request. A
device paired or re-paired on another connection takes effect on the next
request of every open connection with that key. After `revoke` returns, no
request from the revoked device changes state or gets a success response, and
its connections send nothing after the 4401 close.

### Connection limits

A connection is unauthenticated until its client certificate matches a stored
device or it completes pairing. These limits are fixed:

| Limit | Value |
|---|---|
| TLS handshake | 10 seconds from accept |
| WebSocket upgrade | 10 seconds from the TLS handshake |
| Unauthenticated connection lifetime | 30 seconds from accept (close code 1008); a valid proof waiting for the operator is exempt |
| Remote connections, all addresses | 64 |
| Unauthenticated connections | 16 |
| Unauthenticated connections per peer address | 4 (IPv6 grouped by /64, IPv4-mapped IPv6 as IPv4) |
| Incoming message or frame | 64 KiB |
| Requests in flight, unauthenticated | 1 |
| Requests in flight, paired | 8 (more get `busy`; an acknowledged `listen` does not count) |
| Outbound write | 30 seconds |
| `pair.begin` and `pair.complete` per peer address | burst of 5, one more every 2 seconds (more get `rate_limited` and count as no try) |

A connection over an admission cap is dropped right after accept, before TLS.
An oversized message closes the connection without an answer. An
unauthenticated connection that sends a request before the previous one is
answered is closed with code 1008, so unpaired clients must wait for each
answer. A connection whose outbound write does not finish in time is closed.
When accepting on a remote address fails (for example when the process is out
of file descriptors), the hub logs the error once, retries after 50 ms, doubling
up to 1 second, and keeps serving that address.

## Limits

The hub exposes no agent screen inspection, capture,
input, or command-control operations, locally or remotely. Remote observation,
device administration, and forgetting stopped agents change only hub state.
Source envelopes remain versioned and carry capability metadata reserved for a
future separately specified bidirectional transport.
