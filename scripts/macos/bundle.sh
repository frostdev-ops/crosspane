#!/bin/bash
set -euo pipefail

usage() {
    echo 'Usage: bundle.sh --bin PATH --id BUNDLE_ID --name NAME --out DIR [--ui-element] [--entitlements PLIST] [--identity SHA1] [--extra PATH]...' >&2
    exit 2
}
fail() {
    echo "$*" >&2
    exit 1
}

bin= id= name= out= entitlements= identity=
extras=()
ui_element=false
while [[ $# -gt 0 ]]; do
    case $1 in
        --bin|--id|--name|--out|--entitlements|--identity|--extra)
            [[ $# -ge 2 && -n $2 ]] || usage
            case $1 in
                --bin) bin=$2 ;;
                --id) id=$2 ;;
                --name) name=$2 ;;
                --out) out=$2 ;;
                --entitlements) entitlements=$2 ;;
                --identity) identity=$2 ;;
                --extra) extras+=("$2") ;;
            esac
            shift 2
            ;;
        --ui-element) ui_element=true; shift ;;
        *) usage ;;
    esac
done
[[ -n $bin && -n $id && -n $name && -n $out ]] || usage
[[ $id =~ ^io\.frostdev\.crosspane\.([A-Za-z0-9-]+\.)*[A-Za-z0-9-]+$ ]] || \
    fail 'Bundle ID must follow io.frostdev.crosspane.<name>'
case $name in .|..|*/*) fail 'NAME must be a single filename' ;; esac
[[ -f $bin && -r $bin ]] || fail "Cannot read binary: $bin"
[[ -z $entitlements || ( -f $entitlements && -r $entitlements ) ]] || \
    fail "Cannot read entitlements: $entitlements"
if [[ -z $identity ]]; then
    config=$HOME/src/crosspane/crosspane.local.toml
    [[ -r $config ]] || fail "Cannot read signing identity from $config; use --identity SHA1"
    identity=$(/usr/bin/sed -n '/^[[:space:]]*\[macos\][[:space:]]*$/,/^[[:space:]]*\[/p' "$config" |
        /usr/bin/sed -n 's/^[[:space:]]*signing_identity_sha1[[:space:]]*=[[:space:]]*"\([[:xdigit:]]*\)".*/\1/p')
fi
[[ $identity =~ ^[[:xdigit:]]{40}$ ]] || fail 'Signing identity must be a 40-character SHA1'

script_dir=$(cd -- "${BASH_SOURCE[0]%/*}" && pwd -P)
/bin/mkdir -p -- "$out"
out=$(cd -- "$out" && pwd -P)
stage=$(/usr/bin/mktemp -d "$out/.bundle.XXXXXXXX")
trap '/bin/rm -rf "$stage"' EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM
app=$stage/$name.app
/bin/mkdir -p "$app/Contents/MacOS"
/bin/cp -- "$bin" "$app/Contents/MacOS/$name"
/bin/chmod 755 "$app/Contents/MacOS/$name"
# Helper executables (e.g. crosspane-ui) live beside the main one and are signed on their own,
# without the main executable's entitlements, before the bundle is.
for extra in "${extras[@]+"${extras[@]}"}"; do
    /bin/cp -- "$extra" "$app/Contents/MacOS/${extra##*/}"
    /bin/chmod 755 "$app/Contents/MacOS/${extra##*/}"
done
# Homebrew libraries the executables link (e.g. libopus, WP-3.0) go inside the bundle and are
# signed with its identity: the hardened runtime refuses libraries signed by another team.
frameworks=$app/Contents/Frameworks
embedded=()
embed() {
    local exe=$1 lib base
    while read -r lib; do
        [[ $lib == /opt/homebrew/* || $lib == /usr/local/* ]] || continue
        base=${lib##*/}
        if [[ ! -f $frameworks/$base ]]; then
            /bin/mkdir -p "$frameworks"
            /bin/cp -L -- "$lib" "$frameworks/$base"
            /bin/chmod 755 "$frameworks/$base"
            /usr/bin/install_name_tool -id "@rpath/$base" "$frameworks/$base" 2>/dev/null
            embedded+=("$frameworks/$base")
            embed "$frameworks/$base"
        fi
        /usr/bin/install_name_tool -change "$lib" "@executable_path/../Frameworks/$base" "$exe" \
            2>/dev/null
    done < <(/usr/bin/otool -L "$exe" | /usr/bin/tail -n +2 | /usr/bin/awk '{print $1}')
}
embed "$app/Contents/MacOS/$name"
for extra in "${extras[@]+"${extras[@]}"}"; do
    embed "$app/Contents/MacOS/${extra##*/}"
done
plist=$app/Contents/Info.plist
/usr/bin/plutil -create xml1 "$plist"
for key in CFBundleIdentifier CFBundleName CFBundleExecutable CFBundlePackageType \
    CFBundleShortVersionString CFBundleVersion LSMinimumSystemVersion; do
    case $key in
        CFBundleIdentifier) value=$id ;;
        CFBundleName|CFBundleExecutable) value=$name ;;
        CFBundlePackageType) value=APPL ;;
        CFBundleShortVersionString) value=0.0.0 ;;
        CFBundleVersion) value=1 ;;
        LSMinimumSystemVersion) value=26.0 ;;
    esac
    /usr/bin/plutil -insert "$key" -string "$value" "$plist"
done
/usr/bin/plutil -insert NSHighResolutionCapable -bool true "$plist"
# macOS 15+ asks the user before an app may reach the local network; without this the agent's
# QUIC sends to LAN peers fail with "No route to host".
/usr/bin/plutil -insert NSLocalNetworkUsageDescription -string \
    'Crosspane connects to your other computers on the local network to share the keyboard, mouse and windows.' \
    "$plist"
if [[ $ui_element == true ]]; then
    /usr/bin/plutil -insert LSUIElement -bool true "$plist"
fi
sign=(/usr/bin/codesign --force --timestamp=none --options runtime -s "$identity")
for lib in "${embedded[@]+"${embedded[@]}"}"; do
    "$script_dir/run-in-gui.sh" -- "${sign[@]}" "$lib"
done
for extra in "${extras[@]+"${extras[@]}"}"; do
    "$script_dir/run-in-gui.sh" -- "${sign[@]}" "$app/Contents/MacOS/${extra##*/}"
done
if [[ -n $entitlements ]]; then
    sign+=(--entitlements "$entitlements")
fi
"$script_dir/run-in-gui.sh" -- "${sign[@]}" "$app"
/usr/bin/codesign --verify --strict "$app"
/bin/rm -rf -- "$out/$name.app"
/bin/mv -- "$app" "$out/$name.app"
printf '%s\n' "$out/$name.app"
