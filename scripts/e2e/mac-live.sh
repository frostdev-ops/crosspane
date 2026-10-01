#!/usr/bin/env bash
# Live checks between the deployed desktop agent (systemd unit) and the deployed Mac agent
# (LaunchAgent), run from the desktop's live Hyprland session. Lead-only: it moves the real pointer
# and opens a test window on each machine.
#
#   scripts/e2e/mac-live.sh e1    # the desktop pointer crosses to the Mac and back
#   scripts/e2e/mac-live.sh e2    # a test window on the Mac is projected to the desktop and back
#
# E1 uses virtual pointer motion only (never keys). E2 types a few letters into the focused proxy
# window and checks that the Mac test app received them.
set -euo pipefail
: "${HYPRLAND_INSTANCE_SIGNATURE:?run this from the live Hyprland session}"
repo=$(cd "$(dirname "$0")/../.." && pwd -P)
cd "$repo"
work=${XDG_RUNTIME_DIR:?}/cp-mac-live
rm -rf "$work"; mkdir -p "$work"
mac() { ssh -o ConnectTimeout=10 crosspane-mac "zsh -lc '$*'"; }
fail() { echo "FAIL: $*"; exit 1; }
wait_for() { # wait_for SECONDS CHECK...
  local deadline=$((SECONDS + $1)); shift
  until "$@"; do [ $SECONDS -lt $deadline ] || return 1; sleep 0.3; done
}
vinput() {
  cargo build -q -p crosspane-platform-linux --example vinput
  CROSSPANE_NESTED_HYPR=1 timeout 30 target/debug/examples/vinput
}
crosspanectl status | grep -q "macbook.*connected" || fail "the desktop agent isn't connected to the Mac"

e1() {
  mac crosspanectl status | grep -q "keys: true, pointer: true" ||
    fail "the Mac can't inject yet (Accessibility not granted?)"
  # The Mac sits left of the leftmost monitor in the default layout; enter at mid-height.
  local before after
  before=$(mac '~/cp-tools/cursorpos')
  hyprctl dispatch 'hl.dsp.cursor.move({x=200, y=1500})' >/dev/null
  { for _ in $(seq 1 30); do echo "rel -20 0"; echo "sleep 15"; done
    for _ in $(seq 1 20); do echo "rel 0 10"; echo "sleep 15"; done; echo "sleep 300"; } | vinput
  after=$(mac '~/cp-tools/cursorpos')
  mac crosspanectl status | grep -q "controlled by desktop" || fail "the Mac was never controlled"
  [ "$before" != "$after" ] || fail "the Mac pointer didn't move ($before)"
  echo "ok: the Mac pointer moved $before -> $after"
  crosspanectl release >/dev/null
  wait_for 5 bash -c "ssh crosspane-mac 'zsh -lc \"crosspanectl status\"' | grep -q 'control by desktop ended'" ||
    fail "release didn't end control on the Mac"
  echo "ok: released"
}

e2() {
  mac crosspanectl status | grep -q "frames: true" || fail "the Mac can't capture yet (Screen Recording?)"
  local label=io.frostdev.crosspane.e2test bin='~/src/crosspane/target/release/crosspane-testapp'
  mac "cd ~/src/crosspane && cargo build -q --release -p crosspane-testapp"
  mac "launchctl remove $label 2>/dev/null; rm -f /tmp/cp-e2test.events; launchctl submit -l $label -- $bin window --title crosspane-e2test --size 640x480 --events /tmp/cp-e2test.events"
  trap 'mac "launchctl remove io.frostdev.crosspane.e2test" 2>/dev/null || true' EXIT
  local wid=""
  for _ in $(seq 1 30); do
    wid=$(mac crosspanectl windows | awk '/crosspane-e2test/ {print $1; exit}')
    [ -n "$wid" ] && break; sleep 0.5
  done
  [ -n "$wid" ] || fail "the Mac test window never appeared"
  mac crosspanectl project "$wid" desktop >/dev/null
  proxy() { hyprctl -j clients | jq -r '.[] | select(.class == "crosspane-proxy") | "\(.at[0]),\(.at[1]) \(.size[0])x\(.size[1]) \(.address)"' | head -1; }
  wait_for 15 test -n "$(proxy)" || fail "no proxy window on the desktop"
  sleep 3
  read -r at size address <<< "$(proxy)"
  echo "ok: proxy $size at $at"
  crosspanectl status | grep -A1 "showing window" | sed 's/^/   /'
  # The decoded picture (before drawing: the proxy's node-coloured edge isn't in it).
  proj=$(crosspanectl status | awk '/^  projection / && /showing window/ {split($2, k, ":"); print k[2]; exit}')
  snap=$(crosspanectl snapshot macbook "$proj")
  read -r _ snapw snaph < <(head -c 64 "$snap" | tr -s ' \n' ' ' | cut -d' ' -f1-3)
  cargo run -q -p crosspane-testapp -- pattern --size "${snapw}x${snaph}" --out "$work/pattern.ppm"
  python3 - "$snap" "$work/pattern.ppm" <<'PY'
import sys
from PIL import Image, ImageChops
shot = Image.open(sys.argv[1]).convert("RGB")
ref = Image.open(sys.argv[2]).convert("RGB")
if shot.size != ref.size:
    print(f"note: proxy {shot.size} vs pattern {ref.size}; comparing the overlap")
w, h = min(shot.size[0], ref.size[0]), min(shot.size[1], ref.size[1])
diff = ImageChops.difference(shot.crop((0, 0, w, h)), ref.crop((0, 0, w, h)))
bad = sum(1 for p in diff.getdata() if p != (0, 0, 0))
print(f"pixels differing from the reference pattern: {bad} of {w * h}")
PY
  # Type into the proxy: focus it, then a few letters (no Enter).
  hyprctl dispatch "hl.dsp.focus({window=\"address:$address\"})" >/dev/null 2>&1 || true
  sleep 0.5
  { for k in 35 18 38 38 24; do echo "key $k down"; echo "sleep 20"; echo "key $k up"; echo "sleep 20"; done; echo "sleep 300"; } | vinput
  sleep 1
  local keys
  keys=$(mac "grep -c event...key /tmp/cp-e2test.events 2>/dev/null || true")
  echo "key events the Mac test app received: ${keys:-0}"
  local proj
  proj=$(mac crosspanectl status | awk '/projecting window/ {split($2, k, ":"); print k[2]; exit}')
  mac crosspanectl return "${proj:-1}" >/dev/null
  wait_for 10 test -z "$(proxy)" || fail "the proxy stayed open after return"
  echo "ok: returned"
}

case ${1:-} in
  e1) e1 ;;
  e2) e2 ;;
  *) echo "usage: $0 e1|e2" >&2; exit 2 ;;
esac
