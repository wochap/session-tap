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

## Tailscale

- Put the machine's tailnet address in `remote.listen` (for example
  `100.64.0.7:8932`), or its MagicDNS name in `remote.advertise`, to reach the
  hub away from the LAN.
- With Tailscale's "block connections without VPN" enabled on the phone, LAN
  endpoints are unreachable; the app needs the tailnet endpoint.
- The app tries every endpoint from the QR code and prefers the last one that
  worked. The Hubs screen shows the endpoint in use.

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

Instrumented tests skip when `test-control.py` is not reachable.

`android/scripts/emulator.sh run <cmd>` boots the emulator, runs a command, and
shuts it down. Phone-only checks are listed in `docs/smoke-tests.md`.
