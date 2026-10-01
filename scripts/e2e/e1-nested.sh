#!/usr/bin/env bash
# End-to-end E1 between two agents in two nested Hyprland instances (no live session involved).
#
#   scripts/e2e/e1-nested.sh            # build, run, assert, clean up
#   KEEP=1 scripts/e2e/e1-nested.sh     # leave both nests and agents running afterwards
#
# Node A (nest e2e-a) is the controller and node B (nest e2e-b) the target. A virtual pointer in
# nest A pushes past A's right edge; the test asserts that B's agent reports "controlled by" and
# that B's pointer follows A's motion.
set -euo pipefail
repo=$(cd "$(dirname "$0")/../.." && pwd -P)
cd "$repo"
work=${XDG_RUNTIME_DIR:?}/cp-e2e
bin=$repo/target/debug
cargo build -q -p crosspane-agent -p crosspanectl
cargo build -q -p crosspane-platform-linux --example vinput
rm -rf "$work"; mkdir -p "$work"
cleanup() {
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

(eval "$(scripts/hypr-nested.sh env --name e2e-a)"
 { echo "abs 400 200"; for _ in $(seq 1 60); do echo "rel 20 0"; echo "sleep 10"; done
   echo "sleep 300"; for _ in $(seq 1 20); do echo "rel 15 5"; echo "sleep 10"; done; echo "sleep 300"
 } | timeout 30 "$bin/examples/vinput")
pos=$(eval "$(scripts/hypr-nested.sh env --name e2e-b)"; timeout 3 hyprctl cursorpos)
grep -q "controlled by e2e-a" "$work/b/agent.log" || { echo "FAIL: B was never controlled"; exit 1; }
x=${pos%%,*}
[ "$x" -gt 50 ] || { echo "FAIL: B's pointer didn't follow (at $pos)"; exit 1; }
if grep -q "controlled by e2e-b" "$work/a/agent.log"; then echo "FAIL: control bounced back to A"; exit 1; fi
echo "PASS: A controlled B; B's pointer at $pos"
