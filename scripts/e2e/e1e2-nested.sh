#!/usr/bin/env bash
# End-to-end checks for "home on the twin" (WP-2.43e) between two agents in two nested Hyprland
# instances. Every compositor operation addresses a nest; the owner's windows and cursor are
# never moved. The parent display is used only by scripts/hypr-nested.sh to open nest windows.
#
#   scripts/e2e/e1e2-nested.sh            # build, run, assert, clean up
#   KEEP=1 scripts/e2e/e1e2-nested.sh     # leave both nests and agents running afterwards
#
# Node A (nest e1e2-a) is the source and E1 controller, node B (nest e1e2-b) the destination.
# A's agent runs with CROSSPANE_TWIN_BACKEND=wayland: a nested Hyprland can't allocate headless
# outputs, so the twin is a second output of nest A itself (P2; honoured only inside a nest).
#
# Checked:
#   1. start-up (amendments A1, A2, B5): a home bind of ours left in A by an earlier run is removed
#      at start; the owner's own binding on the chord in B is left alone and doesn't fence the agent;
#   2. a window of A projected to B is parked on the twin; B's proxy is placed by the Hyprland
#      placement source (A's log shows a `ProxyPlaced` with a display, B's log the first report);
#   3. A crosses to B (E1); A offers the four twin strips and the backend confirms them
#      (latest `PortalsSet` with the four ids and `Ok`, four current capture layers on the twin);
#   4. B's pointer is moved over B's proxy: A receives the proxy's motion report, and does NOT go
#      home: no home notice, no bind, `status` says no home. THIS IS THE HARNESS'S LIMIT: entry needs
#      fresh physical motion on A, and a virtual pointer's motion doesn't reach the capture's
#      relative pointer (02 §3.3, e1-nested.sh). The debug observation proves no local capture
#      motion was seen; the frozen engine interface exposes no prevalidation or corroboration
#      rejection reason. The product code has no test seam. The entry,
#      native typing in the window, the pointer exit, the chord release and re-entry are the lead's
#      live checklist (see the report and docs/running.md);
#   5. the home bind round trip through the `home_bind` example (binds before and after), and the
#      stop path: SIGTERM removes our bind and never the owner's.
#   6. WP-2.43g: after E1 releases, place A's pointer on the known Wayland twin stand-in;
#      the housekeeping watchdog returns it to a physical output and warns once. This does
#      not force a failed home entry: physical entry still needs the lead's live check.
# If the twin can't be made (P2 failed) the script checks the Mirror case instead (no home, no
# `HomeFailed`, proxy motion received, E1 unchanged) and says so: twin coverage is then live-only
# and is NOT reported as passed. Unknown or absent parking fails the run.
set -euo pipefail
# Test agents stay off the machine's audio server and off mDNS (the agents here connect to
# configured or explicit loopback addresses only), so they never touch the real PipeWire or the
# deployed agents. The GPU paths are off too (CPU capture): this script is about the seat and the
# wiring, not the video, and on the dev machine the NVIDIA driver can crash when a GPU device that
# has carried capture is destroyed at process exit (vkDestroyDevice), which would only raise crash
# notifications on the owner's desktop.
export CROSSPANE_AUDIO=0 CROSSPANE_DISCOVERY=0 CROSSPANE_GPU=0
repo=$(cd "$(dirname "$0")/../.." && pwd -P)
cd "$repo"
work=$(mktemp -d "${XDG_RUNTIME_DIR:?}/cp-e1e2.XXXXXX")
nest_prefix=e1e2-$$-${work##*.}
owned_nests=()
bin=$repo/target/debug
# Refuse inherited live handles rather than treating them as permission to use the desktop.
[ -z "${HYPRLAND_INSTANCE_SIGNATURE:-}" ] && [ -z "${WAYLAND_DISPLAY:-}" ] \
  && [ -z "${WAYLAND_SOCKET:-}" ] || { echo "run through scripts/test-env.sh" >&2; exit 97; }
home_desc=crosspane-home-release
owner_desc=owner-chord
fail() { echo "FAIL: $*"; exit 1; }
cargo build -q -p crosspane-agent -p crosspanectl -p crosspane-testapp
cargo build -q -p crosspane-platform-linux --example vinput --example home_bind

envof() { XDG_CONFIG_HOME=$work/$1/config XDG_STATE_HOME=$work/$1/state CROSSPANE_RUNTIME_DIR=$work/$1/run "${@:2}"; }
# nest N CMD...: run CMD inside nest e1e2-N, and only there. It refuses to run in anything that
# isn't a nest started by scripts/hypr-nested.sh (the nest's env sets CROSSPANE_NESTED_HYPR=1 and a
# signature that is not the live session's).
nest() {
  local n=$1; shift
  (
    nest_env=$(scripts/hypr-nested.sh env --name "$nest_prefix-$n") || exit 1
    eval "$nest_env"
    if [ "${CROSSPANE_NESTED_HYPR:-}" != 1 ] || [ -z "${HYPRLAND_INSTANCE_SIGNATURE:-}" ]; then
      echo "refusing to run outside a nested Hyprland" >&2; exit 97
    fi
    if [ "${1:-}" = hyprctl ]; then timeout 5 "$@"; else "$@"; fi
  )
}
ctl() { envof "$1" timeout 5 "$bin/crosspanectl" "${@:2}"; }
status_json() { ctl "$1" --json status | jq -c '.result'; }
# The agent's log without colours, as a file (grep -q on a pipe would end a pipefail pipeline early).
clean() { sed 's/\x1b\[[0-9;]*m//g' "$work/$1/agent.log" > "$work/$1/clean.log"; echo "$work/$1/clean.log"; }
agent_pid() { # the agent whose CROSSPANE_RUNTIME_DIR is $1 (never any other agent)
  local p
  for p in $(pgrep -f "crosspane-agent run" || true); do
    if tr '\0' '\n' 2>/dev/null < "/proc/$p/environ" | grep -Fx -- "CROSSPANE_RUNTIME_DIR=$1" >/dev/null; then echo "$p"; fi
  done
  return 0
}
wait_for() { # wait_for SECONDS CHECK...
  local deadline=$((SECONDS + $1)); shift
  until "$@"; do [ $SECONDS -lt $deadline ] || return 1; sleep 0.2; done
}
bind_count() { # bind_count NEST DESCRIPTION: how many binds in the nest carry that description
  nest "$1" hyprctl -j binds | jq --arg d "$2" '[.[] | select(.description == $d)] | length'
}
vinput() { # vinput NEST: replay stdin into the nest's compositor
  nest "$1" timeout 30 "$bin/examples/vinput"
}

cleanup() {
  [ "${KEEP:-0}" = 1 ] && return
  local p n
  for n in a b; do for p in $(agent_pid "$work/$n/run"); do kill "$p" 2>/dev/null || true; done; done
  sleep 1
  for p in $(pgrep -x crosspane-testa || true); do
    if tr '\0' '\n' 2>/dev/null < "/proc/$p/environ" | grep -Fx -- "CROSSPANE_RUNTIME_DIR=$work/a/run" >/dev/null; then kill "$p" 2>/dev/null || true; fi
  done
  for n in "${owned_nests[@]}"; do scripts/hypr-nested.sh stop --name "$n" >/dev/null 2>&1 || true; done
}
trap cleanup EXIT

for n in a b; do
  mkdir -p "$work/$n/config/crosspane" "$work/$n/state" "$work/$n/run"
  # Claimed before the start, so a start that fails half-way (Hyprland launched, lookup failed) is
  # still cleaned up. The name is unique to this run, and the helper signals only the process it
  # started (PID plus start time), so stopping a name that never started is harmless.
  owned_nests+=("$nest_prefix-$n")
  scripts/hypr-nested.sh start --name "$nest_prefix-$n" --width 1280 --height 720 >/dev/null
done
sleep 1
port_a=47981 port_b=47991
printf 'name = "e1e2-a"\nport = %s\nforce_file_keystore = true\n[[peers]]\naddr = "127.0.0.1:%s"\n' \
  "$port_a" "$port_b" > "$work/a/config/crosspane/config.toml"
printf 'name = "e1e2-b"\nport = %s\nforce_file_keystore = true\npeers = []\n' \
  "$port_b" > "$work/b/config/crosspane/config.toml"
spki() { nest "$1" envof "$1" "$bin/crosspane-agent" identity 2>/dev/null | awk '/^spki:/{print $2}'; }
spki_a=$(spki a); spki_b=$(spki b)
envof a "$bin/crosspane-agent" trust add e1e2-b "$spki_b" --allow-input >/dev/null 2>&1
envof b "$bin/crosspane-agent" trust add e1e2-a "$spki_a" --allow-input >/dev/null 2>&1

# 1. Start-up cases, staged before the agents start: A holds a home bind of ours from an "earlier
# run"; B holds the owner's own binding on the very chord.
nest a "$bin/examples/home_bind" install --command true >/dev/null
nest b hyprctl eval "hl.bind(\"CTRL + SHIFT + ALT + Escape\", hl.dsp.exec_cmd(\"true\"), { description = \"$owner_desc\" })" >/dev/null
[ "$(bind_count a "$home_desc")" = 1 ] || fail "could not stage the leftover home bind in A"
[ "$(bind_count b "$owner_desc")" = 1 ] || fail "could not stage the owner's binding in B"

start_agent() { # start_agent N [ENV=VALUE...]
  local n=$1; shift
  nest "$n" envof "$n" env RUST_LOG=info,crosspane_agent=debug "$@" \
    setsid "$bin/crosspane-agent" run >"$work/$n/agent.log" 2>&1 </dev/null &
}
start_agent b; sleep 1; start_agent a CROSSPANE_TWIN_BACKEND=wayland
connected() { status_json a 2>/dev/null | jq -e '.peers[] | select(.name == "e1e2-b" and .connected)' >/dev/null; }
wait_for 20 connected || fail "the agents did not connect"
node_a=$(status_json a | jq -r .node); node_b=$(status_json b | jq -r .node)
[[ $node_a =~ ^[0-9a-f]{64}$ && $node_b =~ ^[0-9a-f]{64}$ && $node_a != "$node_b" ]] \
  || fail "the agents did not report distinct node identities"
source_a=${node_a:0:16}; source_b=${node_b:0:16}
# The default layout depends on the (random) node ids: put B right of A explicitly.
ctl a layout e1e2-b right >/dev/null
sleep 0.5

[ "$(bind_count a "$home_desc")" = 0 ] || fail "A's leftover home bind survived the agent's start"
echo "ok: a home bind of ours left by an earlier run was removed at start"
[ "$(bind_count b "$owner_desc")" = 1 ] || fail "B's agent removed or changed the owner's binding"
[ "$(nest b "$bin/examples/home_bind" installed)" = false ] || fail "B holds a home bind of ours"
b_status=$(ctl b status)
[[ $b_status != *"can't remove"* ]] || fail "B fenced itself over the owner's binding"
echo "ok: the owner's binding on the chord was left alone, and the agent started without a fence"
echo "status before (A): $(status_json a | jq -c .home)"

# 2. A window of A, projected to B.
nest a envof a setsid "$bin/crosspane-testapp" window --title e1e2-win --size 400x300 \
  --events "$work/win.events" >/dev/null 2>&1 </dev/null &
id=""
for _ in $(seq 1 50); do
  windows=$(ctl a windows 2>/dev/null || true)
  id=$(awk '/e1e2-win/ {print $1; exit}' <<<"$windows")
  [ -n "$id" ] && break
  sleep 0.2
done
[ -n "$id" ] || fail "the test window never appeared in A"
# `windows` asks the compositor; the engine hears of the window a debounce later. A project request
# that arrives first is refused as Busy and starts nothing, so it is simply asked again.
active_projection() {
  status_json a 2>/dev/null | jq -ce --arg source "$source_a" '
    [.projections[] | select(.source == $source and (.text | test("^projecting window [0-9]+ to e1e2-b \\([A-Za-z]+\\)$")))]
    | select(length == 1) | .[0]'
}
projected() { active_projection >/dev/null; }
for _ in $(seq 1 5); do
  ctl a project "$id" e1e2-b >/dev/null
  wait_for 6 projected && break
done
projected || fail "the window was never projected"
projection=$(active_projection) || fail "A no longer has the active projection"
projection_id=$(jq -r .projection <<<"$projection")
projection_text=$(jq -r .text <<<"$projection")
parking=$(jq -r '.text | capture("\\((?<parking>[A-Za-z]+)\\)$").parking' <<<"$projection")
case "$parking" in Twin|Mirror) ;; *) fail "the active projection has unsupported parking: $parking" ;; esac
echo "ok: active projection $source_a:$projection_id; parking is $parking"
# Frozen parking names twin outputs from the source window ID, not the projection ID.
twin_output=$(printf 'CROSSPANE-%x' "$id")

# Require the selected projection on both nodes, and the current E1 roles rather than notices.
current_control() {
  local a b
  a=$(status_json a 2>/dev/null) && b=$(status_json b 2>/dev/null) || return 1
  jq -e --arg peer "$node_b" --arg source "$source_a" --arg text "$projection_text" --argjson id "$projection_id" '
    .controlling == $peer and .controlled_by == null and
    ([.projections[] | select(.source == $source and .projection == $id and .text == $text)] | length == 1)' <<<"$a" >/dev/null \
    && jq -e --arg peer "$node_a" --arg source "$source_a" --argjson id "$projection_id" '
      .controlled_by == $peer and .controlling == null and
      ([.projections[] | select(.source == $source and .projection == $id)] | length == 1)' <<<"$b" >/dev/null
}

# The active projection on both agents, whatever E1 is doing.
projection_active() {
  local a b
  a=$(status_json a 2>/dev/null) && b=$(status_json b 2>/dev/null) || return 1
  jq -e --arg source "$source_a" --arg text "$projection_text" --argjson id "$projection_id" '
    [.projections[] | select(.source == $source and .projection == $id and .text == $text)] | length == 1' <<<"$a" >/dev/null \
    && jq -e --arg source "$source_a" --argjson id "$projection_id" '
      [.projections[] | select(.source == $source and .projection == $id)] | length == 1' <<<"$b" >/dev/null
}

cross() { # A's virtual pointer pushes past A's right edge: E1 enters B
  { echo "abs 400 200"; for _ in $(seq 1 60); do echo "rel 20 0"; echo "sleep 10"; done
    echo "sleep 300"; for _ in $(seq 1 20); do echo "rel 15 5"; echo "sleep 10"; done; echo "sleep 300"
  } | vinput a
}
proxy_address() { nest b hyprctl -j clients | jq -r 'first(.[] | select(.class == "crosspane-proxy") | .address) // empty'; }

# B's proxy is required in either parking mode, so Mirror exercises the same motion path.
# Floating and inset leaves every edge free for twin strips and fits the twin output.
have_proxy() { [ -n "$(proxy_address)" ]; }
wait_for 20 have_proxy || fail "B never opened the proxy window"
proxy=$(proxy_address)
# Centred in B's display (whatever size the outer layout gave the nest), at most 300x200, about a
# third of each side, and never under 64 (the smallest twin mode), so there is room on every side.
read -r bw bh < <(nest b hyprctl -j monitors | jq -r '.[0] | "\(.width) \(.height)"') \
  || fail "couldn't read B's monitor size"
pw=$((bw / 3)); [ "$pw" -le 300 ] || pw=300; [ "$pw" -ge 64 ] || pw=64
ph=$((bh / 3)); [ "$ph" -le 200 ] || ph=200; [ "$ph" -ge 64 ] || ph=64
[ "$bw" -ge $((pw + 20)) ] && [ "$bh" -ge $((ph + 20)) ] \
  || fail "B's display (${bw}x${bh}) is too small to leave room round a ${pw}x${ph} proxy"
px=$(((bw - pw) / 2)); py=$(((bh - ph) / 2))
nest b hyprctl dispatch "hl.dsp.window.float({ window = \"address:$proxy\", action = \"set\" })" >/dev/null
nest b hyprctl dispatch "hl.dsp.window.resize({ window = \"address:$proxy\", x = $pw, y = $ph, relative = false })" >/dev/null
nest b hyprctl dispatch "hl.dsp.window.move({ window = \"address:$proxy\", x = $px, y = $py, relative = false })" >/dev/null
if [ "$parking" = Twin ]; then
  # The source follows the proxy's size: the twin output becomes about ${pw}x${ph}. The proxy's
  # placed size is B's rounding of the requested one (300x85 for a 300x84 request in a 1912x254
  # display), so allow a pixel either way.
  twin_fits() { nest a hyprctl -j monitors | jq -e --arg output "$twin_output" --argjson w "$pw" --argjson h "$ph" '.[] | select(.name == $output and (.width - $w | fabs) <= 1 and (.height - $h | fabs) <= 1)' >/dev/null; }
  wait_for 20 twin_fits \
    || fail "A's twin output never took the proxy's size ${pw}x${ph} in a ${bw}x${bh} display: $(nest a hyprctl -j monitors | jq -c '[.[] | {name, width, height}]')"
  echo "ok: the twin output follows B's floating proxy (${pw}x${ph} in ${bw}x${bh})"
  # Creating the twin made the nest re-place its own output (its monitor rule is `auto`), which
  # moves A's display away from B in the layout: say where B is again.
  ctl a layout e1e2-b right >/dev/null
  sleep 1
else
  echo "NOTE: active Mirror projection (P2 failed in this nest): twin coverage is NOT exercised"
fi

# 3. E1: A crosses to B.
cross
wait_for 10 current_control || { cross; wait_for 10 current_control; } || fail "A and B do not have the current A-controls-B session and projection"
echo "ok: current A-controls-B session and projection confirmed on both agents"
echo "status during (A): $(status_json a | jq -c .home)"

if [ "$parking" = Twin ]; then
  strips_installed() {
    local latest
    latest=$(awk '/in: portals set / { latest = $0 } END { print latest }' "$(clean a)")
    printf '%s\n' "$latest" > "$work/portals.log"
    jq -en --arg line "$latest" '
      $line | capture("in: portals set ids=(?<ids>\\[[0-9, ]*\\]) result=Ok\\(\\(\\)\\)$").ids
      | fromjson | contains([1073741824, 1073741825, 1073741826, 1073741827])' >/dev/null
  }
  wait_for 10 strips_installed || { cross; wait_for 10 strips_installed; } \
    || fail "the four twin strips were never confirmed: $(tail -n 3 "$work/portals.log")"
  current_control || fail "current control or the active projection ended after the latest strip acknowledgement"
  echo "ok: latest PortalsSet confirmed the four twin strips with current control on both agents: $(sed 's/.*in: portals set //' "$work/portals.log")"
fi

# Compare the first placed report of this projection across B's source and A's receiver.
first_placement() {
  local n=$1 marker=$2 identity=$3
  awk -v marker="$marker" -v identity="$identity" -v id="$projection_id" '
    index($0, marker) && index($0, identity) && index($0, " projection=" id " ") &&
      index($0, " display=Some(") { print; exit }' "$(clean "$n")"
}
placement_tuple() {
  jq -cen --arg line "$1" '$line | capture("projection=(?<projection>[0-9]+).*display=Some\\(DisplayId\\((?<display>[0-9]+)\\)\\) origin=\\((?<x>[-0-9.]+), (?<y>[-0-9.]+)\\) size=(?<w>[0-9]+)x(?<h>[0-9]+)$")
    | {projection: (.projection | tonumber), display: (.display | tonumber),
       origin: [(.x | tonumber), (.y | tonumber)], size: [(.w | tonumber), (.h | tonumber)]}'
}
placement_received() {
  placed_b=$(first_placement b 'placement source: proxy reported' " source=$source_a ")
  placed_a=$(first_placement a 'in: proxy placed report' " peer=$source_b ")
  [ -n "$placed_b" ] && [ -n "$placed_a" ]
}
wait_for 10 placement_received || fail "the active projection never produced and received a placed report"
tuple_b=$(placement_tuple "$placed_b") && tuple_a=$(placement_tuple "$placed_a") \
  || fail "could not parse the first placed reports"
[ "$tuple_b" = "$tuple_a" ] || fail "the first placed reports disagree: B=$tuple_b A=$tuple_a"
echo "ok: first source placement equals A's received ProxyPlaced: $tuple_a"
echo "ok: the Hyprland placement source's first placed report: ${placed_b#*proxy reported }"
echo "ok: A received it: ${placed_a#*in: proxy placed report }"

# 4. B's pointer over B's proxy: the report reaches A, and A does not go home.
if [ "$parking" = Twin ]; then
  current_twin_layers() {
    nest a hyprctl -j layers > "$work/twin-layers.json" || return 1
    jq -e --arg output "$twin_output" '
      [.[$output].levels[]?[]? | select(.namespace == "crosspane-capture")]
      | length == 4 and ([.[].address] | unique | length == 4)' "$work/twin-layers.json" >/dev/null
  }
  wait_for 5 current_twin_layers || fail "the twin does not currently list four distinct capture strips"
  strips_installed || fail "the latest portal acknowledgement no longer confirms the four twin strips"
  echo "ok: four current crosspane-capture layers on $twin_output before proxy motion"
fi
current_control || fail "current control or the active projection ended before proxy motion"
echo "ok: both agents retain current control and the active projection before proxy motion"
before=$(wc -l < "$(clean a)")
# Absolute warps (a virtual pointer's motion doesn't reach a relative-pointer client, 02 §3.3,
# but the compositor's cursor does move) onto the middle of the proxy.
cx=$((px + pw / 2)); cy=$((py + ph / 2))
{ echo "abs $cx $cy"; echo "sleep 300"; echo "abs $((cx + 10)) $((cy + 5))"; echo "sleep 300"
  echo "abs $((cx + 20)) $((cy + 10))"; echo "sleep 500"; } | vinput b
motion_seen() { tail -n +$((before + 1)) "$(clean a)" | grep -F "in: link input peer=$source_b msg=Proj(Motion" > "$work/motion.log"; }
wait_for 5 motion_seen || fail "A never received the proxy's motion report"
sleep 2
# B's virtual pointer is local input on B, the target (WP-1.43 sees the cursor where A's injector
# didn't put it), so the owner rule (WP-1.42) hands control back: E1 may end, but only through that
# handover, and the projection stays.
projection_active || fail "the active projection ended after proxy motion"
if current_control; then
  echo "ok: both agents retain current control and the active projection after proxy motion"
else
  grep -qF "notice=e1e2-b was used locally: control returned" "$(clean a)" \
    || fail "current control ended after proxy motion without the local-use handover"
  echo "ok: B's local pointer motion handed control back (WP-1.42/1.43); the active projection stays"
fi
# The selected projection's observation must follow a fresh B motion before any new input.
# It observes the missing local motion; it does not expose the engine's rejection reason.
motion_observed_without_local_capture() {
  awk -v baseline="$before" -v peer="$source_b" -v id="$projection_id" '
    NR <= baseline { next }
    index($0, "in: link input peer=" peer " msg=Proj(Motion") { motion = $0; next }
    /proxy motion observed without local capture motion/ && motion != "" &&
      $0 ~ (" projection=" id "([[:space:]]|$)") {
      print motion; print; found = 1; exit
    }
    /in:/ { motion = "" }
    END { exit !found }' "$(clean a)" > "$work/observed-motion.log"
}
wait_for 5 motion_observed_without_local_capture \
  || fail "A did not observe fresh motion of the selected projection without local capture motion"
echo "ok: selected projection's post-baseline B motion observed without local capture motion"
sed 's/.*crosspane_agent::agent: //' "$work/observed-motion.log"
echo "LIMIT: no home and missing local capture motion are observed; engine prevalidation and corroboration rejection reasons are unavailable; physical entry remains lead-attended"
if [ "$parking" = Twin ]; then
  if grep -qE 'notice=Input (is (not )?home in|returned to|left )|home bind installed' "$(clean a)"; then
    fail "A went home: the harness drove the entry, which it can't (see the header): $(grep -E 'Input is|home bind' "$(clean a)" | head -n 3)"
  fi
  [ "$(status_json a | jq -r '.home.projection')" = null ] || fail "status says A is home"
  [ "$(bind_count a "$home_desc")" = 0 ] || fail "a home bind exists in A without home"
  echo "ok: A received the proxy's motion report and did not go home; no local capture motion was observed"
else
  # The Mirror case: no home, no HomeFailed, E1 unchanged.
  if grep -qE 'notice=Input (is (not )?home in|returned to|left )|home bind installed|in: portals set ids=.*1073741824' "$(clean a)"; then
    fail "the Mirror case produced home activity: $(grep -E 'Input is|home bind|1073741824' "$(clean a)" | head -n 3)"
  fi
  [ "$(status_json a | jq -r '.home.projection')" = null ] || fail "status says A is home"
  [ "$(bind_count a "$home_desc")" = 0 ] || fail "a home bind exists in the Mirror case"
  echo "ok (Mirror case): active Mirror projection reported proxy motion; no home, no HomeFailed, current E1 unchanged on both agents"
fi

# 5. The home bind round trip through the example, and the stop path.
nest a "$bin/examples/home_bind" install --command true >/dev/null
[ "$(bind_count a "$home_desc")" = 1 ] || fail "the home bind wasn't listed after install"
nest a hyprctl -j binds | jq -c --arg d "$home_desc" '.[] | select(.description == $d) | {key, modmask, description, submap_universal}'
[ "$(nest a "$bin/examples/home_bind" installed)" = true ] || fail "installed() disagrees with binds -j"
nest a "$bin/examples/home_bind" remove >/dev/null
[ "$(bind_count a "$home_desc")" = 0 ] || fail "the home bind was still listed after remove"
echo "ok: the home bind installed and removed (binds -j before and after)"
ctl a release >/dev/null 2>&1 || true
sleep 0.5
echo "status after (A): $(status_json a | jq -c .home)"

if [ "$parking" = Twin ]; then
  status_json a | jq -e '.controlling == null and .home.projection == null and .installer.gate.open' >/dev/null \
    || fail "A is not idle, out of home and unlocked for the watchdog regression"
  read -r tx ty < <(nest a hyprctl -j monitors | jq -r --arg output "$twin_output" \
    '.[] | select(.name == $output) | "\((.x + .width / .scale / 2) | floor) \((.y + .height / .scale / 2) | floor)"')
  rescue_message='the pointer is on a twin outside home; returning it to a physical display'
  warnings_before=$(grep -cF "$rescue_message" "$(clean a)" || true)
  nest a hyprctl dispatch "hl.dsp.cursor.move({ x = $tx, y = $ty })" >/dev/null
  cursor_is_physical() {
    local cursor monitors
    cursor=$(nest a hyprctl -j cursorpos) && monitors=$(nest a hyprctl -j monitors) || return 1
    jq -en --argjson c "$cursor" --argjson monitors "$monitors" '
      $monitors | any(.[]; (.name | startswith("CROSSPANE-") | not) and
        $c.x >= .x and $c.x < (.x + .width / .scale) and
        $c.y >= .y and $c.y < (.y + .height / .scale))' >/dev/null
  }
  wait_for 3 cursor_is_physical || fail "the watchdog left A's pointer on the twin"
  sleep 1.2
  warnings_after=$(grep -cF "$rescue_message" "$(clean a)" || true)
  [ "$warnings_after" -eq $((warnings_before + 1)) ] || fail "the watchdog did not warn exactly once"
  [ "$(bind_count a "$home_desc")" = 0 ] || fail "the watchdog installed a home bind without entry"
  echo "ok: an idle pointer on the Wayland twin stand-in was rescued to a physical output with one warning"
fi

nest a "$bin/examples/home_bind" install --command true >/dev/null
[ "$(bind_count a "$home_desc")" = 1 ] || fail "could not stage a bind of ours before the stop"
stop_agent() { # stop_agent N: SIGTERM and wait
  local n=$1 p pids
  pids=$(agent_pid "$work/$n/run")
  p=${pids%%$'\n'*}
  [ -n "$p" ] || fail "no agent $n to stop"
  kill -TERM "$p"
  stopped() { ! kill -0 "$p" 2>/dev/null; }
  wait_for 15 stopped || fail "agent $n didn't stop"
}
stop_agent a
[ "$(bind_count a "$home_desc")" = 0 ] || fail "stopping A left our home bind in place"
echo "ok: stopping the agent removed our home bind"
stop_agent b
[ "$(bind_count b "$owner_desc")" = 1 ] || fail "stopping B removed the owner's binding"
echo "ok: stopping the agent left the owner's binding alone"
# A stopped agent exits through its shutdown path (WP-2.47): no abort or crash in its log, and the
# clean-exit receipt (WP-4.6) written.
for n in a b; do
  ! grep -Eq "AccessError|Aborted|fatal runtime error|Segmentation fault|panicked at" "$(clean "$n")" \
    || fail "agent $n crashed on exit: $(grep -Em3 "AccessError|Aborted|fatal runtime error|Segmentation fault|panicked at" "$work/$n/clean.log")"
  receipt=$work/$n/state/crosspane/last_exit.json
  [ -f "$receipt" ] && jq -e '.clean == true' "$receipt" >/dev/null \
    || fail "agent $n left no clean exit receipt: $(cat "$receipt" 2>/dev/null || echo missing)"
done
echo "ok: both agents exited through their shutdown path, without a crash, and wrote a clean exit receipt"

if [ "$parking" = Twin ]; then
  echo "PASS: start-up and stop cleanup, twin parking, current twin strips, placement, proxy motion without observed local capture motion and no home, idle twin watchdog rescue (entry and exit are live-only)"
else
  echo "PASS (Mirror case only): twin coverage NOT exercised; the twin cases are live-only"
fi
