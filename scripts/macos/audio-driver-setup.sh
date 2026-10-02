#!/bin/bash
# Owner-run, one command: build, test, sign, install and probe the Crosspane audio driver (WP-3.4).
# Run it in Terminal on the Mac (not over SSH): the install asks for your password once and for the
# word INSTALL, and the probe may raise a Microphone permission prompt for Terminal.
#
#   ~/src/crosspane/scripts/macos/audio-driver-setup.sh              # first install
#   ~/src/crosspane/scripts/macos/audio-driver-setup.sh --reinstall  # replace an installed driver
#   ~/src/crosspane/scripts/macos/audio-driver-setup.sh --uninstall  # remove it
#
# Everything is also written to ~/Library/Logs/Crosspane/audio-driver-setup-<time>.log, which the
# lead reads afterwards. Installing restarts the system audio daemon (sound drops for a moment).
set -uo pipefail

mode=install
case ${1:-} in
    '') ;;
    --reinstall) mode=reinstall ;;
    --uninstall) mode=uninstall ;;
    *) echo "usage: $0 [--reinstall | --uninstall]" >&2; exit 2 ;;
esac

repo=$(cd "$(dirname "$0")/../.." && pwd -P)
cd "$repo" || exit 1
logs="$HOME/Library/Logs/Crosspane"
mkdir -p "$logs"
log="$logs/audio-driver-setup-$(date +%Y%m%d-%H%M%S).log"
exec > >(tee -a "$log") 2>&1

step() { printf '\n=== %s\n' "$*"; }
fail() { printf '\nFAILED: %s\nlog: %s\n' "$*" "$log"; exit 1; }

echo "Crosspane audio driver setup ($mode), $(date), repo $repo at $(git -C "$repo" log -1 --format='%h %s')"
export CROSSPANE_AUDIO_OWNER_ATTENDED=1
dest=/Library/Audio/Plug-Ins/HAL/CrosspaneAudio.driver

if [[ $mode == uninstall ]]; then
    step "uninstall (asks for your password; type UNINSTALL to confirm)"
    scripts/macos/install-audio-plugin.sh --uninstall || fail "uninstall"
    echo "done. log: $log"
    exit 0
fi

identity=$(sed -n 's/^[[:space:]]*signing_identity[[:space:]]*=[[:space:]]*"\(.*\)".*/\1/p' \
    "$repo/crosspane.local.toml" 2>/dev/null | head -n 1)
[[ -n $identity ]] || fail "no signing_identity in $repo/crosspane.local.toml"

step "1/4 build and run the driver's own tests"
scripts/macos/build-audio-plugin.sh --test || fail "build/test"

step "2/4 sign with '$identity'"
scripts/macos/sign-audio-plugin.sh --identity "$identity" || fail "sign"

step "3/4 install (asks for your password once; type INSTALL to confirm)"
if [[ -e $dest ]]; then
    if [[ $mode == reinstall ]]; then
        echo "removing the installed driver first (type UNINSTALL to confirm)"
        scripts/macos/install-audio-plugin.sh --uninstall || fail "uninstall before reinstall"
    else
        echo "already installed at $dest: keeping it (use --reinstall to replace it)"
    fi
fi
if [[ ! -e $dest ]]; then
    scripts/macos/install-audio-plugin.sh --install target/macos-audio/CrosspaneAudio.driver ||
        fail "install"
fi

# The audio daemon loads plug-ins only when it starts. If the driver is installed but the HAL
# doesn't list its speakers yet (e.g. an earlier install whose daemon restart failed), restart it.
loaded() { /usr/sbin/system_profiler SPAudioDataType 2>/dev/null | grep -q 'Crosspane speakers'; }
if ! loaded; then
    echo "the audio daemon hasn't loaded the driver yet: restarting it (sound drops for a moment)"
    /usr/bin/sudo /usr/bin/killall coreaudiod || fail "restart coreaudiod"
    for _ in 1 2 3 4 5 6 7 8 9 10; do
        sleep 1
        loaded && break
    done
fi
if loaded; then
    echo "the HAL lists 'Crosspane speakers'"
else
    echo "warning: 'Crosspane speakers' is still not listed; the probe will say why"
fi

step "4/4 probe the installed driver (synthetic tone through the loopback; no real audio)"
echo "If macOS asks whether Terminal may use the microphone, choose Allow for this test."
target/macos-audio/audio-loopback-probe --synthetic-speakers
probe=$?
echo
if ((probe == 0)); then
    echo "PROBE PASSED. log: $log"
else
    echo "PROBE FAILED (exit $probe). log: $log"
fi
exit "$probe"
