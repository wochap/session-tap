# Opt-in live provider smoke tests

These tests require provider accounts and are intentionally manual. Use a
temporary home in a Linux Wayland session, build with
`nix develop -c cargo build --workspace`, put both binaries on `PATH`, and run
`sessiontap setup <provider>`. Start `sessiontapd` in a separate terminal and
leave it running for the tracked-launch checks.

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
8. Stop `sessiontapd`, set `SESSIONTAPD` to a test executable that would leave a
   marker if run, and launch the wrapper again. Verify the marker is absent,
   stderr instructs you to start `sessiontapd`, the provider remains fully
   interactive with exact arguments/signals/exit status, and `status` and
   `listen` fail. If the shell has inherited `SESSIONTAP_INVOCATION_ID`,
   `SESSIONTAP_CREDENTIAL`, or `SESSIONTAP_PROVIDER`, verify the fallback
   provider does not receive them and its managed hooks emit nothing.
9. For every tracked root session, wait for `usage` and compare cumulative
   input/output with the documented artifact accounting. Verify direct and
   child sessions schedule no root collection. Exercise compaction and confirm
   context can clear or decrease without resetting cumulative totals.
10. For Claude, configure a visible custom statusline before setup. Verify it
    is relayed while tracked, untracked, and with the daemon stopped. Run doctor
    and removal and compare the restored stanza at the JSON-value level. Replace
    the stanza after setup and verify removal leaves the replacement untouched.

Record provider version, Linux distribution, Wayland compositor, date, and
pass/fail; never commit account data or raw event payloads.
