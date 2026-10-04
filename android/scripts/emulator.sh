#!/usr/bin/env bash
# Headless Android 36 emulator for device checks. Run inside `nix develop .#android-emulator`.
#   emulator.sh start        create the AVD if missing, boot it, wait for sys.boot_completed
#   emulator.sh stop         kill the running emulator
#   emulator.sh run <cmd>    start, run <cmd>, stop (also when <cmd> fails)
set -Eeuo pipefail

AVD=sessiontap-36
IMAGE="system-images;android-36;google_apis;x86_64"
export ANDROID_AVD_HOME=${ANDROID_AVD_HOME:-${XDG_CACHE_HOME:-$HOME/.cache}/sessiontap/avd}
LOG=${ANDROID_AVD_HOME}/emulator.log
BOOT_TIMEOUT=${BOOT_TIMEOUT:-300}

die() { echo "emulator.sh: $*" >&2; exit 1; }

check() {
  [[ -n ${ANDROID_HOME:-} ]] || die "ANDROID_HOME is unset; run inside nix develop .#android-emulator"
  [[ -r /dev/kvm && -w /dev/kvm ]] || die "/dev/kvm is not accessible"
  command -v emulator >/dev/null || die "emulator not found; use the android-emulator devShell"
}

create() {
  mkdir -p "$ANDROID_AVD_HOME"
  if ! avdmanager list avd -c 2>/dev/null | grep -qx "$AVD"; then
    echo "emulator.sh: creating AVD $AVD"
    echo no | avdmanager create avd --name "$AVD" --package "$IMAGE" --device pixel_8 --force >/dev/null
    # Enough room for the app and quick boots.
    printf 'hw.ramSize=4096\ndisk.dataPartition.size=6G\nhw.keyboard=yes\n' >>"$ANDROID_AVD_HOME/$AVD.avd/config.ini"
  fi
}

booted() {
  [[ $(adb -e shell getprop sys.boot_completed 2>/dev/null | tr -d '\r') == 1 ]]
}

start() {
  check
  create
  if adb devices | grep -q '^emulator-'; then
    echo "emulator.sh: an emulator is already running"
  else
    echo "emulator.sh: booting $AVD (log: $LOG)"
    nohup emulator -avd "$AVD" -no-window -no-audio -no-boot-anim -no-snapshot \
      -gpu swiftshader_indirect -camera-back none -camera-front none >"$LOG" 2>&1 &
  fi
  adb start-server >/dev/null
  local waited=0
  until booted; do
    sleep 2
    waited=$((waited + 2))
    if ((waited >= BOOT_TIMEOUT)); then
      tail -20 "$LOG" >&2 || true
      die "boot timed out after ${BOOT_TIMEOUT}s"
    fi
  done
  adb -e shell input keyevent 82 >/dev/null 2>&1 || true
  # Animations off keeps instrumented tests stable.
  for key in window_animation_scale transition_animation_scale animator_duration_scale; do
    adb -e shell settings put global "$key" 0 >/dev/null 2>&1 || true
  done
  echo "emulator.sh: booted"
}

stop() {
  if adb devices | grep -q '^emulator-'; then
    adb -e emu kill >/dev/null 2>&1 || true
    local waited=0
    while adb devices | grep -q '^emulator-' && ((waited < 30)); do
      sleep 1
      waited=$((waited + 1))
    done
  fi
  echo "emulator.sh: stopped"
}

case ${1-} in
  start) start ;;
  stop) stop ;;
  run)
    shift
    (($#)) || die "usage: emulator.sh run <command...>"
    start
    status=0
    "$@" || status=$?
    stop
    exit "$status"
    ;;
  *) die "usage: emulator.sh start|stop|run <command...>" ;;
esac
