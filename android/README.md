# SessionTap for Android

Companion app for `sessiontap-hub`. It pairs with one or more hubs over the
remote access channel (see `docs/hub.md`, "Remote access"), shows the merged
agent sessions, and notifies when an agent needs permission or input, or
finishes. It is sideloaded; there is no store build.

Requirements: Android 12 (API 31) or newer, and a hub with a `remote` section.

## Build

All tooling comes from the nix devShell (SDK platform 36, build-tools 36.0.0,
JDK 21, adb). From the repository root:

```bash
nix develop .#android
cd android
./gradlew assembleDebug        # app/build/outputs/apk/debug/app-debug.apk
./gradlew test lint            # unit tests and lint

# ./gradlew assembleRelease      # app/build/outputs/apk/release/app-release.apk
# adb install -r app/build/outputs/apk/release/app-release.apk
```

## Install

Enable USB debugging on the phone, connect it, and run:

```bash
adb install -r app/build/outputs/apk/debug/app-debug.apk
```

## Pair

1. On the hub machine, configure `remote.listen` (and optionally
   `remote.advertise`) in the hub config and restart the hub.
2. Run `sessiontap-hub pair`. It prints a QR code.
3. In the app, grant the requested permissions, tap "Scan QR", and scan.
4. Compare the fingerprint shown by the app with the one `pair` prints, and
   answer `y` on the hub.

The app keeps one foreground service connection per paired hub. Allow the
battery optimization exemption when asked, or Android may stop it.
To pair another hub, open Hubs and tap "Pair another hub" (or use the top bar menu).

## Terminal

With the `watch` scope (`sessiontap-hub pair --scope watch`) the session detail
offers "View terminal"; with `control` it offers "Open terminal" and the phone
can type into the agent. Session rows show a terminal icon when the agent's
terminal can be opened, and needs-attention notifications gain an "Open
terminal" action with `control`.

The terminal screen shows only the agent's tmux pane, at the desktop pane size;
it never resizes it. Portrait opens at 9sp and follows the cursor sideways; tap
the size chip or double-tap to fit the width, pinch up to 200%. The key bar
is a grid, two rows of seven keys by default: Esc, Tab, Shift+Tab, Up, Ctrl+C
(tap twice within 2.5 s), Paste, Backspace, then Ctrl, Alt, Left, Down, Right,
Space, Enter. Tap Ctrl or Alt to apply it to the next key, long-press to lock it
until tapped again. Paste fills the reply field; Send pastes the reply and
presses Enter, long-press Send pastes without Enter. The keyboard toggle left of
the reply field turns on direct mode: every key typed on the soft or hardware
keyboard goes straight to the agent, in order, for vim-style input such as
Claude vim mode (`w`, `i`, Esc, `$`) and Ctrl combinations such as Ctrl+R.
Tap the toggle again or hide the keyboard to leave. When the agent waits for
approval and answers digits, chips 1-4 send that digit. Input pauses while the
agent is not in front or the desktop scrolls the pane, and nothing typed is
sent after a reconnect until you tap Send.

The top bar ⋮ menu offers Fit to width, Copy visible screen, and Edit keys.
Edit keys (also from long-pressing any key) rearranges the key bar: up to 4 rows
of 7 keys, named keys including Home, End, PgUp, PgDn, Del, and F1-F12, any
single character, Ctrl, Alt, Ctrl+C, and Paste. The layout is stored on the
phone and applies to every agent and hub; Reset restores the default.

Phone check for direct mode: see "Direct keyboard with Claude vim mode" in
`docs/smoke-tests.md` (drive vim mode with `w`, `i`, Esc, `$`, and Ctrl+R).

## Tailscale

- Put the machine's tailnet address in `remote.listen` (for example
  `100.64.0.7:8932`), or its MagicDNS name in `remote.advertise`, to reach the
  hub away from the LAN.
- With Tailscale's "block connections without VPN" enabled on the phone, LAN
  endpoints are unreachable; the app needs the tailnet endpoint.
- The app tries every endpoint from the QR code and prefers the last one that
  worked. The Hubs screen shows the endpoint in use.

## Troubleshooting

`adb install` fails with `adb: protocol fault (couldn't read status): Connection
reset by peer`: the adb server is stuck. Restart it and retry:

```bash
adb kill-server
adb start-server
adb devices                    # phone must show as "device", not "unauthorized"
```

If it persists, replug the USB cable or toggle USB debugging on the phone.

## Emulator checks

```bash
nix develop .#android-emulator
android/scripts/test-hub.sh start          # isolated hub with fixture agents
TEST_HUB=2 android/scripts/test-hub.sh start   # second hub for the two-hub tests
android/scripts/test-control.py &          # lets instrumented tests drive the hubs
android/scripts/emulator.sh start
adb uninstall dev.sessiontap.android       # the flow test starts from a fresh install
(cd android && ./gradlew connectedDebugAndroidTest)
android/scripts/test-hub.sh install-link   # pair manually via the debug deep link
```

Instrumented tests skip when `test-control.py` is not reachable. The terminal
flow (`a13_terminal`) pairs with `test-hub.sh link control`, starts the terminal
fixture with `test-hub.sh terminal start` (needs `tmux`), answers the fake
agent's approval by typing `2` in direct mode, types a reply line key by key,
sends a latched Ctrl combination, adds a third key row in Edit keys and resets
it, ends it with `test-hub.sh terminal exit`,
and stops the fixture afterwards.

`android/scripts/emulator.sh run <cmd>` boots the emulator, runs a command, and
shuts it down. Phone-only checks are listed in `docs/smoke-tests.md`.
