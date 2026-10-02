#!/usr/bin/env bash
# Start, inspect and stop a Hyprland instance nested in the owner's live session, for automated
# platform tests and spikes (docs/wp/WP-0.11.md).
#
#   scripts/hypr-nested.sh start [--name N] [--width W --height H] [--config FILE]
#   scripts/hypr-nested.sh env|status|stop [--name N]
#
# Safety rules (AGENTS.md, R19):
# - Every hyprctl call names the nested instance explicitly (-i <signature>) and has a timeout.
#   The live instance is never addressed by this script.
# - Only the PID this script started is ever signalled.
# Hyprland 0.56 has no headless-only mode (aquamarine can't create a backend without DRM or a
# parent Wayland compositor), so the instance runs as a window (class "aquamarine") on the live
# desktop. A live window rule can send it to a special workspace; see scripts/hypr-nested/README.md.
set -euo pipefail

script_dir=$(cd -- "${BASH_SOURCE[0]%/*}" && pwd -P)
config=$script_dir/hypr-nested/hyprland.lua

usage() {
    sed -n '5,6p' "${BASH_SOURCE[0]}" | sed 's/^#   //' >&2
    exit 2
}

[[ $# -ge 1 ]] || usage
cmd=$1
shift
name=test
width=1280
height=720
while [[ $# -gt 0 ]]; do
    case $1 in
        --name) name=${2:?}; shift 2 ;;
        --width) width=${2:?}; shift 2 ;;
        --height) height=${2:?}; shift 2 ;;
        --config) config=$(cd -- "$(dirname -- "${2:?}")" && pwd -P)/$(basename -- "$2"); shift 2 ;;
        *) usage ;;
    esac
done
[[ $name =~ ^[A-Za-z0-9_-]+$ ]] || { echo "hypr-nested: bad name: $name" >&2; exit 2; }
[[ -n ${XDG_RUNTIME_DIR:-} ]] || { echo "hypr-nested: XDG_RUNTIME_DIR is not set" >&2; exit 2; }
state=$XDG_RUNTIME_DIR/crosspane-hypr-$name

# A process's start time (clock ticks after boot, /proc/PID/stat field 22), or empty if there is
# no such process. The comm field may hold spaces, so fields are counted after its closing paren.
starttime() {
    local stat
    stat=$(cat "/proc/$1/stat" 2>/dev/null) || return 0
    awk '{print $20}' <<<"${stat##*) }"
}

# Whether the recorded process is still the Hyprland this script started: same PID **and** same
# start time. A PID alone can be reused (by anything, the owner's compositor included), and then
# nothing here may signal it or address its instance.
owned() {
    [[ -f $state/pid && -f $state/start ]] || return 1
    local now
    now=$(starttime "$(<"$state/pid")")
    [[ -n $now && $now == "$(<"$state/start")" ]]
}

# The nested instance's signature, or empty if it isn't running.
signature() {
    owned || return 0
    local pid
    pid=$(<"$state/pid")
    timeout 5 hyprctl instances -j 2>/dev/null |
        jq -r --argjson pid "$pid" '.[] | select(.pid == $pid) | .instance' | head -n1
}

nested() {
    local sig=$1
    shift
    timeout 5 hyprctl -i "$sig" "$@"
}

start() {
    if [[ -n $(signature) ]]; then
        echo "hypr-nested: $name is already running" >&2
        exit 1
    fi
    # Implementer runs (scripts/lead/run-wp.sh) have no WAYLAND_DISPLAY, so nothing they run can
    # reach the live session by accident; only this script gets the parent display, to open the
    # nested instance's window.
    local parent=${WAYLAND_DISPLAY:-${CROSSPANE_PARENT_WAYLAND_DISPLAY:-}}
    [[ -n $parent ]] || { echo "hypr-nested: needs a parent Wayland session (WAYLAND_DISPLAY)" >&2; exit 1; }
    rm -rf -- "$state"
    mkdir -p -- "$state"
    local t0=$SECONDS
    env -u HYPRLAND_INSTANCE_SIGNATURE -u WAYLAND_SOCKET WAYLAND_DISPLAY="$parent" \
        CROSSPANE_NESTED_WIDTH="$width" CROSSPANE_NESTED_HEIGHT="$height" \
        setsid Hyprland --config "$config" >"$state/stdout.log" 2>&1 </dev/null &
    echo $! >"$state/pid"
    starttime $! >"$state/start"

    local sig="" deadline=$((SECONDS + 20))
    while [[ -z $sig ]]; do
        (( SECONDS < deadline )) || { echo "hypr-nested: instance didn't appear; see $state/stdout.log" >&2; stop_quiet; exit 1; }
        sleep 0.2
        sig=$(signature)
    done
    until nested "$sig" -j monitors 2>/dev/null | jq -e 'length > 0' >/dev/null; do
        (( SECONDS < deadline )) || { echo "hypr-nested: instance didn't answer IPC" >&2; stop_quiet; exit 1; }
        sleep 0.2
    done
    local wl
    wl=$(timeout 5 hyprctl instances -j | jq -r --arg sig "$sig" '.[] | select(.instance == $sig) | .wl_socket')
    printf 'unset WAYLAND_SOCKET\nexport HYPRLAND_INSTANCE_SIGNATURE=%q\nexport WAYLAND_DISPLAY=%q\nexport CROSSPANE_NESTED_HYPR=1\n' "$sig" "$wl" >"$state/env"
    printf 'hypr-nested: %s started in %ss\n  instance %s\n  wayland  %s\n' "$name" "$((SECONDS - t0))" "$sig" "$wl"
}

env_cmd() {
    [[ -n $(signature) && -f $state/env ]] || { echo "hypr-nested: $name is not running" >&2; exit 1; }
    cat "$state/env"
}

status() {
    local sig
    sig=$(signature)
    if [[ -z $sig ]]; then
        echo "$name: stopped"
        return 0
    fi
    echo "$name: running (pid $(<"$state/pid"), instance $sig)"
    nested "$sig" -j monitors | jq -r '.[] | "  monitor \(.name) \(.width)x\(.height)@\(.refreshRate) scale \(.scale)"'
}

stop_quiet() {
    [[ -f $state/pid ]] || { rm -rf -- "$state"; return 0; }
    local pid sig
    pid=$(<"$state/pid")
    if ! owned; then
        # Gone, or the PID now belongs to another process: never signal it.
        kill -0 "$pid" 2>/dev/null &&
            echo "hypr-nested: pid $pid is no longer $name's Hyprland; not signalling it" >&2
        rm -rf -- "$state"
        return 0
    fi
    sig=$(signature)
    if [[ -n $sig ]]; then
        nested "$sig" eval 'hl.dispatch(hl.dsp.exit())' >/dev/null 2>&1 || true
    fi
    local i
    for i in $(seq 1 25); do
        owned || break
        sleep 0.2
    done
    if owned; then
        kill -TERM "$pid" 2>/dev/null || true
        for i in $(seq 1 25); do
            owned || break
            sleep 0.2
        done
    fi
    owned && kill -KILL "$pid" 2>/dev/null || true
    rm -rf -- "$state"
}

case $cmd in
    start) start ;;
    env) env_cmd ;;
    status) status ;;
    stop) stop_quiet; echo "$name: stopped" ;;
    *) usage ;;
esac
