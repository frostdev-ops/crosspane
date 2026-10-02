#!/usr/bin/env bash
# End-to-end E1 between two agents in two nested Hyprland instances (no live session involved).
#
#   scripts/e2e/e1-nested.sh            # build, run, assert, clean up
#   KEEP=1 scripts/e2e/e1-nested.sh     # leave both nests and agents running afterwards
#
# Node A (nest e2e-a) is the controller and node B (nest e2e-b) the target. A virtual pointer in
# nest A pushes past A's right edge; the test asserts that B's agent reports "controlled by" and
# that control didn't bounce back to A. Motion forwarding needs physical input (02 §3.3).
set -euo pipefail
# Test agents stay off the machine's audio server and off mDNS (the agents here connect to
# configured or explicit loopback addresses only), so they never touch the real PipeWire or the
# deployed agents.
export CROSSPANE_AUDIO=0 CROSSPANE_DISCOVERY=0
repo=$(cd "$(dirname "$0")/../.." && pwd -P)
cd "$repo"
work=${XDG_RUNTIME_DIR:?}/cp-e2e
bin=$repo/target/debug
cargo build -q -p crosspane-agent -p crosspanectl
cargo build -q -p crosspane-platform-linux --example vinput
rm -rf "$work"; mkdir -p "$work"
cleanup() {
  if [ -n "${outer_cursor:-}" ]; then
    hyprctl dispatch "hl.dsp.cursor.move({ x = ${outer_cursor%%,*}, y = ${outer_cursor##*, } })" >/dev/null 2>&1 || true
  fi
  [ "${KEEP:-0}" = 1 ] && return
  for n in a b; do [ -f "$work/$n/pid" ] && kill "$(cat "$work/$n/pid")" 2>/dev/null || true; done
  for n in a b; do scripts/hypr-nested.sh stop --name e2e-$n >/dev/null 2>&1 || true; done
}
trap cleanup EXIT

envof() { XDG_CONFIG_HOME=$work/$1/config XDG_STATE_HOME=$work/$1/state CROSSPANE_RUNTIME_DIR=$work/$1/run "${@:2}"; }
for n in a b; do
  mkdir -p "$work/$n/config/crosspane" "$work/$n/state" "$work/$n/run"
  scripts/hypr-nested.sh start --name e2e-$n --width 1280 --height 720 >/dev/null
done
# Pin both nests as floating 1280x720 windows in the outer session: if the outer layout re-tiles
# them, their outputs resize mid-test and the capture ends (as it should, but not what we test).
nest_addr() {
  local pid; pid=$(cat "$XDG_RUNTIME_DIR/crosspane-hypr-e2e-$1/pid" 2>/dev/null || true)
  [ -n "$pid" ] && hyprctl -j clients | jq -r --argjson pid "$pid" '.[] | select(.pid == $pid) | .address' | head -n1
}
for n in a b; do
  addr=$(nest_addr "$n")
  [ -n "$addr" ] || continue
  hyprctl dispatch "hl.dsp.window.float({ window = \"address:$addr\", action = \"set\" })" >/dev/null
  hyprctl dispatch "hl.dsp.window.resize({ window = \"address:$addr\", x = 1280, y = 720, relative = false })" >/dev/null
done
sleep 1
port_a=47961 port_b=47971
cat > "$work/a/config/crosspane/config.toml" <<CFG
name = "e2e-a"
port = $port_a
force_file_keystore = true
[[peers]]
addr = "127.0.0.1:$port_b"
CFG
cat > "$work/b/config/crosspane/config.toml" <<CFG
name = "e2e-b"
port = $port_b
force_file_keystore = true
peers = []
CFG
spki() { (eval "$(scripts/hypr-nested.sh env --name e2e-$1)"; envof "$1" "$bin/crosspane-agent" identity 2>/dev/null | awk '/^spki:/{print $2}'); }
spki_a=$(spki a); spki_b=$(spki b)
envof a "$bin/crosspane-agent" trust add e2e-b "$spki_b" --allow-input >/dev/null 2>&1
envof b "$bin/crosspane-agent" trust add e2e-a "$spki_a" --allow-input >/dev/null 2>&1
for n in b a; do
  (eval "$(scripts/hypr-nested.sh env --name e2e-$n)"
   envof "$n" env RUST_LOG=info,crosspane_agent=debug setsid "$bin/crosspane-agent" run >"$work/$n/agent.log" 2>&1 </dev/null &
   echo $! >"$work/$n/pid")
  sleep 1
done
# Wait for the link.
for _ in $(seq 1 50); do
  envof a "$bin/crosspanectl" status 2>/dev/null | grep -q "connected" && break
  sleep 0.2
done
envof a "$bin/crosspanectl" status | grep -q "connected" || { echo "FAIL: peers did not connect"; exit 1; }
# The default layout depends on the (random) node ids: put B right of A explicitly.
envof a "$bin/crosspanectl" layout e2e-b right >/dev/null
sleep 0.5

# Nest A must have keyboard and pointer focus in the outer session: its pointer lock (the capture)
# only holds while it does, and the outer focus follows wherever the real mouse happens to be.
# The nest maps its pointer lock (the capture) onto the outer pointer, which only works while the
# real cursor is over the nest's window: focus nest A and put the cursor in its middle (restored
# at exit).
outer_cursor=$(hyprctl cursorpos)
addr=$(nest_addr a)
if [ -n "$addr" ]; then
  hyprctl dispatch "hl.dsp.focus({ window = \"address:$addr\" })" >/dev/null
  read -r cx cy < <(hyprctl -j clients | jq -r --arg a "$addr" '.[] | select(.address == $a) | "\(.at[0] + .size[0] / 2 | floor) \(.at[1] + .size[1] / 2 | floor)"')
  hyprctl dispatch "hl.dsp.cursor.move({ x = $cx, y = $cy })" >/dev/null
fi
sleep 0.3
(eval "$(scripts/hypr-nested.sh env --name e2e-a)"
 { echo "abs 400 200"; for _ in $(seq 1 60); do echo "rel 20 0"; echo "sleep 10"; done
   echo "sleep 300"; for _ in $(seq 1 20); do echo "rel 15 5"; echo "sleep 10"; done; echo "sleep 300"
 } | timeout 30 "$bin/examples/vinput")
pos=$(eval "$(scripts/hypr-nested.sh env --name e2e-b)"; timeout 3 hyprctl cursorpos)
grep -q "controlled by e2e-a" "$work/b/agent.log" || { echo "FAIL: B was never controlled"; exit 1; }
# A virtual pointer's motion doesn't reach the capture's relative pointer on Hyprland 0.56 (02
# §3.3), so motion forwarding can't be checked here: only that B was entered at the portal.
x=${pos%%,*}
if [ "$x" -gt 50 ]; then
  echo "note: B's pointer followed the motion too (at $pos)"
else
  echo "note: motion forwarding not exercised (virtual pointer); B's pointer at the entry $pos"
fi
if grep -q "controlled by e2e-b" "$work/a/agent.log"; then echo "FAIL: control bounced back to A"; exit 1; fi
echo "PASS: A controlled B; B's pointer at $pos"
