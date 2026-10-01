#!/usr/bin/env bash
# End-to-end E2: a window of the LIVE Hyprland session is projected into a nested Hyprland.
# Lead-only: it parks a real window on a headless twin output of the live session (nested
# instances can't allocate headless outputs), so it is never run by implementers or CI.
#
#   scripts/e2e/e2-live-nested.sh            # build, run, assert, clean up
#   KEEP=1 scripts/e2e/e2-live-nested.sh     # leave the nest, agents and test window afterwards
#
# Node L (live session) is the source and node B (nest e2e-b) the destination; E1 crossing is off
# on both, so the live session's own input is never routed. The test window is a foot terminal
# printing the time. Checked:
#   1. project → the window is parked on a CROSSPANE-* output and a proxy opens in the nest;
#   2. return  → the window is back and the twin output is gone;
#   3. project again, then SIGTERM the source agent → the clean shutdown restores the window.
set -euo pipefail
: "${HYPRLAND_INSTANCE_SIGNATURE:?run this from the live Hyprland session}"
repo=$(cd "$(dirname "$0")/../.." && pwd -P)
cd "$repo"
work=${XDG_RUNTIME_DIR:?}/cp-e2e-live
bin=$repo/target/debug
app=crosspane-e2-test
cargo build -q -p crosspane-agent -p crosspanectl
rm -rf "$work"; mkdir -p "$work"
pkill -f -- "--app-id $app" 2>/dev/null || true # a test window left by an earlier KEEP=1 run

agent_pid() { # the agent whose CROSSPANE_RUNTIME_DIR is $1 (the setsid wrapper's PID isn't it)
  for p in $(pgrep -f "crosspane-agent run" || true); do
    tr '\0' '\n' < "/proc/$p/environ" 2>/dev/null | grep -qx "CROSSPANE_RUNTIME_DIR=$1" && echo "$p"
  done
}
cleanup() {
  [ "${KEEP:-0}" = 1 ] && return
  for n in l b; do kill $(agent_pid "$work/$n/run") 2>/dev/null || true; done
  sleep 1
  pkill -f -- "--app-id $app" 2>/dev/null || true
  scripts/hypr-nested.sh stop --name e2e-b >/dev/null 2>&1 || true
}
trap cleanup EXIT
fail() { echo "FAIL: $*"; exit 1; }

envof() { XDG_CONFIG_HOME=$work/$1/config XDG_STATE_HOME=$work/$1/state CROSSPANE_RUNTIME_DIR=$work/$1/run "${@:2}"; }
nest() { (eval "$(scripts/hypr-nested.sh env --name e2e-b)"; "$@"); }
for n in l b; do mkdir -p "$work/$n/config/crosspane" "$work/$n/state" "$work/$n/run"; done
scripts/hypr-nested.sh start --name e2e-b --width 1600 --height 900 >/dev/null
port_l=47981 port_b=47991
cat > "$work/l/config/crosspane/config.toml" <<CFG
name = "e2e-live"
port = $port_l
force_file_keystore = true
crossing = false
[[peers]]
addr = "127.0.0.1:$port_b"
CFG
cat > "$work/b/config/crosspane/config.toml" <<CFG
name = "e2e-b"
port = $port_b
force_file_keystore = true
crossing = false
peers = []
CFG
spki_l=$(envof l "$bin/crosspane-agent" identity 2>/dev/null | awk '/^spki:/{print $2}')
spki_b=$(nest envof b "$bin/crosspane-agent" identity 2>/dev/null | awk '/^spki:/{print $2}')
envof l "$bin/crosspane-agent" trust add e2e-b "$spki_b" >/dev/null 2>&1
envof b "$bin/crosspane-agent" trust add e2e-live "$spki_l" >/dev/null 2>&1

start_agent() {
  if [ "$1" = b ]; then
    nest envof b env RUST_LOG=info,crosspane_agent=debug setsid "$bin/crosspane-agent" run \
      >>"$work/b/agent.log" 2>&1 </dev/null &
  else
    envof l env RUST_LOG=info,crosspane_agent=debug setsid "$bin/crosspane-agent" run \
      >>"$work/l/agent.log" 2>&1 </dev/null &
  fi
}
wait_connected() {
  for _ in $(seq 1 75); do
    envof l "$bin/crosspanectl" status 2>/dev/null | grep -q "connected" && return 0
    sleep 0.2
  done
  fail "the agents did not connect"
}
start_agent b; sleep 1; start_agent l; wait_connected

hyprctl dispatch "hl.dsp.exec_cmd(\"foot --app-id $app -e sh -c 'while true; do date +%T.%N; sleep 0.2; done'\")" >/dev/null
for _ in $(seq 1 25); do
  wid=$(envof l "$bin/crosspanectl" windows 2>/dev/null | awk -v a="$app" '$2 == a {print $1; exit}')
  [ -n "$wid" ] && break
  sleep 0.2
done
[ -n "${wid:-}" ] || fail "the test window never appeared"

# Window ids are Hyprland's stableId (hex, no 0x).
where() { hyprctl -j clients | jq -r --arg s "$(printf '%x' "$wid")" '.[] | select(.stableId == $s) | .workspace.name'; }
twins() { hyprctl -j monitors | jq '[.[] | select(.name | startswith("CROSSPANE-"))] | length'; }
proxies() { nest timeout 3 hyprctl -j clients | jq --arg a crosspane-proxy '[.[] | select(.class == $a)] | length'; }
wait_for() { # wait_for SECONDS CHECK...
  local deadline=$((SECONDS + $1)); shift
  until "$@"; do [ $SECONDS -lt $deadline ] || return 1; sleep 0.2; done
}
parked() { [[ $(where) == crosspane-* ]] && [ "$(twins)" -ge 1 ] && [ "$(proxies)" -ge 1 ]; }
restored() { [[ $(where) != crosspane-* ]] && [ "$(twins)" -eq 0 ]; }
no_proxy() { [ "$(proxies)" -eq 0 ]; }

home=$(where)
echo "test window $wid on workspace $home"

# 1. Project.
envof l "$bin/crosspanectl" project "$wid" e2e-b >/dev/null
wait_for 10 parked || fail "not projected (workspace $(where), twins $(twins), proxies $(proxies))"
sleep 2
errors=$(sed 's/\x1b\[[0-9;]*m//g' "$work/b/agent.log" | grep -c "MediaError" || true)
[ "$errors" -eq 0 ] || fail "the destination reported $errors media errors"
echo "ok: projected (window on $(where), proxy open in the nest)"

# 2. Return.
envof l "$bin/crosspanectl" return 1 >/dev/null
wait_for 10 restored || fail "not returned (workspace $(where), twins $(twins))"
wait_for 5 no_proxy || fail "the proxy stayed open in the nest"
echo "ok: returned to $(where)"

# 3. Project again, then stop the source agent with SIGTERM.
envof l "$bin/crosspanectl" project "$wid" e2e-b >/dev/null
wait_for 10 parked || fail "not projected the second time"
pid=$(agent_pid "$work/l/run")
stopped() { ! kill -0 "$pid" 2>/dev/null; }
kill -TERM "$pid"
wait_for 10 stopped || fail "the agent didn't stop"
wait_for 5 restored || fail "SIGTERM left the window parked (workspace $(where), twins $(twins))"
grep -q "parked windows restored" "$work/l/agent.log" || fail "no clean-shutdown log line"
echo "ok: SIGTERM restored the window to $(where)"
echo "PASS"
