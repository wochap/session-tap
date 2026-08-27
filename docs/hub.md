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
```

The service reads `$XDG_CONFIG_HOME/sessiontap-hub/config.yaml` (falling back
to `$HOME/.config/sessiontap-hub/config.yaml`). Configuration is versioned and
strict: unknown fields, unsupported versions, empty commands, and unknown
`changes` fields are reported and the service refuses to start rather than
running a partial or broadened rule set.

```yaml
version: 1
listen: "127.0.0.1:8931"        # HTTP ingestion bind address
retention_days: 7               # stopped agents + accepted-event identities
# token_file: /run/keys/sessiontap-hub-token   # optional bearer token
subscriptions: []
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
explicitly trusted:

```toml
source_id = "sandbox"
source_name = "NixOS sandbox"

[sinks.hub]
type = "hub"
enabled = true
url = "http://10.233.0.1:8931/ingest"
trusted_addresses = ["10.233.0.1"]
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
- Delivery is at-least-once. The hub deduplicates by `(source_id, event_id)`
  and acknowledges duplicates and stale revisions without changing state, so
  lost acknowledgements never double-apply or double-trigger scripts.
- Registration, child binding, normalized hook changes, lifecycle exit, and
  reconciliation are all sink-visible when they change public state.

Hub updates carry the complete resulting invocation snapshot, normalized event
metadata with optional turn identity and categorical failure, and the current attention object
— an explicit `null` when attention was cleared. Raw hook bodies, transcripts,
prompts, tool inputs, and credentials never enter hub envelopes. Attention
summaries are bounded derived text (160 chars / 512 bytes); enabling a hub
sink is explicit disclosure of that summary to the receiver.

## Token authentication

Both sides may configure a bearer token. The hub reads its token from
`token_file` at request time; daemons support `token_env` or `token_file` on
the hub sink. Token files must be regular files, not symlinks, and have no
group/other permissions. When neither side configures a token, ingestion is
unauthenticated — acceptable for local deployments bound to trusted
interfaces.

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
      events: [waiting_input, waiting_approval]
      statuses: [blocked]
      lifecycles: [alive]
      repositories: [/home/me/projects/agents]
    changes: [status, attention]
    commands:
      - [notify-send, "Agent is waiting"]
      - [/home/me/bin/agent-page.sh]
```

`changes` compares the previously persisted state with the accepted resulting
state. Canonical field names are: `status`, `lifecycle`, `activity`, `usage`,
`provider_session`, `provider_metadata`, `repository`, `multiplexer`, `process`, `attention`. A
subscription with `changes: [status, attention]` runs when either field
materially changed; it does not run for unrelated enrichment such as a usage
update. A previously unknown invocation reports every canonical field as
changed.

Commands are argument arrays executed directly, without shell evaluation. An
argument containing spaces or metacharacters is passed as one literal process
argument. Subscriptions are evaluated only after an update is durably
accepted; rejected, stale, and transport-duplicate deliveries never invoke
commands, and each `(source_id, event_id)` runs matching subscriptions at most
once.

### Script input contract

Matching commands receive the accepted canonical envelope on stdin (the same
versioned update shape delivered by the source):

```json
{
  "type": "update",
  "schema_version": 1,
  "source_id": "sandbox",
  "event_id": "...",
  "revision": 42,
  "event": {"kind": "waiting_input", "observed_at": "...", "received_at": "..."},
  "snapshot": { "invocation_id": "...", "provider": "codex", "status": "blocked", "...": "..." },
  "attention": {"kind": "waiting_input", "context": {"summary": "...", "source": "question"}}
}
```

Scalar conveniences are exported as environment variables. They are
conveniences, not an alternative schema — read stdin for anything richer:

| Variable | Meaning |
| --- | --- |
| `SESSIONTAP_SOURCE` | Source ID (`host`, `sandbox`, ...) |
| `SESSIONTAP_EVENT_ID` | Stable delivery identity |
| `SESSIONTAP_HUB_REVISION` | Hub revision of this accepted update |
| `SESSIONTAP_EVENT` | Normalized event kind (`waiting_input`, ...) |
| `SESSIONTAP_PROVIDER` | Provider (`claude`, `codex`, `qwen`, ...) |
| `SESSIONTAP_STATUS` | Public status (`running`, `idle`, `blocked`, `stopped`) |
| `SESSIONTAP_LIFECYCLE` | `starting`, `alive`, `exited`, `lost` |
| `SESSIONTAP_INVOCATION_ID` | Invocation UUID |
| `SESSIONTAP_CHANGED` | Comma-separated changed canonical fields |
| `SESSIONTAP_SESSION_ID` / `SESSIONTAP_SESSION_NAME` | Provider session when known |
| `SESSIONTAP_REPOSITORY_ROOT` / `SESSIONTAP_REPOSITORY_BRANCH` | Repository when known |
| `SESSIONTAP_ATTENTION_KIND` / `SESSIONTAP_ATTENTION_SUMMARY` / `SESSIONTAP_ATTENTION_SOURCE` | Attention when present |

Command failures are logged to the hub's stderr and never reject or redeliver
an already accepted ingestion. Scripts that need stronger than best-effort
guarantees should persist their own idempotency keys keyed by
`SESSIONTAP_EVENT_ID` and `SESSIONTAP_SOURCE`.

## Consuming the merged stream

```sh
sessiontap-hub listen
```

The first line is the persisted merged baseline:

```json
{"type":"snapshot","hub_revision":57,"sources":[{"source_id":"host","display_name":"Host machine","revision":120},{"source_id":"sandbox","revision":88}],"invocations":[{"source_id":"host","snapshot":{"...":"..."},"attention":null}]}
```

Each later line is one accepted update with the hub revision, source identity,
canonical event, resulting agent state, explicit attention, and changed field
names:

```json
{"type":"update","hub_revision":58,"source_id":"sandbox","event_id":"...","event":{"kind":"waiting_input","observed_at":"...","received_at":"..."},"snapshot":{"...":"..."},"attention":{"kind":"waiting_input","context":{"summary":"...","source":"question"}},"changed":["status","activity","attention"]}
```

Consumers receive updates strictly after their baseline revision; a reconnect
(after a hub or consumer restart) receives a fresh complete baseline first.
Notify on post-baseline updates only.

### Migrating Quickshell from broker listen

The per-broker `sessiontap listen` remains available and unchanged for
single-daemon views. To migrate a Quickshell surface (for example
`SAgents.qml`) to the merged hub:

1. Run the hub on the host and configure both daemons with hub sinks.
2. Validate merged state: compare `sessiontap-hub listen` output against
   `sessiontap listen` on each daemon.
3. Replace the listener command (`sessiontap listen` → `sessiontap-hub
   listen`).
4. Interpret `attention: null` as clearing any prior attention banner, and
   use `changed` plus `status`/`activity` for notification decisions.
5. Rollback if needed: switch the command back to `sessiontap listen`; broker
   state and behavior are unaffected.

## Limits

The initial hub is one-way. It exposes no agent screen inspection, capture,
input, or command-control operations. Source envelopes remain versioned and
carry capability metadata reserved for a future separately specified
bidirectional transport.
