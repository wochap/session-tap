# Opt-in live provider smoke tests

These tests require provider accounts and are intentionally manual. Use a
temporary home in a Linux Wayland session, build with
`nix develop -c cargo build --workspace`, put both binaries on `PATH`, and run
`sessiontap setup <provider>`.

For each of `claude`, `codex`, and `qwen`:

1. Run `sessiontap <provider> --version` and compare exact provider arguments.
2. Launch the ordinary interactive TUI and verify input, rendering, resize,
   Ctrl-C, and exit behavior.
3. In another terminal, verify `sessiontap status` and `sessiontap listen` show the
   wrapped invocation and expected state changes.
4. With synthetic, non-sensitive prompts, run `sessiontap inspect-hooks` in a
   separate terminal and verify records appear only while it runs. Confirm the
   provider discriminator extraction documented in `docs/cli.md`, including an
   unknown event when the public provider contract offers a safe way to emit
   one. Redact any captured output before sharing it.
5. Launch the provider directly and verify it remains absent from both tracking
   and inspection.
6. For Codex, review/trust the hook with `/hooks`; verify an untrusted hook
   yields lifecycle-only degraded observation.
7. For Qwen, verify the ordinary TUI remains active while usage/session fields
   arrive from the private side channel, then repeat with a user `--json-file`
   and verify SessionTap does not override it.

Record provider version, Linux distribution, Wayland compositor, date, and
pass/fail; never commit account data or raw event payloads.
