#!/usr/bin/env bash
# Isolated sessiontap-hub for emulator checks. Run inside `nix develop .#android-emulator`.
#
#   test-hub.sh start               build and start the hub, post the fixture snapshot
#   test-hub.sh stop                stop the hub and delete its state
#   test-hub.sh post <agent> <state>  send one update (see FIXTURES below)
#   test-hub.sh snapshot            clear forget tombstones and re-send the fixture snapshot
#   test-hub.sh link [expired] [scope...]  print a debug pairing deep link from a fresh pair
#                                   window (scopes default to the hub default: read, manage)
#   test-hub.sh answer y|n          answer the pending pairing confirmation
#   test-hub.sh hub <args...>       run sessiontap-hub against this instance (listen, devices, revoke, ...)
#   test-hub.sh install-link        `link` and send it to the emulator with adb
#
# HUB_BIN=<path> HUB_BIN_PREBUILT=1 skips the cargo build.
# TEST_HUB=2 runs a second independent hub (TestHub2) on the next ports.
#
# FIXTURES: agents fix (running), devbox (blocked on approval), billing (child
# Explore blocked on approval), vite (stopped, completed), triage (stopped, no reason).
# States for `post`: running idle approval input child completed stopped failed.
set -Eeuo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
N=${TEST_HUB:-1}
NAME=$([[ $N == 1 ]] && echo TestHub || echo "TestHub$N")
# Off the production 8931/8932 so a running hub service never collides.
REMOTE_PORT=$((18940 + N))
INGEST_PORT=$((18930 + N))
BASE=${XDG_CACHE_HOME:-$HOME/.cache}/sessiontap/test-hub-$N
export XDG_CONFIG_HOME=$BASE/config XDG_STATE_HOME=$BASE/state XDG_RUNTIME_DIR=$BASE/run
HUB_BIN=${HUB_BIN:-$ROOT/target/debug/sessiontap-hub}
BUILD_HUB=${HUB_BIN_PREBUILT:-build}
SOCK=$XDG_RUNTIME_DIR/sessiontap-hub/sessiontap-hub.sock
SOURCE=${TEST_SOURCE:-host}

declare -A IDS=(
  [fix]=00000000-0000-4000-8000-00000000000$N
  [devbox]=00000000-0000-4000-8001-00000000000$N
  [billing]=00000000-0000-4000-8002-00000000000$N
  [vite]=00000000-0000-4000-8003-00000000000$N
  [triage]=00000000-0000-4000-8004-00000000000$N
)
declare -A NAMES=([fix]="Fix flaky auth tests" [devbox]="Provision dev box" [billing]="Refactor billing webhooks" [vite]="Migrate to Vite 6" [triage]="Triage open issues")
declare -A PROVIDERS=([fix]=claude [devbox]=codex [billing]=claude [vite]=claude [triage]=codex)
declare -A BRANCHES=([fix]=feat/auth-retry [devbox]=chore/devbox [billing]=feat/stripe-v2 [vite]=chore/vite-6 [triage]=main)
declare -A INITIAL=([fix]=running [devbox]=approval [billing]=child [vite]=completed [triage]=stopped)

die() { echo "test-hub.sh: $*" >&2; exit 1; }
now() { date -u +%Y-%m-%dT%H:%M:%SZ; }
uuid() { cat /proc/sys/kernel/random/uuid; }

next_revision() {
  local file=$BASE/revision rev=0
  [[ -f $file ]] && rev=$(<"$file")
  rev=$((rev + 1))
  echo "$rev" >"$file"
  echo "$rev"
}

# view <agent> <state>: one PublicAgentView as JSON.
view() {
  local agent=$1 state=$2 status reason=null children=null t
  t=$(now)
  case $state in
    running) status=running ;;
    idle) status=idle ;;
    approval) status=blocked reason='{"kind":"approval","summary":"Bash · sudo apt install postgresql-16"}' ;;
    input) status=blocked reason='{"kind":"input","summary":"Which version tag, 2.8.0 or 2.8.0-rc1?"}' ;;
    child)
      status=running
      children=$(jq -nc --arg t "$t" '[
        {agent_id:"c1",agent_type:"Explore",status:"blocked",reason:{kind:"approval",summary:"find ~/code -name \"*.pem\""},started_at:$t,updated_at:$t},
        {agent_id:"c2",agent_type:"test-runner",status:"running",reason:{summary:"auth.spec.ts · 14/31 passing"},started_at:$t,updated_at:$t}]')
      ;;
    completed) status=stopped reason='{"kind":"completed","summary":"Done · 14 files changed"}' ;;
    failed) status=stopped reason='{"kind":"failed","summary":"Exited 1"}' ;;
    stopped) status=stopped ;;
    *) die "unknown state '$state'" ;;
  esac
  jq -nc \
    --arg id "${IDS[$agent]}" --arg provider "${PROVIDERS[$agent]}" --arg status "$status" \
    --argjson reason "$reason" --argjson children "$children" --arg name "${NAMES[$agent]}" \
    --arg branch "${BRANCHES[$agent]}" --arg t "$t" --arg cwd "$HOME/code/$agent" '
    {invocation_id:$id, provider:$provider, status:$status, cwd:$cwd, created_at:$t, updated_at:$t,
     session:{id:("sess-" + $id[0:8]), name:$name},
     metadata:{model:"opus-4.1", effort:"high", permission_mode:"default"},
     usage:{input_tokens:15300, output_tokens:2100, context_tokens:84000, context_window_percent:42},
     repository:{root:$cwd, branch:$branch, head:"a41f9c2d", dirty:true}}
    + (if $reason == null then {} else {reason:$reason} end)
    + (if $children == null then {} else {children:$children} end)'
}

ingest() {
  local body=$1 code
  code=$(curl -s -o "$BASE/last-response" -w '%{http_code}' -H 'content-type: application/json' \
    --data-binary "$body" "http://127.0.0.1:$INGEST_PORT/ingest")
  [[ $code == 200 ]] || die "ingest answered $code: $(cat "$BASE/last-response")"
}

snapshot() {
  local views=() agent
  # Fixtures are reusable: drop tombstones left by forget checks so the stopped agents come back.
  sqlite3 "$XDG_STATE_HOME/sessiontap-hub/hub.sqlite3" 'DELETE FROM forgotten_agents' 2>/dev/null || true
  for agent in fix devbox billing vite triage; do views+=("$(view "$agent" "${INITIAL[$agent]}")"); done
  ingest "$(printf '%s\n' "${views[@]}" | jq -sc --arg src "$SOURCE" --argjson rev "$(next_revision)" \
    '{type:"snapshot", schema_version:1, source:{id:$src, display_name:"Host machine"}, revision:$rev, views:.}')"
}

post() {
  local agent=${1-} state=${2-}
  [[ -n ${IDS[$agent]-} ]] || die "unknown agent '$agent' (fix devbox billing vite triage)"
  ingest "$(jq -nc --arg src "$SOURCE" --arg d "$(uuid)" --argjson rev "$(next_revision)" --argjson view "$(view "$agent" "$state")" \
    '{type:"update", schema_version:1, source_id:$src, delivery_id:$d, revision:$rev, changed:["status","reason"], view:$view}')"
}

start() {
  command -v jq >/dev/null && command -v socat >/dev/null || die "jq and socat are required (nix develop .#android-emulator)"
  if [[ $BUILD_HUB == build ]]; then
    (cd "$ROOT" && cargo build -q -p sessiontap-hub)
  fi
  stop_quiet
  mkdir -p "$XDG_CONFIG_HOME/sessiontap-hub" "$XDG_STATE_HOME" "$XDG_RUNTIME_DIR"
  chmod 700 "$XDG_RUNTIME_DIR"
  cat >"$XDG_CONFIG_HOME/sessiontap-hub/config.yaml" <<YAML
version: 1
listen: "127.0.0.1:$INGEST_PORT"
remote:
  name: $NAME
  listen: ["127.0.0.1:$REMOTE_PORT"]
  advertise: ["10.0.2.2:$REMOTE_PORT"]
  control: true
YAML
  "$HUB_BIN" run >"$BASE/hub.log" 2>&1 &
  echo $! >"$BASE/pid"
  local waited=0
  until curl -sf "http://127.0.0.1:$INGEST_PORT/health" >/dev/null && [[ -S $SOCK ]]; do
    sleep 0.2
    waited=$((waited + 1))
    ((waited < 100)) || { cat "$BASE/hub.log" >&2; die "hub did not start"; }
  done
  if grep -q "cannot bind remote address" "$BASE/hub.log"; then
    cat "$BASE/hub.log" >&2
    die "remote port $REMOTE_PORT is in use"
  fi
  snapshot
  echo "test-hub.sh: $NAME on 127.0.0.1:$REMOTE_PORT (emulator: 10.0.2.2:$REMOTE_PORT), state in $BASE"
}

stop_quiet() {
  if [[ -f $BASE/pid ]]; then
    kill "$(<"$BASE/pid")" 2>/dev/null || true
    sleep 0.3
  fi
  rm -rf "$BASE"
  mkdir -p "$BASE"
}

# Opens a pair window through the hub socket. The session stays open in the
# background; `answer` replies to its confirmation.
link() {
  local expired= fifo=$BASE/pair.in out=$BASE/pair.out payload request
  if [[ ${1-} == expired ]]; then expired=expired; shift; fi
  request=$(jq -cn '{type: "pair", scopes: $ARGS.positional}' --args "$@")
  [[ -S $SOCK ]] || die "hub is not running"
  [[ -f $BASE/pair.pid ]] && kill "$(<"$BASE/pair.pid")" 2>/dev/null || true
  rm -f "$fifo" "$out"
  mkfifo "$fifo"
  # Keep the fifo open for writing so socat does not see EOF before `answer`.
  (exec 3<>"$fifo"; echo "$request" >&3; socat - "UNIX-CONNECT:$SOCK" <&3 >"$out") </dev/null >/dev/null 2>&1 &
  echo $! >"$BASE/pair.pid"
  local waited=0
  until payload=$(jq -r 'select(.type=="pair_window") | .payload' "$out" 2>/dev/null) && [[ -n $payload ]]; do
    sleep 0.1
    waited=$((waited + 1))
    ((waited < 50)) || die "no pair window: $(cat "$out" 2>/dev/null)"
  done
  if [[ $expired == expired ]]; then payload=$(jq -c '.exp = 1' <<<"$payload"); fi
  echo "sessiontap://pair?p=$(printf '%s' "$payload" | base64 -w0 | tr '+/' '-_' | tr -d '=')"
}

answer() {
  local reply fifo=$BASE/pair.in out=$BASE/pair.out
  case ${1-} in y) reply=true ;; n) reply=false ;; *) die "usage: answer y|n" ;; esac
  local waited=0
  until grep -q '"pair_confirm"' "$out" 2>/dev/null; do
    sleep 0.2
    waited=$((waited + 1))
    ((waited < 300)) || die "no device asked to pair: $(cat "$out" 2>/dev/null)"
  done
  jq -c 'select(.type=="pair_confirm")' "$out"
  echo "{\"type\":\"accept\",\"accept\":$reply}" >"$fifo"
  waited=0
  until grep -qE '"pair_done"|"pair_failed"' "$out" 2>/dev/null; do
    sleep 0.1
    waited=$((waited + 1))
    ((waited < 100)) || break
  done
  tail -1 "$out"
  kill "$(<"$BASE/pair.pid")" 2>/dev/null || true
}

case ${1-} in
  start) start ;;
  stop) stop_quiet; rm -rf "$BASE"; echo "test-hub.sh: stopped $NAME" ;;
  snapshot) snapshot ;;
  post) shift; post "$@" ;;
  link) shift; link "$@" ;;
  install-link) shift; adb shell am start -W -a android.intent.action.VIEW -d "'$(link "$@")'" dev.sessiontap.android ;;
  answer) shift; answer "$@" ;;
  hub) shift; exec "$HUB_BIN" "$@" ;;
  *) sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'; exit 1 ;;
esac
