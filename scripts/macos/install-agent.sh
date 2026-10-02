#!/bin/bash
# Build, bundle and install Crosspane.app to ~/Applications, and run it at login (LaunchAgent).
# Run from the repository on the Mac, in a login shell. Re-running upgrades in place; macOS
# permissions stay granted because the signing identity and bundle ID don't change.
#
#   scripts/macos/install-agent.sh [--features private-vdisplay,video]
#
# Without `video` the agent has no H.264: it neither offers nor accepts video, and every projection
# to or from this Mac falls back to lossless tiles.
set -euo pipefail
repo=$(cd -- "${BASH_SOURCE[0]%/*}/../.." && pwd -P)
cd "$repo"
label=io.frostdev.crosspane.agent
# crosspane-media links the system libopus (WP-3.0): Homebrew's `opus`, as in CI.
export OPUS_LIB_DIR=${OPUS_LIB_DIR:-$(brew --prefix opus)}
cargo build --release --locked -p crosspane-agent "$@"
cargo build --release --locked -p crosspanectl -p crosspane-ui
scripts/macos/bundle.sh --bin target/release/crosspane-agent --id "$label" --name Crosspane \
    --out target/macos-bundles --ui-element --extra target/release/crosspane-ui \
    --entitlements packaging/macos/crosspane-agent.entitlements >/dev/null
mkdir -p "$HOME/Applications" "$HOME/Library/LaunchAgents" "$HOME/Library/Logs/Crosspane" "$HOME/.cargo/bin"
plist=$HOME/Library/LaunchAgents/$label.plist
domain=gui/$(id -u)
/bin/launchctl bootout "$domain/$label" 2>/dev/null || true
# Any agent started by hand stops cleanly too (it releases input and restores parked windows).
/usr/bin/pkill -TERM -f 'Crosspane.app/Contents/MacOS/Crosspane run' 2>/dev/null || true
sleep 2
/bin/rm -rf "$HOME/Applications/Crosspane.app"
/usr/bin/ditto target/macos-bundles/Crosspane.app "$HOME/Applications/Crosspane.app"
/usr/bin/install -m 755 target/release/crosspanectl "$HOME/.cargo/bin/crosspanectl"
/usr/bin/sed "s|__HOME__|$HOME|g" packaging/macos/$label.plist > "$plist"
/usr/bin/plutil -lint "$plist" >/dev/null
/bin/launchctl bootstrap "$domain" "$plist"
echo "installed: ~/Applications/Crosspane.app (LaunchAgent $label), ~/.cargo/bin/crosspanectl"
echo "log: ~/Library/Logs/Crosspane/agent.log"
