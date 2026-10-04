#!/usr/bin/env bash
# Scripted fake agent for terminal relay checks. Launched by
# `test-hub.sh terminal start` through `sessiontap` with a provider alias that
# inherits claude, so it reports status like Claude through
# `sessiontap hook emit` with the wrapper's environment:
#
#   draws a numbered approval menu    -> PermissionRequest (blocked, approval)
#   reads one raw key 1-3             -> prints the choice, then Stop and an
#                                        idle_prompt Notification (idle)
#   waits for a free-text line        -> prints it, then shows the menu again
#   q (menu key or reply line)        -> exits
#
# SESSIONTAP_BIN names the wrapper binary when it is not on PATH.
set -u

ST=${SESSIONTAP_BIN:-sessiontap}
SESSION=fake-agent-$$

emit() {
  [[ -n ${SESSIONTAP_PROVIDER-} ]] || return 0
  printf '%s' "$1" | "$ST" hook emit "$SESSIONTAP_PROVIDER" >/dev/null 2>&1 || true
}

saved=$(stty -g)
trap 'stty "$saved"' EXIT

while true; do
  printf '\033[2J\033[H'
  printf 'Fake agent wants to run a command:\r\n\r\n'
  printf '  rm -rf build/\r\n\r\n'
  printf 'Do you want to proceed?\r\n'
  printf '  1. Yes\r\n'
  printf "  2. Yes, and don't ask again for rm\r\n"
  printf '  3. No, and tell the agent what to do differently\r\n\r\n'
  printf 'Press 1-3 (q quits) '
  emit "{\"hook_event_name\":\"PermissionRequest\",\"session_id\":\"$SESSION\",\"tool_name\":\"Bash\",\"tool_input\":{\"command\":\"rm -rf build/\",\"description\":\"Clean the build directory\"},\"permission_mode\":\"default\"}"
  key=
  while [[ -z $key ]]; do
    stty raw -echo
    key=$(dd bs=1 count=1 2>/dev/null)
    stty "$saved"
    case $key in
      1 | 2 | 3) ;;
      q) printf '\r\n'; exit 0 ;;
      *) key= ;;
    esac
  done
  printf '\r\nYou chose option %s\r\n' "$key"
  emit "{\"hook_event_name\":\"Stop\",\"session_id\":\"$SESSION\"}"
  emit "{\"hook_event_name\":\"Notification\",\"session_id\":\"$SESSION\",\"notification_type\":\"idle_prompt\",\"message\":\"Waiting for your reply\"}"
  printf 'Reply to the agent (q quits): '
  IFS= read -r line || exit 0
  [[ $line == q ]] && exit 0
  printf 'Agent got: %s\n' "$line"
  sleep 1
done
