#!/bin/bash
# Sign the Crosspane audio driver bundle and the owner probe (WP-3.4). PREPARED FOR THE OWNER/LEAD:
# implementers never run this script. Run it only after the lead has reviewed the direct test,
# lifecycle and sanitizer results, in the Mac's GUI session context (it signs through
# scripts/macos/run-in-gui.sh so the keychain identity is reachable). Signing is separate from
# installation: this script never installs, copies into /Library, uses sudo or restarts anything.
#
#   scripts/macos/build-audio-plugin.sh --test
#   scripts/macos/sign-audio-plugin.sh --identity 'Apple Development: Created via API (UMB4CJ832G)'
#
# Options:
#   --identity NAME|SHA1   an EXISTING Apple Development identity (certificate name or 40-hex SHA-1)
#   --team-id ID           expected Team ID (default: team_id under [macos] in
#                          ~/src/crosspane/crosspane.local.toml, as scripts/macos/bundle.sh reads
#                          its identity)
#
# Signing flags: no hardened runtime. The driver is a CFPlugIn bundle loaded into the system's driver
# host, which does not need it; the probe is a command-line tool that must be able to raise the
# standard Microphone permission prompt for the hidden loopback input, which a hardened-runtime
# binary could not do without an extra entitlement. No timestamp server is contacted.
#
# After signing it verifies, strictly: the bundle's signature, its signing identifier
# (io.frostdev.crosspane.audio.driver), that it is not ad hoc, and that its Team ID is the expected one.
set -euo pipefail

usage() {
    echo "Usage: sign-audio-plugin.sh --identity 'Apple Development: ... (TEAMUSER)' [--team-id TEAMID]" >&2
    exit 2
}
fail() {
    echo "sign-audio-plugin: $*" >&2
    exit 1
}

identity= team=
while [[ $# -gt 0 ]]; do
    case $1 in
        --identity|--team-id)
            [[ $# -ge 2 && -n $2 ]] || usage
            case $1 in
                --identity) identity=$2 ;;
                --team-id) team=$2 ;;
            esac
            shift 2
            ;;
        *) usage ;;
    esac
done
[[ -n $identity ]] || usage
[[ $(/usr/bin/uname -s) == Darwin ]] || fail 'macOS only'
[[ $EUID -ne 0 ]] || fail 'do not run as root; signing uses the owner keychain'
name_re='^Apple Development: .+ \([A-Z0-9]+\)$'
sha_re='^[[:xdigit:]]{40}$'
[[ $identity =~ $name_re || $identity =~ $sha_re ]] || \
    fail "identity must be an existing 'Apple Development: ...' identity or a 40-character SHA-1"

if [[ -z $team ]]; then
    config=$HOME/src/crosspane/crosspane.local.toml
    [[ -r $config ]] || fail "cannot read the expected Team ID from $config; use --team-id"
    team=$(/usr/bin/sed -n '/^[[:space:]]*\[macos\][[:space:]]*$/,/^[[:space:]]*\[/p' "$config" |
        /usr/bin/sed -n 's/^[[:space:]]*team_id[[:space:]]*=[[:space:]]*"\([A-Z0-9]*\)".*/\1/p')
fi
[[ $team =~ ^[A-Z0-9]{10}$ ]] || fail 'the expected Team ID must be 10 upper-case letters/digits'

script_dir=$(cd -- "${BASH_SOURCE[0]%/*}" && pwd -P)
root=$(cd -- "$script_dir/../.." && pwd -P)
out=$root/target/macos-audio
bundle=$out/CrosspaneAudio.driver
probe=$out/audio-loopback-probe
bundle_id=io.frostdev.crosspane.audio.driver
probe_id=io.frostdev.crosspane.audio.probe

[[ -d $bundle && ! -L $bundle ]] || fail "missing $bundle: run scripts/macos/build-audio-plugin.sh --test first"
[[ -f $probe && ! -L $probe ]] || fail "missing $probe: run scripts/macos/build-audio-plugin.sh --test first"
[[ $(/usr/bin/plutil -extract CFBundleIdentifier raw "$bundle/Contents/Info.plist") == "$bundle_id" ]] || \
    fail 'bundle identifier is not the frozen driver identifier'

echo "Signing as '$identity' (expected Team ID $team); nothing is installed."
sign=(/usr/bin/codesign --force --timestamp=none -s "$identity")
"$script_dir/run-in-gui.sh" -- "${sign[@]}" --identifier "$probe_id" "$probe"
"$script_dir/run-in-gui.sh" -- "${sign[@]}" --identifier "$bundle_id" "$bundle"

verify() { # verify path identifier
    local path=$1 want_id=$2 details
    /usr/bin/codesign --verify --strict --deep --verbose=2 "$path"
    details=$(/usr/bin/codesign -dvv "$path" 2>&1)
    /usr/bin/grep -E '^(Identifier|Authority|TeamIdentifier|Signature)=' <<< "$details" || true
    [[ $(/usr/bin/sed -n 's/^Identifier=//p' <<< "$details") == "$want_id" ]] || fail "$path: signing identifier is not $want_id"
    [[ $(/usr/bin/grep -c '^Signature=adhoc' <<< "$details" || true) == 0 ]] || fail "$path: signature is ad hoc"
    [[ $(/usr/bin/sed -n 's/^TeamIdentifier=//p' <<< "$details") == "$team" ]] || fail "$path: Team ID is not $team"
    return 0
}
echo '== verifying (strict)'
verify "$bundle" "$bundle_id"
verify "$probe" "$probe_id"
echo "signed and verified: $bundle"
echo "                     $probe"
echo 'Next (owner, after explicit approval for the run), see scripts/macos/install-audio-plugin.sh.'
