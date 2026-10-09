#!/usr/bin/env bash
# Run a development command with live desktop and audio handles removed.
#
#   scripts/test-env.sh <command…>
#
# The parent Wayland display is kept only for scripts/hypr-nested.sh to open a
# nested compositor. Tests that need a session bus start their own.
set -u
[ $# -ge 1 ] || { echo 'usage: test-env.sh <command…>' >&2; exit 2; }
exec env -u HYPRLAND_INSTANCE_SIGNATURE -u WAYLAND_DISPLAY -u WAYLAND_SOCKET -u DISPLAY \
    -u PIPEWIRE_REMOTE -u PULSE_SERVER \
    DBUS_SESSION_BUS_ADDRESS=unix:path=/nonexistent/crosspane-test-bus \
    CROSSPANE_PARENT_WAYLAND_DISPLAY="${CROSSPANE_PARENT_WAYLAND_DISPLAY:-${WAYLAND_DISPLAY:-}}" \
    "$@"
