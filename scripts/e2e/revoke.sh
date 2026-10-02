#!/usr/bin/env bash
# Revocation propagation (04 §4) between four agents on loopback, all in one nested Hyprland.
# Online: A revokes D while B is connected, so B forgets D at once. Offline: B is stopped, A
# revokes C, B restarts and gets the notice on reconnect. Revoked nodes are disconnected and can't
# come back.
set -euo pipefail
# Test agents stay off the machine's audio server and off mDNS (the agents here connect to
# configured or explicit loopback addresses only), so they never touch the real PipeWire or the
# deployed agents.
export CROSSPANE_AUDIO=0 CROSSPANE_DISCOVERY=0
cd "$(dirname "$0")/../.."
cargo build -q -p crosspane-agent -p crosspanectl
bin=$PWD/target/debug
work=$(mktemp -d /tmp/crosspane-revoke.XXXXXX)
nodes=(a b c d)
declare -A port=([a]=47981 [b]=47982 [c]=47983 [d]=47984)
cleanup() {
  for n in "${nodes[@]}"; do [ -f "$work/$n/pid" ] && kill "$(cat "$work/$n/pid")" 2>/dev/null || true; done
  scripts/hypr-nested.sh stop --name rv >/dev/null 2>&1 || true
  [ "${KEEP:-0}" = 1 ] && echo "kept $work" || rm -rf "$work"
}
trap cleanup EXIT
# The agents need a Hyprland session; they share one nest (no input or windows are involved).
scripts/hypr-nested.sh start --name rv --width 800 --height 600 >/dev/null
eval "$(scripts/hypr-nested.sh env --name rv)"
envof() {
  XDG_CONFIG_HOME="$work/$1/config" XDG_STATE_HOME="$work/$1/state" \
    CROSSPANE_RUNTIME_DIR="$work/$1/run" CROSSPANE_NO_MULTICAST=1 "${@:2}"
}
for n in "${nodes[@]}"; do
  mkdir -p "$work/$n/config/crosspane" "$work/$n/state" "$work/$n/run"
  printf 'name = "rv-%s"\nport = %s\nforce_file_keystore = true\n' "$n" "${port[$n]}" \
    > "$work/$n/config/crosspane/config.toml"
done
declare -A spki
for n in "${nodes[@]}"; do spki[$n]=$(envof "$n" "$bin/crosspane-agent" identity 2>/dev/null | awk '/^spki:/{print $2}'); done
pair() { envof "$1" "$bin/crosspane-agent" trust add "rv-$2" "${spki[$2]}" >/dev/null 2>&1; }
for p in a:b a:c a:d b:c b:d; do pair "${p%:*}" "${p#*:}"; pair "${p#*:}" "${p%:*}"; done
# A dials everyone, B dials C and D.
printf '[[peers]]\naddr = "127.0.0.1:%s"\n' "${port[b]}" "${port[c]}" "${port[d]}" >> "$work/a/config/crosspane/config.toml"
printf '[[peers]]\naddr = "127.0.0.1:%s"\n' "${port[c]}" "${port[d]}" >> "$work/b/config/crosspane/config.toml"
start() {
  envof "$1" env RUST_LOG=info setsid "$bin/crosspane-agent" run >>"$work/$1/agent.log" 2>&1 </dev/null &
  echo $! >"$work/$1/pid"
}
ctl() { envof "$1" "$bin/crosspanectl" "${@:2}"; }
connected() { ctl "$1" --json status | jq -r '.result.peers[] | select(.connected) | .name' | sort | tr '\n' ' '; }
knows() { ctl "$1" --json status | jq -e --arg p "rv-$2" '.result.peers[] | select(.name == $p)' >/dev/null; }
wait_for() { for _ in $(seq 1 60); do eval "$1" && return 0; sleep 0.25; done; echo "FAIL: timed out waiting for: $1"; for n in "${nodes[@]}"; do echo "--- $n"; tail -n 15 "$work/$n/agent.log"; done; exit 1; }
for n in d c b a; do start "$n"; sleep 0.3; done
wait_for '[ "$(connected a)" = "rv-b rv-c rv-d " ]'
wait_for '[ "$(connected b)" = "rv-a rv-c rv-d " ]'
echo "all connected"

# Online: A revokes D; B (connected to A) forgets D and drops its connection.
ctl a revoke rv-d
wait_for '! knows b d'
wait_for '[ "$(connected d)" = "" ]'
grep -q "revocation applied" "$work/b/agent.log" || { echo "FAIL: B never applied the notice"; exit 1; }
echo "online: B forgot D"

# Offline: the lost device C and peer B are both down while A revokes C (A resolves C from its
# trust store); B learns it when it reconnects to A.
kill "$(cat "$work/b/pid")" "$(cat "$work/c/pid")"; sleep 1
ctl a revoke rv-c
start c
start b
wait_for '[ "$(connected a)" = "rv-b " ]'
wait_for '! knows b c'
echo "offline: A revoked C while C was down; B forgot C after reconnecting"

# Revoked nodes stay out: D and C are refused by A and B even when they dial.
ctl d dial "127.0.0.1:${port[a]}" >/dev/null; ctl c dial "127.0.0.1:${port[b]}" >/dev/null; sleep 2
[ "$(connected d)" = "" ] && [ "$(connected c)" = "" ] || { echo "FAIL: a revoked node reconnected"; exit 1; }
echo "PASS: revocation propagates online and on reconnect; revoked nodes stay out"
