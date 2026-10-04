#!/bin/bash
# Tier 1 build only. Existing GUI signing helper jobs are admitted; no product service or app runs.
# Same-UID changes inside the private build parent are outside this build-time shell threat model.
set -euo pipefail
umask 077
fail() { echo "stage-mac: $*" >&2; exit 1; }
[[ $# == 1 && $(uname -s) == Darwin && $(uname -m) == arm64 && $EUID != 0 ]] || \
    fail 'usage on an ordinary-user Apple Silicon Mac: stage-mac.sh /absolute/fresh-private-output-dir'
root=$(cd -- "${BASH_SOURCE[0]%/*}/../.." && pwd -P)
cd "$root"
out=$1
# Build artifacts stay in this disposable mac-sync tree, never in the deployed application.
# -I: isolated mode ignores PYTHON* (PYTHONOPTIMIZE would otherwise strip these checks).
python3 -I - "$root" "$out" <<'PY'
import os, pathlib, stat, sys
def require(ok, message):
    if not ok:
        sys.exit("stage-mac: " + message)
root, out = map(pathlib.Path, sys.argv[1:])
require(out.is_absolute() and out.parent.is_dir() and not out.exists() and not out.is_symlink(),
        "output must be a fresh absolute path in an existing directory")
require(out.parent.resolve() == out.parent and out.parent.is_relative_to(root / "target"),
        "output parent must be inside this tree's target, without symlinks")
p = out.parent.stat()
require(p.st_uid == os.getuid() and not stat.S_IMODE(p.st_mode) & 0o022,
        "output parent must be user-owned and private from writers")
PY
[[ ! ${CROSSPANE_AUDIO_PKG_TEST_CODESIGN+x} ]] || fail 'test-only package recorder is forbidden'
config=$HOME/src/crosspane/crosspane.local.toml
[[ -r $config ]] || fail 'the existing local signing configuration is missing'
identity=$(/usr/bin/sed -n '/^[[:space:]]*\[macos\][[:space:]]*$/,/^[[:space:]]*\[/p' "$config" |
    /usr/bin/sed -n 's/^[[:space:]]*signing_identity_sha1[[:space:]]*=[[:space:]]*"\([[:xdigit:]]*\)".*/\1/p')
team=$(/usr/bin/sed -n '/^[[:space:]]*\[macos\][[:space:]]*$/,/^[[:space:]]*\[/p' "$config" |
    /usr/bin/sed -n 's/^[[:space:]]*team_id[[:space:]]*=[[:space:]]*"\([A-Z0-9]*\)".*/\1/p')
[[ $identity =~ ^[[:xdigit:]]{40}$ && $team =~ ^[A-Z0-9]{10}$ ]] || fail 'invalid local Apple Development identity/Team'
export OPUS_LIB_DIR=${OPUS_LIB_DIR:-$(brew --prefix opus)}
export CARGO_BUILD_JOBS=2
export CARGO_TARGET_DIR=$root/target
work=$(mktemp -d "${out%/*}/.stage-mac.XXXXXXXX")
trap 'rm -rf -- "$work"' EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM
result=$work/result
payload=$work/payload
mkdir -p "$result" "$payload"
# The parent remains 0700; conventional bundle leaf modes must be 0644/0755.
umask 022
features=private-vdisplay,video
echo 'stage-mac: building release payload (no inventory in bootstrap)'
env -u CROSSPANE_MAC_APPROVED_INVENTORY cargo build --release --locked -p crosspane-agent --features "$features"
env -u CROSSPANE_MAC_APPROVED_INVENTORY cargo build --release --locked -p crosspanectl -p crosspane-ui -p crosspane-installer --bins
# The producer's health check compares the agent's own CARGO_PKG_VERSION with this value.
version=$(cargo metadata --locked --no-deps --format-version 1 | python3 -I -c \
    'import json,sys; print(next(p["version"] for p in json.load(sys.stdin)["packages"] if p["name"]=="crosspane-agent"))')
[[ -n $version ]] || fail 'could not read the agent version'

gui=$root/scripts/macos/run-in-gui.sh
sign() { "$gui" -- /usr/bin/codesign --force --timestamp=none --options runtime -s "$identity" "$@"; }
# This script compiles and statically checks the CFPlugIn only; it never registers or loads it.
bash scripts/macos/build-audio-plugin.sh
"$gui" -- /usr/bin/codesign --force --timestamp=none -s "$identity" \
    --identifier io.frostdev.crosspane.audio.driver "$root/target/macos-audio/CrosspaneAudio.driver"
bash packaging/macos/installer/audio/build.sh "$root/target/macos-audio/CrosspaneAudio.driver" "$team" "$work/audio"
bash scripts/macos/bundle.sh --bin target/release/crosspane-agent --id io.frostdev.crosspane.agent \
    --name Crosspane --out "$payload" --ui-element --extra target/release/crosspane-ui \
    --extra target/release/crosspane-tutorial --entitlements packaging/macos/crosspane-agent.entitlements \
    --identity "$identity"
audio=$payload/Crosspane.app/Contents/Resources/audio
mkdir -p "$audio"
cp "$work/audio/packages.json" "$work/audio/"*.pkg "$audio/"
chmod 644 "$audio/"*
# Resource staging changes the outer seal, so reseal using the established flags and identity.
sign --entitlements packaging/macos/crosspane-agent.entitlements "$payload/Crosspane.app"
for bin in crosspanectl crosspane-installer; do
    cp "target/release/$bin" "$payload/$bin"
    chmod 755 "$payload/$bin"
    # A bare payload executable has no Frameworks folder: under the hardened runtime it may load
    # only system libraries (a Homebrew dylib would be refused at launch).
    libs=$(/usr/bin/otool -L "$payload/$bin" | /usr/bin/tail -n +2 | /usr/bin/awk '{print $1}')
    [[ -n $libs ]] || fail "cannot read the libraries of $bin"
    while read -r lib; do
        [[ $lib == /usr/lib/* || $lib == /System/Library/* ]] || fail "$bin links a non-system library: $lib"
    done <<< "$libs"
    case $bin in
        crosspanectl) identifier=io.frostdev.crosspane.ctl ;;
        crosspane-installer) identifier=io.frostdev.crosspane.installer ;;
    esac
    sign --identifier "$identifier" "$payload/$bin"
done
echo 'stage-mac: generating inventory from signed payload'
tool=$root/target/release/crosspane-mac-inventory
"$tool" "$payload" --product-version "$version" --features "$features" --team-id "$team" > "$result/ApprovedInventory.json"
inventory=$(cat "$result/ApprovedInventory.json")
[[ -n $inventory ]] || fail 'inventory generator produced no inventory'
echo 'stage-mac: compiling outer installer with embedded inventory'
CROSSPANE_MAC_APPROVED_INVENTORY=$inventory cargo build --release --locked -p crosspane-installer --bin crosspane-installer
bash scripts/macos/bundle.sh --bin target/release/crosspane-installer --id io.frostdev.crosspane.installer \
    --name 'Crosspane Installer' --out "$result" --identity "$identity"
app="$result/Crosspane Installer.app"
mkdir -p "$app/Contents/Resources"
mv "$payload" "$app/Contents/Resources/payload"
sign "$app"
/usr/bin/codesign --verify --deep --strict "$app"
# Check the completed copy too, before publication. Runtime never reads this adjacent sample.
"$tool" "$app/Contents/Resources/payload" --product-version "$version" --features "$features" --team-id "$team" > "$work/rechecked.json"
cmp "$result/ApprovedInventory.json" "$work/rechecked.json"
python3 -I - "$result" <<'PY'
import hashlib, pathlib, sys
root = pathlib.Path(sys.argv[1])
with (root / "SHA256SUMS").open("x") as sums:
    for p in sorted(root.rglob("*")):
        if p.is_file() and p.name != "SHA256SUMS":
            with p.open("rb") as leaf:
                digest = hashlib.sha256()
                for block in iter(lambda: leaf.read(65536), b""):
                    digest.update(block)
            sums.write(digest.hexdigest() + "  " + str(p.relative_to(root)) + "\n")
PY
chmod 644 "$result/ApprovedInventory.json" "$result/SHA256SUMS"
[[ ! -e $out && ! -L $out ]] || fail 'output appeared during build; refusing publication'
mv "$result" "$out"
echo "stage-mac: signed Tier 1 installer ready at $out/Crosspane Installer.app"
