#!/usr/bin/env bash
# An owned server with no hardware discovery or access to the owner's server.
set -euo pipefail
if (( $# == 0 )); then
    echo 'usage: private-pipewire.sh command [args...]' >&2
    exit 2
fi
fixture=$(mktemp -d /tmp/crosspane-pipewire.XXXXXXXX)
chmod 700 "$fixture"
server_pid=
cleanup() {
    if [[ -n "$server_pid" ]]; then
        kill "$server_pid" 2>/dev/null || true
        wait "$server_pid" 2>/dev/null || true
    fi
    rm -rf -- "$fixture"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
export XDG_RUNTIME_DIR="$fixture"
export PIPEWIRE_RUNTIME_DIR="$fixture"
export PIPEWIRE_REMOTE="crosspane-${fixture##*.}"
export CROSSPANE_PRIVATE_PIPEWIRE=1
export CROSSPANE_PRIVATE_AUDIO_TEST_DONE="$fixture/audio-tests-ran"
export PIPEWIRE_CONFIG_DIR="$fixture"
export PIPEWIRE_CONFIG_PREFIX=
export PIPEWIRE_CONFIG_NAME=private.conf
unset PIPEWIRE_CORE PIPEWIRE_AUTOCONNECT
cat > "$fixture/private.conf" <<EOF
context.properties = {
    core.daemon = true
    core.name = "$PIPEWIRE_REMOTE"
    support.dbus = false
    default.clock.rate = 48000
    default.clock.quantum = 480
    default.clock.min-quantum = 480
    default.clock.max-quantum = 480
}
context.spa-libs = {
    support.* = support/libspa-support
    audio.convert.* = audioconvert/libspa-audioconvert
}
context.modules = [
    { name = libpipewire-module-protocol-native }
    { name = libpipewire-module-access args = { access.force = unrestricted } }
    { name = libpipewire-module-metadata }
    { name = libpipewire-module-spa-node-factory }
    { name = libpipewire-module-client-node }
    { name = libpipewire-module-adapter }
    { name = libpipewire-module-link-factory }
]
context.objects = [
    { factory = spa-node-factory args = {
        factory.name = support.node.driver
        node.name = crosspane.private.driver
        priority.driver = 20000
    } }
    { factory = adapter args = {
        factory.name = support.null-audio-sink
        node.name = crosspane.private.sink
        media.class = Audio/Sink
        audio.format = F32LE
        audio.rate = 48000
        audio.channels = 2
        audio.position = [ FL FR ]
        adapter.auto-port-config = { mode = dsp monitor = true position = preserve }
    } }
    { factory = adapter args = {
        factory.name = support.null-audio-sink
        node.name = crosspane.private.source
        media.class = Audio/Source/Virtual
        audio.format = F32LE
        audio.rate = 48000
        audio.channels = 1
        audio.position = [ MONO ]
        adapter.auto-port-config = { mode = dsp monitor = true position = preserve }
    } }
    { factory = metadata args = {
        metadata.name = default
        metadata.values = [
            { key = default.audio.sink value = { name = crosspane.private.sink } }
            { key = default.audio.source value = { name = crosspane.private.source } }
        ]
    } }
]
EOF
pipewire -c "$fixture/private.conf" > "$fixture/server.log" 2>&1 &
server_pid=$!
export CROSSPANE_PRIVATE_PIPEWIRE_PID="$server_pid"
for (( attempt=0; attempt<100; attempt++ )); do
    if ! kill -0 "$server_pid" 2>/dev/null; then
        cat "$fixture/server.log" >&2
        echo 'private PipeWire server exited before creating its socket' >&2
        exit 1
    fi
    if [[ -S "$fixture/$PIPEWIRE_REMOTE" ]]; then
        break
    fi
    sleep 0.02
done
if [[ ! -S "$fixture/$PIPEWIRE_REMOTE" || ! -O "$fixture/$PIPEWIRE_REMOTE" || -L "$fixture/$PIPEWIRE_REMOTE" ]]; then
    cat "$fixture/server.log" >&2
    echo 'private PipeWire socket was not created within 2 seconds' >&2
    exit 1
fi
# Clients must use their normal client configuration, never the daemon configuration.
unset PIPEWIRE_CONFIG_DIR PIPEWIRE_CONFIG_PREFIX PIPEWIRE_CONFIG_NAME
pipewire --version
pw-cli --version
expect_audio=0
previous=
for argument in "$@"; do
    if [[ "$previous" == --test && "$argument" == audio || "$argument" == --test=audio ]]; then
        expect_audio=1
    fi
    previous="$argument"
done
"$@"
if (( expect_audio )); then
    if [[ ! -f "$CROSSPANE_PRIVATE_AUDIO_TEST_DONE" ]] ||
       [[ $(<"$CROSSPANE_PRIVATE_AUDIO_TEST_DONE") != PRIVATE_AUDIO_SERVER_TESTS_RAN ]]; then
        echo 'private audio server tests did not complete; refusing a passing skip' >&2
        exit 1
    fi
    echo 'verified: private audio server tests actually ran'
fi
