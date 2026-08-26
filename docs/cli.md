# CLI guide

Build with `nix build` or enter the development environment with `nix develop`.
Keep `sessiontap` and `sessiontapd` together on `PATH`.

```sh
sessiontap setup                 # merge all managed hooks
sessiontap doctor codex          # diagnose a provider integration
sessiontap hooks remove claude   # remove only SessionTap-owned entries
sessiontap claude [args...]
sessiontap codex [args...]
sessiontap qwen [args...]
sessiontap status                # JSON array
sessiontap listen                # snapshot then JSONL updates
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
SQLite database under `$XDG_STATE_HOME/sessiontap`. If tracking initialization
fails, the provider launches untracked. Status fields are sanitized: no raw
prompt, transcript, terminal output, hook body, or tool input is stored.

When launched inside tmux, snapshots contain descriptive socket/session/window/
pane metadata. Capture and input are local-only capabilities; the broker must
resolve an invocation and revalidate the server and pane before control. Input
is delivered through a tmux buffer without shell evaluation.

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

Failures are retried from the durable outbox. Each sink backlog is capped at
1,024 records so a persistently unavailable receiver cannot grow broker storage
without bound; local normalized state continues to commit after the cap is
reached. Credential files must not be symlinks and must have no group/other
permissions. Provider hooks fail open and produce no provider-visible output.

## Shell completions

`sessiontap completions zsh` prints the zsh completion script to stdout.

Nix package users get completions automatically: both `_sessiontap` and
`_sessiontapd` are installed under `$out/share/zsh/site-functions/`, which
NixOS adds to the completion search path.

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
