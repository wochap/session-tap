# SessionTap

Local observability for explicitly launched coding-agent terminal sessions
(Claude Code, Codex, Qwen). It wraps the provider CLI, normalizes its hook and
side-channel events into a local broker, and exposes machine-readable session
state. Nothing is captured unless you launch the provider through the wrapper.

Target: Linux, Wayland.

## How it works

Three layers:

```
sessiontap <provider> [args]
        │
        ▼
┌───────────────────────┐
│ CLI wrapper           │  launches the real provider binary
│                       │  preserves TTY, argv, signals, exit code
│                       │  detects tmux, captures repo metadata
└──────────┬────────────┘
           │ registers invocation, sets SESSIONTAP_* env vars
           ▼
┌───────────────────────┐
│ Broker daemon         │  sessiontapd: Unix socket + SQLite
│ (sessiontapd)         │  normalizes hook events, tracks state
│                       │  serves status/listen, forwards to sinks
└──────────┬────────────┘
           │ optional HTTPS
           ▼
┌───────────────────────┐
│ Remote consumers      │  (optional) HTTP sinks
└───────────────────────┘
```

The broker daemon is required because hooks are short-lived processes, while
state tracking, SQLite writes, event ordering, listeners, and sink retries need
a persistent owner.

Provider lifecycle hooks are installed into each provider's config file by
`sessiontap setup`. Each hook runs `sessiontap hook emit <provider>`, which
reads the hook JSON on stdin and forwards a normalized event to the broker.

**Why the wrapper is required:** a hook only reports when the provider process
carries `SESSIONTAP_INVOCATION_ID` and `SESSIONTAP_CREDENTIAL`, which the
wrapper injects at launch. Launching the provider directly still fires the
hooks, but they exit silently and nothing is tracked. Hooks fail open and never
block or alter the provider.

## Usage

Build with `nix build` or `nix develop -c cargo build --workspace`. Keep
`sessiontap` and `sessiontapd` together on `PATH`.

```sh
# 1. Install managed hooks (writes into each provider's config file)
sessiontap setup qwen        # or: setup claude / setup codex

# 2. Launch a tracked session (spawns the real provider binary)
sessiontap qwen [args...]

# 3. Inspect state from another terminal
sessiontap status            # JSON array of all invocations
sessiontap listen            # JSONL stream: snapshot, then updates
sessiontap inspect-hooks     # ephemeral raw managed-hook JSONL
```

`sessiontap setup` is idempotent: it replaces previously installed SessionTap
hook entries and leaves your other hooks untouched. `sessiontap doctor
<provider>` checks hook health without writing; `sessiontap hooks remove
<provider>` strips only SessionTap-owned entries.

Everything after the provider token is forwarded to the provider verbatim, so
`sessiontap codex --help` shows Codex help.

## Stack

Rust workspace. `sessiontap` (CLI wrapper), `sessiontapd` (broker daemon),
`sessiontap-core` (domain + protocol), `sessiontap-adapters` (hook installers
and normalizers), `sessiontap-storage` (SQLite). tokio, SQLite in WAL mode, a
private Unix domain socket, and tmux detection.

## `sessiontap listen` schema

The stream is JSONL. The first line is a full snapshot; each later line is a
single-invocation update:

```json
{"type":"snapshot","schema_version":1,"revision":41,"invocations":[ ... ],"active_attention":{}}
{"type":"update","schema_version":1,"revision":42,"snapshot":{ ... },"event":{"kind":"completed"}}
```

Each invocation object:

| key | type | meaning |
| --- | --- | --- |
| `schema_version` | u32 | snapshot schema version |
| `revision` | u64 | broker-global monotonic revision |
| `invocation_id` | string (uuid) | unique wrapper invocation |
| `provider` | string | `claude` / `codex` / `qwen` |
| `executable` | string | resolved provider binary path |
| `args` | string[] | redacted argv (secrets removed) |
| `cwd` | string | launch working directory |
| `process` | object | `{wrapper_pid, child_pid, start_identity, exit_code, signal}` |
| `created_at`, `updated_at` | RFC 3339 | timestamps |
| `lifecycle` | enum | `starting` / `alive` / `exited` / `lost` |
| `activity` | enum | `unknown` / `idle` / `working` / `waiting_input` / `waiting_approval` |
| `status` | enum | derived public status (below) |
| `provider_session` | object? | `{id, name}` — the provider's own session id |
| `usage` | object? | `{input_tokens, output_tokens, context_tokens}` |
| `repository` | object? | `{root, branch, head, dirty}` |
| `multiplexer` | object? | tmux: `{backend, socket, server_pid, session_id, session_name, window_id, window_index, pane_id, pane_tty, pane_pid}` |
| `capabilities` | object | `{capture, send_input, usage}` |

`status` is derived from `lifecycle` + `activity`:

- `exited` / `lost` → `stopped`
- `waiting_input` / `waiting_approval` → `blocked`
- `working` → `running`
- otherwise → `idle`

## Data handling

Status fields are sanitized: no raw prompt, transcript, terminal output, hook
body, or tool input is stored. Sinks are disabled by default; HTTP sinks require
HTTPS except loopback development receivers. See `docs/cli.md` for sink and
custom-adapter configuration, and `docs/smoke-tests.md` for live provider
testing.

Snapshot envelopes are non-notifying baselines and include the current local
`active_attention` map. Notify only from later update envelopes. Every update
from a current daemon includes an effective `event` cause; older daemons may
omit it. These additions retain schema version 1, and consumers that only read
the existing `snapshot` field remain compatible.

## Provenance

Clean-room MIT implementation based on public provider contracts. See
`docs/clean-room.md`.
