#!/bin/bash
# Ordinary-user assembly only. The test recorder can never produce a production manifest.
set -euo pipefail
umask 022
export PATH=/usr/bin:/bin:/usr/sbin:/sbin LC_ALL=C
[[ $EUID != 0 && $# == 3 ]] || { printf 'usage: build.sh CrosspaneAudio.driver TEAM_ID output-directory\n' >&2; exit 1; }
base=$(cd -- "${BASH_SOURCE%/*}" && /bin/pwd -P)
driver=$1; team=$2; out=$3
[[ $driver == /* && $out == /* && $team =~ ^[A-Z0-9]{10}$ ]] || exit 1
manifest=packages.json
if [[ ${CROSSPANE_AUDIO_PKG_TEST_CODESIGN+x} ]]; then
    [[ -n $CROSSPANE_AUDIO_PKG_TEST_CODESIGN ]] || exit 1
    manifest=packages.test.json
fi
readonly CODESIGN=${CROSSPANE_AUDIO_PKG_TEST_CODESIGN-/usr/bin/codesign}
[[ $CODESIGN == /* && -f $CODESIGN && -x $CODESIGN && ! -L $CODESIGN ]] || exit 1
source "$base/common.sh"
driver_tree "$driver" no || exit 1
version=$(/usr/bin/plutil -extract CFBundleShortVersionString raw "$driver/Contents/Info.plist") || exit 1
[[ $version =~ ^[0-9]{1,4}\.[0-9]{1,4}\.[0-9]{1,4}$ ]] || exit 1
details=$("$CODESIGN" -dvvv "$driver" 2>&1) || exit 1
hash=$(/usr/bin/sed -n 's/^CDHash=//p' <<< "$details") || exit 1
[[ $hash =~ ^[0-9a-f]{40}$ ]] || exit 1
verify_driver "$driver" "$version" "$team" "$hash" no || exit 1
work=$(/usr/bin/mktemp -d); /bin/chmod 0700 "$work"
trap '/bin/rm -rf -- "$work"' EXIT
/bin/mkdir -m 0700 "$work/payload" "$work/install" "$work/remove"
/usr/bin/ditto --norsrc --noextattr --noqtn "$driver" "$work/payload/CrosspaneAudio.driver"
verify_driver "$work/payload/CrosspaneAudio.driver" "$version" "$team" "$hash" no || exit 1
for kind in install remove; do
    /bin/cp "$base/common.sh" "$work/$kind/common.sh"
    for phase in preinstall postinstall; do
        /usr/bin/sed -e "s/@VERSION@/$version/g" -e "s/@TEAM_ID@/$team/g" -e "s/@CDHASH@/$hash/g" "$base/$kind/$phase" > "$work/$kind/$phase"
        if /usr/bin/grep -Eq '@[A-Z_]+@' "$work/$kind/$phase"; then exit 1; fi
        /bin/chmod 0755 "$work/$kind/$phase"
    done
done
/bin/mkdir -p "$out"
/usr/bin/pkgbuild --root "$work/payload" --component-plist "$base/component.plist" \
    --identifier io.frostdev.crosspane.audio.install --version "$version" \
    --install-location '/Library/Application Support/Crosspane/Installer/staging' \
    --scripts "$work/install" --ownership recommended "$out/CrosspaneAudio-install-$version.pkg"
/usr/bin/pkgbuild --nopayload --scripts "$work/remove" \
    --identifier io.frostdev.crosspane.audio.remove --version "$version" "$out/CrosspaneAudio-remove-$version.pkg"
install_hash=$(/usr/bin/shasum -a 256 "$out/CrosspaneAudio-install-$version.pkg"); install_hash=${install_hash%% *}
remove_hash=$(/usr/bin/shasum -a 256 "$out/CrosspaneAudio-remove-$version.pkg"); remove_hash=${remove_hash%% *}
printf '{"schema_version":1,"version":"%s","packages":[{"kind":"install","file":"CrosspaneAudio-install-%s.pkg","sha256":"%s"},{"kind":"remove","file":"CrosspaneAudio-remove-%s.pkg","sha256":"%s"}]}\n' \
    "$version" "$version" "$install_hash" "$version" "$remove_hash" > "$out/$manifest"
