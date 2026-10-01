#!/usr/bin/env bash
# End-to-end E2 with four concurrent projections, entirely in nested Hyprlands (05 "up to 4
# concurrent projections"). Safe to run from the live session: nothing is projected from it.
#
#   scripts/e2e/e2-multi-nested.sh            # build, run, assert, clean up
#   KEEP=1 scripts/e2e/e2-multi-nested.sh     # leave the nests, agents and windows afterwards
#
# Node A (nest e2e-ma) is the source, node B (nest e2e-mb) the destination; E1 crossing is off.
# Nested Hyprland can't allocate headless outputs, so A's windows are mirrored in place (M1, the
# reported fallback, WP-2.18) rather than parked on twins. Checked:
#   1. four windows projected at once → four proxies in B, each receiving frames;
#   2. return all four → every proxy closes, A's windows are where they were, the journal is empty;
#   3. project two again, then SIGTERM A → B's proxies close and A restores both.
set -euo pipefail
repo=$(cd "$(dirname "$0")/../.." && pwd -P)
cd "$repo"
work=${XDG_RUNTIME_DIR:?}/cp-e2e-multi
bin=$repo/target/debug
cargo build -q -p crosspane-agent -p crosspanectl -p crosspane-testapp
rm -rf "$work"; mkdir -p "$work"
fail() { echo "FAIL: $*"; exit 1; }

envof() { XDG_CONFIG_HOME=$work/$1/config XDG_STATE_HOME=$work/$1/state CROSSPANE_RUNTIME_DIR=$work/$1/run "${@:2}"; }
nest() { local n=$1; shift; (eval "$(scripts/hypr-nested.sh env --name "e2e-m$n")"; "$@"); }
agent_pid() { # the agent whose CROSSPANE_RUNTIME_DIR is $1
  for p in $(pgrep -f "crosspane-agent run" || true); do
    tr '\0' '\n' < "/proc/$p/environ" 2>/dev/null | grep -qx "CROSSPANE_RUNTIME_DIR=$1" && echo "$p"
  done
}
close_windows() { # test windows by argv (pkill -f would also match a shell quoting the pattern)
  for p in $(pgrep -x crosspane-testa || true); do
    tr '\0' ' ' < "/proc/$p/cmdline" | grep -q -- "--title e2e-multi-" && kill "$p" 2>/dev/null || true
  done
}
cleanup() {
  [ "${KEEP:-0}" = 1 ] && return
  for n in a b; do kill $(agent_pid "$work/$n/run") 2>/dev/null || true; done
  sleep 1
  close_windows
  for n in a b; do scripts/hypr-nested.sh stop --name "e2e-m$n" >/dev/null 2>&1 || true; done
}
trap cleanup EXIT

for n in a b; do
  mkdir -p "$work/$n/config/crosspane" "$work/$n/state" "$work/$n/run"
  scripts/hypr-nested.sh start --name "e2e-m$n" --width 1600 --height 900 >/dev/null
done
port_a=48001 port_b=48011
cat > "$work/a/config/crosspane/config.toml" <<CFG
name = "e2e-ma"
port = $port_a
force_file_keystore = true
crossing = false
[[peers]]
addr = "127.0.0.1:$port_b"
CFG
cat > "$work/b/config/crosspane/config.toml" <<CFG
name = "e2e-mb"
port = $port_b
force_file_keystore = true
crossing = false
peers = []
CFG
spki() { nest "$1" envof "$1" "$bin/crosspane-agent" identity 2>/dev/null | awk '/^spki:/{print $2}'; }
spki_a=$(spki a); spki_b=$(spki b)
envof a "$bin/crosspane-agent" trust add e2e-mb "$spki_b" >/dev/null 2>&1
envof b "$bin/crosspane-agent" trust add e2e-ma "$spki_a" >/dev/null 2>&1
start_agent() {
  nest "$1" envof "$1" env RUST_LOG=info,crosspane_agent=debug setsid "$bin/crosspane-agent" run \
    >>"$work/$1/agent.log" 2>&1 </dev/null &
}
start_agent b; sleep 1; start_agent a
for _ in $(seq 1 75); do
  envof a "$bin/crosspanectl" status 2>/dev/null | grep -q "connected" && break
  sleep 0.2
done
envof a "$bin/crosspanectl" status | grep -q "connected" || fail "the agents did not connect"

wait_for() { # wait_for SECONDS CHECK...
  local deadline=$((SECONDS + $1)); shift
  until "$@"; do [ $SECONDS -lt $deadline ] || return 1; sleep 0.2; done
}
for i in 1 2 3 4; do
  nest a setsid "$bin/crosspane-testapp" window --title "e2e-multi-$i" --size 400x300 \
    --events "$work/win-$i.events" >/dev/null 2>&1 </dev/null &
done
ids=()
for i in 1 2 3 4; do
  id=""
  for _ in $(seq 1 50); do
    id=$(envof a "$bin/crosspanectl" windows 2>/dev/null | awk -v t="e2e-multi-$i" '$0 ~ t {print $1; exit}')
    [ -n "$id" ] && break
    sleep 0.2
  done
  [ -n "$id" ] || fail "test window $i never appeared"
  ids+=("$id")
done
spot() { nest a hyprctl -j clients | jq -r --arg t "$1" '.[] | select(.title == $t) | "\(.workspace.name) \(.at | join(",")) \(.size | join("x"))"'; }
before=$(for i in 1 2 3 4; do spot "e2e-multi-$i"; done)
proxies() { nest b timeout 3 hyprctl -j clients | jq '[.[] | select(.class == "crosspane-proxy")] | length'; }
projections() { envof "$1" "$bin/crosspanectl" status 2>/dev/null | grep -cE "^  projection " || true; }
journal_empty() { for f in "$work"/a/state/crosspane/*.json; do [ -f "$f" ] || continue; case $f in *parking*|*mirror*) [ "$(jq length "$f")" -eq 0 ] || return 1;; esac; done; }

# 1. Four at once.
for id in "${ids[@]}"; do envof a "$bin/crosspanectl" project "$id" e2e-mb >/dev/null; done
four() { [ "$(proxies)" -eq 4 ] && [ "$(projections b)" -eq 4 ]; }
wait_for 40 four || fail "expected 4 proxies, got $(proxies) (B projections $(projections b))"
sleep 3
receiving() { # every projection on B has frames
  envof b "$bin/crosspanectl" status 2>/dev/null | awk '/received/ {n++; if ($2 + 0 == 0) bad++} END {exit !(n == 4 && bad == 0)}'
}
wait_for 10 receiving || fail "not every projection receives frames: $(envof b "$bin/crosspanectl" status | grep received)"
errors=$(sed 's/\x1b\[[0-9;]*m//g' "$work/b/agent.log" | grep -c "MediaError" || true)
[ "$errors" -eq 0 ] || fail "the destination reported $errors media errors"
echo "ok: 4 concurrent projections, all receiving frames"

# 2. Return all four.
for p in $(envof a "$bin/crosspanectl" status | awk '/^  projection / {split($2, k, ":"); print k[2]}'); do
  envof a "$bin/crosspanectl" return "$p" >/dev/null
done
none() { [ "$(proxies)" -eq 0 ] && [ "$(projections a)" -eq 0 ]; }
wait_for 15 none || fail "after return: $(proxies) proxies, $(projections a) projections on A"
after=$(for i in 1 2 3 4; do spot "e2e-multi-$i"; done)
[ "$before" = "$after" ] || fail "windows moved: before [$before] after [$after]"
journal_empty || fail "the parking journal is not empty"
echo "ok: all returned, windows unchanged"

# 3. Two again, then SIGTERM the source.
for id in "${ids[@]:0:2}"; do envof a "$bin/crosspanectl" project "$id" e2e-mb >/dev/null; done
two() { [ "$(proxies)" -eq 2 ]; }
wait_for 30 two || fail "expected 2 proxies, got $(proxies)"
pid=$(agent_pid "$work/a/run")
kill -TERM "$pid"
stopped() { ! kill -0 "$pid" 2>/dev/null; }
wait_for 10 stopped || fail "the source agent didn't stop"
no_proxies() { [ "$(proxies)" -eq 0 ]; }
wait_for 30 no_proxies || fail "B kept $(proxies) proxies after the source stopped"
journal_empty || fail "SIGTERM left the journal non-empty"
echo "ok: SIGTERM ended both projections and restored the windows"
echo "PASS"
