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
   child sessions schedule no root collection. Send a burst of hooks for one
   provider session and verify one trailing-edge result, then overlap bursts
   across independent Claude, Codex, and Qwen sessions and verify isolation.
   Exercise compaction and confirm context can clear or decrease without
   resetting cumulative totals.
10. For Claude, configure a visible custom statusline before setup. Run setup,
    doctor, collection, and hook removal and verify the stanza remains unchanged
    and is never executed by SessionTap. Confirm context percentage is absent
    when the transcript supplies no verified denominator.

Record provider version, Linux distribution, Wayland compositor, date, and
pass/fail; never commit account data or raw event payloads.

## Android companion (phone only)

The emulator checks in `android/README.md` cover the app flows. These checks
need a real phone and a hub with a `remote` section:

1. Camera QR scan: run `sessiontap-hub pair`, scan the code from the app,
   compare fingerprints, answer `y`, and verify the hub appears as connected.
2. Tailnet endpoint over mobile data: turn Wi-Fi off, keep Tailscale on, and
   verify the Hubs screen shows the tailnet endpoint in use and sessions update.
3. Wi-Fi off/on reconnect: toggle Wi-Fi and verify the hub reconnects within a
   few seconds without restarting the app, and a block that happened meanwhile
   notifies.
4. OEM background kill recovery: swipe the app away, wait at least 30 minutes
   with the screen off, then trigger an approval prompt and verify the
   notification arrives. Record the phone model and Android version.
5. Terminal approval from the phone: pair with `sessiontap-hub pair --scope
   control`, start a real Claude Code session in tmux on the hub machine, and
   ask it for something that needs approval. Open the agent from the
   notification's "Open terminal" action (unlock first), verify the terminal
   shows the approval menu with chips 1-4, tap 1, and verify the agent proceeds
   and the status leaves "waiting for you".
6. Terminal end: with the terminal open, exit Claude Code on the desktop
   (`/exit`). Verify the pane stays dimmed with the "Agent exited" end card,
   no key bar or reply field, and "Copy last screen" puts the screen text on
   the clipboard.
7. Terminal reply with Claude vim mode: turn on vim mode in Claude Code
   (`/vim`), open its terminal from the phone, and tap "Reply to agent…".
   Verify the keyboard opens and stays open, the field keeps focus, and typed
   text appears. Clear the field, type `i`, long-press Send, and record whether
   Claude enters INSERT mode.
9. Direct keyboard with Claude vim mode: with vim mode on, open the agent's
   terminal from the phone and tap the keyboard toggle left of the reply
   field. Verify the soft keyboard opens with no suggestions and the strip
   reads "Typing to agent". Type `w` and verify the cursor moves one word,
   type `i` and verify INSERT mode, tap Esc on the key bar and verify NORMAL
   mode, then type `$` and verify the cursor jumps to the line end. Tap Ctrl,
   type `r`, and verify the strip briefly reads "Sent Ctrl+R" and Claude opens
   its history search. Hide the keyboard with the back gesture and verify the
   reply field returns. Record the keyboard app (for example Gboard or Samsung
   Keyboard).
8. Nerd Font prompt: with a shell prompt that uses Powerline separators and
   Nerd Font icons (for example starship), open its terminal and verify the
   icons render as glyphs, not boxes, and the text after them stays aligned.
