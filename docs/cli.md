# CLI guide

Build with `nix build` or enter the development environment with `nix develop`.
Keep `sessiontap` and `sessiontapd` together on `PATH`. Start `sessiontapd`
explicitly in a dedicated terminal or service before provider launches,
`status`, or `listen`; the client never starts it. The optional
`sessiontap-hub` service merges streams from multiple daemons; see
`docs/hub.md`.

```sh
sessiontap setup                 # merge all managed hooks
sessiontap doctor codex          # diagnose a provider integration
sessiontap hooks remove claude   # remove only SessionTap-owned entries
sessiontapd                      # required for tracking, status, and listen
sessiontap claude [args...]
sessiontap codex [args...]
sessiontap qwen [args...]
sessiontap status                # JSON array
sessiontap listen                # snapshot then JSONL updates
sessiontap inspect-hooks         # ephemeral raw managed-hook JSONL
sessiontap-hub                   # merged multi-source service
sessiontap-hub listen            # merged snapshot then JSONL updates
```

SessionTap option parsing ends at the provider token; every later argument is
passed as its original argv element. Custom commands are configured in
`$XDG_CONFIG_HOME/sessiontap/config.toml`:

```toml
version = 1
retention_days = 7

[adapters.company]
executable = "company-claude"
inherits = "claude"
```

The daemon uses a private Unix socket under `$XDG_RUNTIME_DIR/sessiontap` and a
SQLite database under `$XDG_STATE_HOME/sessiontap`. If it is unavailable or
tracking initialization fails, the provider launches untracked and inherited
`SESSIONTAP_INVOCATION_ID`, `SESSIONTAP_CREDENTIAL`, and
`SESSIONTAP_PROVIDER` values are removed. `status` and `listen` fail with an
instruction to start `sessiontapd`. Status fields are sanitized: no raw
prompt, transcript, terminal output, hook body, or tool input is stored.

## Inspecting raw hooks

Run `sessiontap inspect-hooks` in a foreground terminal, then exercise one or
more providers launched through SessionTap. While the inspector is active,
managed hooks emit records such as:

```json
{"provider":"claude","hook_type":"Stop","payload":{"hook_event_name":"Stop","future_field":{"value":true}}}
```

This command intentionally exposes complete, unnormalized hook input. It may
contain prompts, tool inputs, paths, credentials, and other sensitive values.
SessionTap prints a warning before accepting records. Terminal scrollback and
explicit shell redirection can retain output outside SessionTap, so reproduce
with non-sensitive inputs and redact captures before sharing.

The endpoint exists only for the command's lifetime in SessionTap's private
runtime directory. SessionTap never writes inspection records to its database,
state files, logs, configuration, broker protocol, status/listen output,
durable outbox, or configured sinks. It retains only a bounded output queue;
slow or overloaded output causes diagnostic records to be dropped with a
stderr notice rather than delaying provider hooks.

Valid input uses the public discriminator `hook_event_name`, `event_name`, or
`type`, in that order; absent or non-string discriminators produce a null
`hook_type`. Invalid JSON is represented losslessly as hex in a diagnostic
payload. Input larger than 32 KiB is identified by a lower-bound size and is
not truncated and presented as complete. Warnings and drop notices use stderr;
JSONL records alone use stdout.

`status` returns retained `PublicAgentView` values. `listen` begins with a
non-notifying `views` baseline and then emits complete `view` updates with a
stable `delivery_id` and deterministic non-empty `changed` public field set.
Internal lifecycle, activity, event kinds, and reducer bookkeeping are not
part of either observation protocol. Notify only on post-baseline updates,
never on initial or lag-recovery snapshots.

The optional current status reason is status-compatible: blocked views may use
`input` or `approval`; stopped views may use `completed` or `failed`. Selected
question, provider message, description, command, or final assistant text has
controls removed, whitespace collapsed, and is truncated to its first 100
Unicode characters. Approval summaries are `<tool> <description>`, falling
back to `<tool> <first 100 command characters>`; patch bodies and unrelated
arguments are never selected. Only the latest reason is stored, outside
normalized event history, and reliable work or explicit idle clears it.

`stopped` is a breaking semantic: it can mean a live provider finished or
failed a response, not only that its process exited. Match
`status: stopped` plus `reason.kind: completed` for evidence-backed response
notifications. A message-less completion or lifecycle-only exit has no reason.

When launched inside tmux, the daemon retains socket/session/window/pane data
only for local control. Public status, listen, sink, and hub payloads never
contain it. Capture and input revalidate the server and pane locally; input is
delivered through a tmux buffer without shell evaluation.

Sinks are disabled by default. A debug stdout sink writes on the daemon's
stdout, never the provider terminal stream. HTTP sinks require HTTPS except for
loopback development receivers:

```toml
[sinks.local]
type = "http"
enabled = true
url = "http://127.0.0.1:8787/events"
token_env = "SESSIONTAP_SINK_TOKEN"
timeout_ms = 3000
max_payload_bytes = 262144
```

Hub sinks deliver the canonical versioned source stream (snapshots and
updates) to a `sessiontap-hub` service and require a stable `source_id`; see
`docs/hub.md`. Cleartext HTTP is limited to loopback or the sink's explicitly
configured `trusted_addresses`:

```toml
source_id = "host"
source_name = "Host machine"

[sinks.hub]
type = "hub"
enabled = true
url = "http://127.0.0.1:8931/ingest"
token_file = "/run/keys/sessiontap-hub-token"
```

Failures are retried from the durable outbox. Each sink backlog is capped at
1,024 records so a persistently unavailable receiver cannot grow broker storage
without bound; local normalized state continues to commit after the cap is
reached. Credential files must not be symlinks and must have no group/other
permissions. Provider hooks fail open and produce no provider-visible output.
Credentials, process-control data, multiplexer fields, internal reducer state,
raw failures, complete prompts/transcripts/assistant messages, and arbitrary
provider fields never enter stdout or HTTP sink payloads. Configured sinks are
trusted by the single operator and may receive explicitly selected bounded
status summaries. Public cwd, repository paths, session names, metadata, usage,
and bounded reasons remain potentially sensitive observer data.

## Shell completions

`sessiontap completions zsh` prints the zsh completion script to stdout.

Nix package users get completions automatically: `_sessiontap`,
`_sessiontapd`, and `_sessiontap-hub` are installed under
`$out/share/zsh/site-functions/`, which NixOS adds to the completion search
path. The package installs both binaries but deliberately does not activate or
supervise a daemon; NixOS modules and other consumers must arrange
`sessiontapd` startup before invoking daemon-dependent commands.

Manual installation — copy the script into your `fpath`:

```sh
mkdir -p ~/.zsh/completions
sessiontap completions zsh > ~/.zsh/completions/_sessiontap
fpath=(~/.zsh/completions $fpath)
autoload -Uz compinit && compinit
```

Or source it directly (no `fpath` entry needed, but no caching either):

```sh
eval "$(sessiontap completions zsh)"
```

`completions` is a reserved token: a custom adapter named `completions` is
shadowed by the subcommand.
