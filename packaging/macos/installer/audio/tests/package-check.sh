#!/bin/bash
# Inspection of explicitly supplied inert fixtures; never install either package.
set -euo pipefail
umask 022
export PATH=/usr/bin:/bin:/usr/sbin:/sbin LC_ALL=C
[[ $EUID != 0 && $# == 2 && $1 == /* && $2 == /* ]] || { printf 'usage: package-check.sh inert-driver codesign-recorder\n' >&2; exit 1; }
base=$(cd -- "${BASH_SOURCE%/*}/.." && /bin/pwd -P)
fixture=$1; recorder=$2; team=ABCDEFGHIJ
version=$(/usr/bin/plutil -extract CFBundleShortVersionString raw "$fixture/Contents/Info.plist")
hash=$("$recorder" -dvvv "$fixture" | /usr/bin/sed -n 's/^CDHash=//p')
work=$(/usr/bin/mktemp -d); trap '/bin/rm -rf -- "$work"' EXIT
if /usr/bin/env CROSSPANE_AUDIO_PKG_TEST_CODESIGN='' /bin/bash "$base/build.sh" "$fixture" "$team" "$work/empty"; then exit 1; fi
[[ ! -e $work/empty ]]
/usr/bin/env CROSSPANE_AUDIO_PKG_TEST_CODESIGN="$recorder" /bin/bash "$base/build.sh" "$fixture" "$team" "$work/out"
[[ -f $work/out/packages.test.json && ! -e $work/out/packages.json ]]
expected=$(printf '%s\n' CrosspaneAudio.driver CrosspaneAudio.driver/Contents CrosspaneAudio.driver/Contents/MacOS \
    CrosspaneAudio.driver/Contents/_CodeSignature CrosspaneAudio.driver/Contents/Info.plist \
    CrosspaneAudio.driver/Contents/MacOS/CrosspaneAudio CrosspaneAudio.driver/Contents/_CodeSignature/CodeResources | /usr/bin/sort)
for kind in install remove; do
    pkg="$work/out/CrosspaneAudio-$kind-$version.pkg"
    /usr/sbin/pkgutil --expand "$pkg" "$work/$kind"
    info=$(/bin/cat "$work/$kind/PackageInfo")
    [[ $info == *"identifier=\"io.frostdev.crosspane.audio.$kind\""* && $info == *"version=\"$version\""* ]]
    if [[ $kind == install ]]; then
        [[ $info == *'install-location="/Library/Application Support/Crosspane/Installer/staging"'* ]]
        actual=$(/usr/sbin/pkgutil --payload-files "$pkg" | /usr/bin/sed 's#^\./##; /^\.$/d' | /usr/bin/sort)
        [[ $actual == "$expected" ]]
        owners=$(/usr/bin/lsbom -p u "$work/$kind/Bom" | /usr/bin/sort -u)
        [[ $owners == 0 ]] || { printf 'FAIL payload owners: %s\n' "$owners"; exit 1; }
        /usr/bin/lsbom -p m "$work/$kind/Bom" | while read -r mode; do (( (8#$mode & 022) == 0 )); done
        /usr/sbin/pkgutil --expand-full "$pkg" "$work/full"
        for file in Contents/Info.plist Contents/MacOS/CrosspaneAudio Contents/_CodeSignature/CodeResources; do
            /usr/bin/cmp "$fixture/$file" "$work/full/Payload/CrosspaneAudio.driver/$file"
        done
    else [[ ! -e $work/$kind/Payload ]]; fi
    /usr/bin/cmp "$base/common.sh" "$work/$kind/Scripts/common.sh"
    for phase in preinstall postinstall; do
        /usr/bin/sed -e "s/@VERSION@/$version/g" -e "s/@TEAM_ID@/$team/g" -e "s/@CDHASH@/$hash/g" "$base/$kind/$phase" > "$work/rendered"
        script="$work/$kind/Scripts/$phase"
        /usr/bin/cmp "$work/rendered" "$script"
        for header in "readonly SYS=''                                # prefix of every fixed path" \
            'readonly ROOT_UID=0' 'readonly OWNER=root:wheel' 'readonly RESTART=(/usr/bin/killall coreaudiod)' \
            'readonly CODESIGN=/usr/bin/codesign'; do /usr/bin/grep -Fx "$header" "$script" > /dev/null; done
        ! /usr/bin/grep -Eq '@[A-Z_]+@|CROSSPANE_AUDIO_PKG_TEST_CODESIGN' "$script"
    done
    digest=$(/usr/bin/shasum -a 256 "$pkg"); digest=${digest%% *}
    if [[ $kind == install ]]; then install_hash=$digest; else remove_hash=$digest; fi
    index=0; [[ $kind == install ]] || index=1
    [[ $(/usr/bin/plutil -extract packages.$index.sha256 raw "$work/out/packages.test.json") == "$digest" ]]
done
manifest=$(printf '{"schema_version":1,"version":"%s","packages":[{"kind":"install","file":"CrosspaneAudio-install-%s.pkg","sha256":"%s"},{"kind":"remove","file":"CrosspaneAudio-remove-%s.pkg","sha256":"%s"}]}' "$version" "$version" "$install_hash" "$version" "$remove_hash")
[[ $(/bin/cat "$work/out/packages.test.json") == "$manifest" ]]
(( $(/usr/bin/wc -c < "$work/out/packages.test.json") == ${#manifest} + 1 && ${#manifest} + 1 <= 1024 ))
/usr/bin/cmp "$work/install/Scripts/common.sh" "$work/remove/Scripts/common.sh"
[[ $(/usr/bin/plutil -extract 0.RootRelativeBundlePath raw "$base/component.plist") == CrosspaneAudio.driver ]]
for key in BundleIsRelocatable BundleIsVersionChecked; do [[ $(/usr/bin/plutil -extract "0.$key" raw "$base/component.plist") == false ]]; done
[[ $(/usr/bin/plutil -extract 0.BundleHasStrictIdentifier raw "$base/component.plist") == true &&
   $(/usr/bin/plutil -extract 0.BundleOverwriteAction raw "$base/component.plist") == upgrade ]]
# The actual package must encode the relocation choice, rather than relying only on source data.
[[ $(/bin/cat "$work/install/PackageInfo") != *'<relocate'* ]]
printf 'PASS package-check: 2 inert packages, exact payload/scripts/manifest; no installation\n'
