# MVP acceptance record

Automated Linux/Wayland checks on 2026-08-25:

- `cargo test --workspace --all-features`: pass (44 passed; one opt-in real-tmux
  test ignored by the default suite and passed when run separately)
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`: pass
- `cargo fmt --all --check`: pass
- `nix build path:.# --print-build-logs`: pass (all three binaries installed)
- `cargo deny check licenses`: pass
- `cargo audit`: pass, no known RustSec vulnerability reported
- Isolated Linux daemon/CLI smoke: pass (daemon started explicitly; `status`
  empty snapshot, exact fake provider exit code 7, stopped snapshot, listen
  snapshot, graceful shutdown, and restart restoration). Daemon-absent
  coverage also verifies no client spawn, actionable observation-command
  failures, untracked provider argv/stdin/exit behavior, tracking-environment
  removal, and hook no-op behavior.
- Broker/sink hardening integration: pass (restart and activation locking,
  subscription races and reconnects, disconnect cleanup, transient HTTP retry
  with event-ID deduplication and acknowledgement, permissions/symlink matrix,
  concurrent sessions, burst hooks, slow listeners, and bounded outbox load)
- Linux-only scope audit: pass (macOS CI and compatibility fallbacks removed;
  no first-party Windows or X11 compatibility code found)
- Linux CI starts a headless Weston session before running formatting, lint, and
  workspace tests. This checkout has no Git remote configured from which to
  dispatch or inspect the hosted workflow.

Manual account-backed TUI smoke tests are not claimed. Follow
`docs/smoke-tests.md` from a Linux Wayland session and record results separately
before release.
