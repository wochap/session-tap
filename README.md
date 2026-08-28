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
`sessiontap` and `sessiontapd` together on `PATH`. SessionTap never starts the
daemon itself: run `sessiontapd` directly or under your service manager before
launching tracked providers or using `status`/`listen`.

```sh
# 1. Install managed hooks (writes into each provider's config file)
sessiontap setup qwen        # or: setup claude / setup codex

# 2. Start the broker in a dedicated terminal or service
sessiontapd

# 3. Launch a tracked session (spawns the real provider binary)
sessiontap qwen [args...]

# 4. Inspect state from another terminal
sessiontap status            # JSON array of all invocations
sessiontap listen            # JSONL stream: snapshot, then updates
sessiontap inspect-hooks     # ephemeral raw managed-hook JSONL
```

If `sessiontapd` is unavailable, provider launches continue untracked with
their normal arguments, terminal behavior, signals, and exit status. The
wrapper prints an explicit-start diagnostic and removes inherited SessionTap
tracking context. `status` and `listen` instead fail because they have no
untracked fallback.

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
{"type":"snapshot","schema_version":1,"revision":41,"views":[ ... ]}
{"type":"update","schema_version":1,"revision":42,"delivery_id":"...","changed":["status","reason"],"view":{ ... }}
```

Each `PublicAgentView` contains only observer-facing normalized fields:

| key | type | meaning |
| --- | --- | --- |
| `invocation_id` | string (uuid) | unique wrapper invocation |
| `provider` | string | configured identity, including aliases |
| `status` | enum | `running` / `blocked` / `idle` / `stopped` |
| `reason` | object? | bounded blocked reason: `input` or `approval` |
| `cwd` | string | launch working directory |
| `created_at`, `updated_at` | RFC 3339 | timestamps |
| `session` | object? | sanitized `{id, name, start_reason}` |
| `metadata` | object? | sanitized `{model, effort, permission_mode, current_turn_id}` |
| `usage` | object? | optional verified `{input_tokens, output_tokens, context_tokens, context_window_percent}` |
| `repository` | object? | `{root, branch, head, dirty}` |

## Data handling

Public fields are sanitized: no credentials, executable arguments, process or
multiplexer identities, reducer state, raw prompt, transcript, hook body, or
tool input is serialized. Sinks are disabled by default; HTTP sinks require
HTTPS except loopback development receivers. See `docs/cli.md` for sink and
custom-adapter configuration, and `docs/smoke-tests.md` for live provider
testing.

Snapshot envelopes are non-notifying baselines. Updates contain the complete
resulting view and a deterministic non-empty set of changed public field paths.
Observer-facing fields such as cwd, repository paths, session names, and
bounded reasons can still be sensitive and require appropriate access control.

## Provenance

Clean-room MIT implementation based on public provider contracts. See
`docs/clean-room.md`.
