#!/bin/bash
# All execution is ordinary-user scratch copies. Only the five frozen headers are rewritten.
set -euo pipefail
umask 022
export PATH=/usr/bin:/bin:/usr/sbin:/sbin LC_ALL=C
[[ $EUID != 0 && ( $# == 0 || ( $# == 1 && $1 == --packages-only ) || ( $# == 2 && $1 == --case ) ) ]] || exit 1
base=$(cd -- "${BASH_SOURCE%/*}/.." && /bin/pwd -P)
work=$(/usr/bin/mktemp -d /tmp/crosspane-audio-fixture.XXXXXXXX)
work=$(cd -- "$work" && /bin/pwd -P); /bin/chmod 0700 "$work"
SYS="$work/sys"; ROOT_UID=$EUID; OWNER="$EUID:$(/usr/bin/id -g)"; CODESIGN="$work/recorder"
cleanup() {
    local p="$SYS/Library/Audio/Plug-Ins/HAL"
    if [[ -d $p && ! -L $p ]]; then /bin/chmod u+w "$p"; fi
    /bin/rm -rf -- "$work"
}
trap cleanup EXIT
fixture="$work/CrosspaneAudio.driver"; team=ABCDEFGHIJ; hash=0123456789abcdef0123456789abcdef01234567
/bin/mkdir -p "$fixture/Contents/MacOS"
/bin/cp /usr/bin/true "$fixture/Contents/MacOS/CrosspaneAudio"
printf '%s\n' '<?xml version="1.0"?><plist version="1.0"><dict><key>CFBundleIdentifier</key><string>io.frostdev.crosspane.audio.driver</string><key>CFBundleExecutable</key><string>CrosspaneAudio</string><key>CFBundleShortVersionString</key><string>0.1.0</string><key>CFBundlePackageType</key><string>BNDL</string></dict></plist>' > "$fixture/Contents/Info.plist"
/usr/bin/codesign --force -s - "$fixture"
printf '%s\n' '#!/bin/bash' 'set -euo pipefail' 'base=${BASH_SOURCE%/*}' \
    'status=0; [[ ! -e $base/refuse ]] || status=1' \
    'if [[ $1 == restart && -e $base/restart-refused ]]; then status=1; fi' \
    'if [[ $1 == -dvvv && -e $base/details-refuse ]]; then status=1; fi' \
    'printf "%s\0" "$#" "$@" "$status" >> "$base/calls"' \
    '[[ $1 != -dvvv ]] || printf "CDHash=0123456789abcdef0123456789abcdef01234567\n"' \
    'exit "$status"' > "$CODESIGN"
/bin/chmod 0755 "$CODESIGN"
if [[ ${1:-} == --packages-only ]]; then /bin/bash "$base/tests/package-check.sh" "$fixture" "$CODESIGN"; exit; fi
source "$base/common.sh"
reset_case() {
    local pin=$hash
    [[ $name != cdhash_mismatch ]] || pin=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
    /bin/rm -rf -- "$SYS" "$work/copies"; /bin/rm -f "$work/refuse" "$work/restart-refused" "$work/details-refuse" "$work/calls"
    /bin/mkdir -p "$SYS/Library/Audio/Plug-Ins/HAL" "$SYS/Library/Application Support" "$work/copies/install" "$work/copies/remove"
    paths
    /bin/mkdir "$HAL/OtherVendor.plugin" "$SYS/Library/Application Support/OtherVendor"
    printf 'other HAL vendor\n' > "$HAL/OtherVendor.plugin/sentinel"
    printf 'other support vendor\n' > "$SYS/Library/Application Support/OtherVendor/sentinel"
    snapshot "$HAL/OtherVendor.plugin" > "$work/hal-vendor"
    snapshot "$SYS/Library/Application Support/OtherVendor" > "$work/support-vendor"
    for kind in install remove; do
        /bin/cp "$base/common.sh" "$work/copies/$kind/common.sh"
        for phase in preinstall postinstall; do
            /usr/bin/sed -e "s/@VERSION@/0.1.0/g" -e "s/@TEAM_ID@/$team/g" -e "s/@CDHASH@/$pin/g" \
                -e "s|^readonly SYS=.*|readonly SYS='$SYS'|" -e "s/^readonly ROOT_UID=.*/readonly ROOT_UID=$ROOT_UID/" \
                -e "s|^readonly OWNER=.*|readonly OWNER='$OWNER'|" -e "s|^readonly RESTART=.*|readonly RESTART=('$CODESIGN' restart)|" \
                -e "s|^readonly CODESIGN=.*|readonly CODESIGN='$CODESIGN'|" "$base/$kind/$phase" > "$work/copies/$kind/$phase"
            /usr/bin/sed -e '/^readonly SYS=/d' -e '/^readonly ROOT_UID=/d' -e '/^readonly OWNER=/d' -e '/^readonly RESTART=/d' -e '/^readonly CODESIGN=/d' "$work/copies/$kind/$phase" > "$work/audit-copy"
            /usr/bin/sed -e "s/@VERSION@/0.1.0/g" -e "s/@TEAM_ID@/$team/g" -e "s/@CDHASH@/$pin/g" \
                -e '/^readonly SYS=/d' -e '/^readonly ROOT_UID=/d' -e '/^readonly OWNER=/d' -e '/^readonly RESTART=/d' -e '/^readonly CODESIGN=/d' "$base/$kind/$phase" > "$work/audit-source"
            /usr/bin/cmp "$work/audit-copy" "$work/audit-source"
            ! /usr/bin/grep -q '/usr/bin/killall' "$work/copies/$kind/$phase"
        done
    done
}
run_script() {
    /bin/bash -p "$work/copies/$1/$2" '' '' "${3:-/}" || return 1
    if [[ $1 == install && $2 == postinstall ]]; then hal_is published; fi
}
stage() { /usr/bin/ditto --norsrc --noextattr --noqtn "$fixture" "$STAGED"; /bin/cp "$STAGED/Contents/_CodeSignature/CodeResources" "$work/published"; }
current() {
    /usr/bin/ditto --norsrc --noextattr --noqtn "$fixture" "$INSTALLED"
    # A differs from the staged B/C, so overwriting the backup cannot pass the preservation oracle.
    printf 'retained working A\n' > "$INSTALLED/Contents/_CodeSignature/CodeResources"
    /bin/cp "$INSTALLED/Contents/_CodeSignature/CodeResources" "$work/A"
}
preserved_A() {
    for file in Contents/Info.plist Contents/MacOS/CrosspaneAudio; do /usr/bin/cmp "$fixture/$file" "$SLOT/$file"; done
    /usr/bin/cmp "$work/A" "$SLOT/Contents/_CodeSignature/CodeResources"
    driver_tree "$SLOT" yes
}
snapshot() {
    local p=$1 q
    (cd -- "$p" && /usr/bin/find . -print | /usr/bin/sort) | while IFS= read -r q; do
        printf '%s\n' "$q"
        /usr/bin/stat -f '%u %g %p %i %l %z %m %c %f' "$p/$q"
        if [[ -f $p/$q ]]; then /usr/bin/shasum -a 256 < "$p/$q"; fi
    done
}
hal_is() {
    local expected=$1 actual
    actual=$(/usr/bin/find "$HAL" -mindepth 1 -maxdepth 1 -exec /usr/bin/basename '{}' \; | /usr/bin/sort)
    if [[ $expected == absent ]]; then [[ $actual == OtherVendor.plugin && ! -e $INSTALLED ]]; return; fi
    [[ $actual == $'CrosspaneAudio.driver\nOtherVendor.plugin' ]] && driver_tree "$INSTALLED" yes || return 1
    for file in Contents/Info.plist Contents/MacOS/CrosspaneAudio; do /usr/bin/cmp "$fixture/$file" "$INSTALLED/$file" || return 1; done
    /usr/bin/cmp "$work/$expected" "$INSTALLED/Contents/_CodeSignature/CodeResources" || return 1
    if [[ $expected == published ]]; then [[ ! -e $STAGED && ! -L $STAGED ]]; fi
}
calls_exact() {
    local argc arg status pending='' index; local -a args
    [[ -f $work/calls ]] || return 0
    while IFS= read -r -d '' argc; do
        [[ $argc =~ ^[0-9]+$ ]] || return 1
        args=(); for (( index=0; index<argc; index++ )); do IFS= read -r -d '' arg || return 1; args+=("$arg"); done
        IFS= read -r -d '' status || return 1
        [[ $status == 0 || $status == 1 ]] || return 1
        if [[ -n $pending ]]; then
            [[ $argc == 2 && ${args[0]} == -dvvv && ${args[1]} == "$pending" ]] || return 1
            pending=''
        elif [[ ${args[0]} == --verify ]]; then
            [[ $argc == 5 && ${args[1]} == --strict && ${args[2]} == --deep &&
               ${args[3]} == "-R=anchor apple generic and identifier \"io.frostdev.crosspane.audio.driver\" and certificate leaf[subject.OU] = \"$team\"" && ${args[4]} == "$STAGED" ]] || return 1
            if [[ $status == 0 ]]; then pending=${args[4]}; fi
        elif [[ $name == build_*_failure ]]; then
            [[ $argc == 2 && ${args[0]} == -dvvv && ${args[1]} == "$fixture" ]] || return 1
        else [[ $argc == 1 && ${args[0]} == restart ]] || return 1; fi
    done < "$work/calls"
    [[ -z $pending ]]
}
inject() {
    # One deterministic scratch mutation between admission and a guarded operation.
    /bin/bash -p -c 'set -T; cut=$2; mutation=$3; fixture=$4; leaf=$5; fired=0; trap '\''if [[ $fired == 0 && $BASH_COMMAND == "$cut" ]]; then fired=1; eval "$mutation"; fi'\'' DEBUG; source "$1" "" "" /' _ "$work/copies/$1/postinstall" "$2" "$3" "$fixture" "${leaf:-}"
}
result_is() {
    local p=$1 result=$2 n line expected now
    n=$(/usr/bin/plutil -extract at_unix_ms raw "$p"); now=$(/bin/date -u +%s)000
    [[ $n =~ ^[0-9]+000$ ]] && (( n <= now + 2000 && n >= now - 2000 )) || return 1
    if [[ $p == "$OUTCOME" ]]; then
        expected=$(printf '{"schema_version":1,"result":"%s","driver_version":"0.1.0","at_unix_ms":%s}' "$result" "$n")
    else expected=$(printf '{"schema_version":1,"result":"%s","at_unix_ms":%s}' "$result" "$n"); fi
    line=$(/bin/cat "$p")
    [[ $line == "$expected" && $(/usr/bin/stat -f %p "$p") == 100644 && $(/usr/bin/stat -f %u "$p") == "$ROOT_UID" ]] &&
        (( $(/usr/bin/wc -c < "$p") == ${#line} + 1 && ${#line} + 1 <= 256 ))
}
fails() { if "$@" > "$work/failure.log" 2>&1; then /bin/cat "$work/failure.log"; return 1; fi; }
interrupt() {
    # DEBUG is installed by the harness, without adding a sixth script seam or editing code.
    /bin/bash -p -c 'set -T; cut=$2; trap '\''[[ $BASH_COMMAND != "$cut" ]] || exit 77'\'' DEBUG; source "$1" "" "" /' _ "$work/copies/$1/postinstall" "$2"
}
interrupted() {
    local code=0
    interrupt "$@" > "$work/interruption.log" 2>&1 || code=$?
    [[ $code == 77 ]] || { /bin/cat "$work/interruption.log"; return 1; }
    case $name in
        interrupt_before_previous|interrupt_before_save) hal_is A ;;
        interrupt_before_publish) hal_is absent ;;
        *) hal_is published ;;
    esac
}
fail_move() {
    # Change only this scratch HAL after admission, forcing mv to fail while INST stays writable.
    /bin/bash -p -c 'set -T; cut=$2; trap '\''if [[ $BASH_COMMAND == "$cut" ]]; then /bin/chmod 0555 "$SYS/Library/Audio/Plug-Ins/HAL"; fi'\'' DEBUG; source "$1" "" "" /' _ "$work/copies/$1/postinstall" "$2"
}
ordered() {
    /bin/bash -p -c 'set -T; trace=$2; trap '\''printf "%s\n" "$BASH_COMMAND" >> "$trace"'\'' DEBUG; source "$1" "" "" /' _ "$work/copies/install/postinstall" "$work/order"
    hal_is published
}
position() {
    local p first previous n line
    p=$(/usr/bin/grep -n -F -- "$1" "$work/order") || { printf 'Missing trace command: %s\n' "$1" >&2; return 1; }
    first=${p%%:*}; previous=$first
    # functrace reports a function invocation twice consecutively (call and entry).
    while IFS= read -r line; do n=${line%%:*}; (( n == previous || n == previous + 1 )) || return 1; previous=$n; done <<< "$p"
    printf '%s\n' "$first"
}
count=0; tested=0
while name=$(/usr/bin/plutil -extract "cases.$count" raw "$base/tests/transaction-fixtures.json" 2>/dev/null); do
    if [[ ${1:-} == --case && $name != "$2" ]]; then count=$(( count + 1 )); continue; fi
    reset_case
    case $name in
        ours_plist_failure|verify_identity_failure|verify_executable_failure|verify_version_failure|verify_hash_failure|verify_details_failure)
            run_script install preinstall; stage; current; field=CFBundleIdentifier
            case $name in verify_executable_failure) field=CFBundleExecutable ;; verify_version_failure) field=CFBundleShortVersionString ;; verify_hash_failure) field=hash ;; verify_details_failure) field=details; printf x > "$work/details-refuse" ;; esac
            snapshot "$SYS" > "$work/before"
            if [[ $name == ours_plist_failure ]]; then
                fails /bin/bash -p -c '/usr/bin/plutil() { command /usr/bin/plutil "$@" || return; return 1; }; source "$1" "" "" /' _ "$work/copies/install/preinstall"
            else
                fails /bin/bash -c 'ROOT_UID=$1; CODESIGN=$2; bad=$3; source "$4"; /usr/bin/plutil() { command /usr/bin/plutil "$@" || return; [[ $2 != "$bad" ]]; }; /usr/bin/sed() { command /usr/bin/sed "$@" || return; [[ $bad != hash ]]; }; verify_driver "$5" 0.1.0 ABCDEFGHIJ "$6" yes' _ "$ROOT_UID" "$CODESIGN" "$field" "$base/common.sh" "$STAGED" "$hash"
            fi
            snapshot "$SYS" > "$work/after"; /usr/bin/cmp "$work/before" "$work/after" ;;
        build_version_failure|build_details_failure|build_hash_failure)
            if [[ $name == build_details_failure ]]; then printf x > "$work/details-refuse"; fi
            fails /bin/bash -c 'bad=$1; CROSSPANE_AUDIO_PKG_TEST_CODESIGN=$2; /usr/bin/plutil() { command /usr/bin/plutil "$@" || return; [[ $bad != build_version_failure ]]; }; /usr/bin/sed() { command /usr/bin/sed "$@" || return; [[ $bad != build_hash_failure ]]; }; source "$3" "$4" ABCDEFGHIJ "$5"' _ "$name" "$CODESIGN" "$base/build.sh" "$fixture" "$work/refused-build"
            [[ ! -e $work/refused-build ]] ;;
        discard_install_no_staging|discard_remove_link|discard_remove_directory)
            run_script remove preinstall; current
            if [[ $name == discard_remove_directory ]]; then /bin/mkdir "$DISCARD"; else /bin/ln -s "$fixture" "$DISCARD"; fi
            kind=remove; [[ $name != discard_install_no_staging ]] || kind=install
            run_script "$kind" preinstall; [[ ! -e $DISCARD && ! -L $DISCARD && -d $fixture ]]
            if [[ $kind == install ]]; then stage; run_script install postinstall; else run_script remove postinstall; fi ;;
        outcome_link|outcome_directory|tmp_link|tmp_directory|outcome_rename_race|tmp_rename_race)
            run_script remove preinstall; leaf=$REMOVAL_OUTCOME
            if [[ $name == tmp_* ]]; then leaf="$INST/.outcome.tmp"; fi
            if [[ $name == *_race ]]; then
                fails inject remove 'rename_ok "$INST/.outcome.tmp" "$path"' '/bin/rm -f -- "$leaf"; /bin/ln -s "$fixture" "$leaf"'
            else
                if [[ $name == *_directory ]]; then /bin/mkdir "$leaf"; else /bin/ln -s "$fixture" "$leaf"; fi
                fails run_script remove postinstall
            fi
            [[ -d $fixture && ! -e $fixture/.outcome.tmp ]] ;;
        date_status|date_nonnumeric)
            run_script remove preinstall
            fails /bin/bash -c 'SYS=$1; ROOT_UID=$2; OWNER=$3; source "$4"; paths; bad=$5; /bin/date() { [[ $bad != date_status ]] || return 1; printf "bad\n"; }; outcome absent "" "$REMOVAL_OUTCOME"' _ "$SYS" "$ROOT_UID" "$OWNER" "$base/common.sh" "$name"
            [[ ! -e $REMOVAL_OUTCOME && ! -e $INST/.outcome.tmp ]] ;;
        stat_status|stat_nonnumeric)
            fails /bin/bash -p -c 'bad=$2; /usr/bin/stat() { [[ $bad != stat_status ]] || return 1; printf "bad\n"; }; source "$1" "" "" /' _ "$work/copies/install/preinstall" "$name"
            fails /bin/bash -c 'ROOT_UID=$1; source "$2"; bad=$3; /usr/bin/stat() { [[ $bad != stat_status ]] || return 1; printf "bad\n"; }; safe_dir "$4"' _ "$ROOT_UID" "$base/common.sh" "$name" "$HAL"
            [[ ! -e $ROOT ]] ;;
        volume_before_prepare)
            fails /bin/bash -p -c 'foreign=$2; /usr/bin/stat() { if [[ $2 == %d && $3 == "$foreign" ]]; then printf "0\n"; else command /usr/bin/stat "$@"; fi; }; source "$1" "" "" /' _ "$work/copies/remove/preinstall" "$SYS/Library/Application Support"
            [[ ! -e $ROOT ]] ;;
        installer_wrong_group|installer_special_bits)
            run_script install preinstall
            if [[ $name == installer_wrong_group ]]; then
                # Only the frozen header is changed: existing INST remains the ordinary user's group.
                /usr/bin/sed "s/^readonly OWNER=.*/readonly OWNER='$EUID:0'/" "$work/copies/install/preinstall" > "$work/changed"
                /bin/mv "$work/changed" "$work/copies/install/preinstall"
            else /bin/chmod 2755 "$INST"; fi
            fails run_script install preinstall; [[ ! -e $OUTCOME ]]
            if [[ $name == installer_special_bits ]]; then [[ $(/usr/bin/stat -f %p "$INST") == 42755 ]]; fi ;;
        privilege_refusal)
            /usr/bin/sed "s/^readonly ROOT_UID=.*/readonly ROOT_UID=$(( EUID + 1 ))/" "$work/copies/install/preinstall" > "$work/changed"
            /bin/mv "$work/changed" "$work/copies/install/preinstall"
            fails run_script install preinstall; [[ ! -e $ROOT ]] ;;
        environment_ignored)
            /usr/bin/env SYS=/ignored ROOT_UID=0 OWNER=invalid CODESIGN=/bin/false \
                /bin/bash -p "$work/copies/install/preinstall" '' '' /
            [[ -d $STAGING && ! -e $OUTCOME ]] ;;
        remove_without_staging)
            run_script remove preinstall; [[ -d $INST && ! -e $STAGING ]]
            run_script remove postinstall; result_is "$REMOVAL_OUTCOME" absent ;;
        foreign_installed|remove_foreign)
            current; /usr/bin/plutil -replace CFBundleIdentifier -string foreign "$INSTALLED/Contents/Info.plist"
            snapshot "$INSTALLED" > "$work/foreign-before"
            kind=install; [[ $name == foreign_installed ]] || kind=remove
            fails run_script "$kind" preinstall; [[ ! -e $ROOT && -d $INSTALLED ]]
            snapshot "$INSTALLED" > "$work/foreign-after"; /usr/bin/cmp "$work/foreign-before" "$work/foreign-after" ;;
        unsafe_ancestor|ancestor_acl|non_system_volume|common_link|common_mode|script_directory_link|script_directory_mode)
            case $name in
                unsafe_ancestor) /bin/chmod 0777 "$SYS/Library/Audio" ;;
                ancestor_acl) /bin/chmod +a "user:$(/usr/bin/id -un) allow read" "$SYS/Library/Audio" ;;
                common_link) /bin/mv "$work/copies/install/common.sh" "$work/common"; /bin/ln -s "$work/common" "$work/copies/install/common.sh" ;;
                common_mode) /bin/chmod 0666 "$work/copies/install/common.sh" ;;
                script_directory_link) /bin/mv "$work/copies/install" "$work/entry"; /bin/ln -s "$work/entry" "$work/copies/install" ;;
                script_directory_mode) /bin/chmod 0777 "$work/copies/install" ;;
            esac
            target=/; [[ $name != non_system_volume ]] || target="$SYS"
            fails run_script install preinstall "$target"; [[ ! -e $ROOT ]] ;;
        remove_absent|remove_previous_only|remove_current_previous|remove_failed)
            run_script install preinstall
            if [[ $name != remove_absent ]]; then current; /bin/mkdir -m 0700 "$PREVIOUS"; /bin/mv "$INSTALLED" "$SLOT"; fi
            if [[ $name == remove_current_previous || $name == remove_failed ]]; then current; fi
            run_script remove preinstall
            if [[ $name == remove_failed ]]; then
                fails fail_move remove '/bin/mv -- "$INSTALLED" "$DISCARD"'
                result_is "$REMOVAL_OUTCOME" remove_failed; [[ -d $INSTALLED && -d $SLOT ]]
                /bin/chmod 0755 "$HAL"
            else
                run_script remove postinstall; result=removed; [[ $name != remove_absent ]] || result=absent
                result_is "$REMOVAL_OUTCOME" "$result"; [[ ! -e $INSTALLED && ! -e $PREVIOUS && -d $STAGING ]]
            fi ;;
        *)
            run_script install preinstall; stage
            if [[ $name == preinstall_cleans_links ]]; then
                /bin/rm -rf -- "$STAGED"; /bin/ln -s "$fixture" "$STAGED"; /bin/ln -s "$fixture" "$DISCARD"
                run_script install preinstall
                [[ ! -e $STAGED && ! -L $STAGED && ! -e $DISCARD && ! -L $DISCARD && -d $fixture ]]
                stage
            fi
            case $name in
                current_and_empty_previous) current; /bin/mkdir -m 0700 "$PREVIOUS" ;;
                move_failed_after_save|exact_transaction_order|volume_before_rename) current ;;
                discard_rename_race) current; /bin/mkdir -m 0700 "$PREVIOUS"; /bin/mv "$INSTALLED" "$SLOT"; current ;;
                occupied_slot_retry|verify_failed_preserves_A|interrupt_*)
                    current
                    if [[ $name != interrupt_before_previous && $name != interrupt_before_save && $name != interrupt_before_publish ]]; then
                        /bin/mkdir -m 0700 "$PREVIOUS"; /bin/mv "$INSTALLED" "$SLOT"; current
                    fi ;;
            esac
            case $name in
                staged_symlink) /bin/rm -rf "$STAGED"; /bin/ln -s "$fixture" "$STAGED" ;;
                staged_wrong_id) /usr/bin/plutil -replace CFBundleIdentifier -string foreign "$STAGED/Contents/Info.plist" ;;
                staged_missing_plist) /bin/rm "$STAGED/Contents/Info.plist" ;;
                unsafe_staging) /bin/chmod 0777 "$STAGING"; fails run_script install preinstall; [[ ! -e $OUTCOME ]] ;;
                previous_conflict) /bin/mkdir -m 0700 "$PREVIOUS"; printf x > "$PREVIOUS/foreign" ;;
                tree_extra) printf x > "$STAGED/extra" ;;
                tree_symlink) /bin/rm "$STAGED/Contents/MacOS/CrosspaneAudio"; /bin/ln -s "$fixture/Contents/MacOS/CrosspaneAudio" "$STAGED/Contents/MacOS/CrosspaneAudio" ;;
                tree_hardlink) /bin/ln "$STAGED/Contents/MacOS/CrosspaneAudio" "$work/hardlink" ;;
                tree_special) /bin/rm "$STAGED/Contents/MacOS/CrosspaneAudio"; /usr/bin/mkfifo "$STAGED/Contents/MacOS/CrosspaneAudio" ;;
                tree_setid) /bin/chmod 4755 "$STAGED/Contents/MacOS/CrosspaneAudio" ;;
                tree_setgid) /bin/chmod 2755 "$STAGED/Contents/MacOS/CrosspaneAudio" ;;
                tree_writable) /bin/chmod 0666 "$STAGED/Contents/MacOS/CrosspaneAudio" ;;
                tree_oversize) /bin/dd if=/dev/zero of="$STAGED/Contents/MacOS/CrosspaneAudio" bs=1048576 count=17 2>/dev/null ;;
                owner_admission) fails /bin/bash -c 'ROOT_UID=$1; source "$2"; driver_tree "$3" yes' _ "$(( EUID + 1 ))" "$base/common.sh" "$STAGED" ;;
                real_adhoc_refusal)
                    /usr/bin/codesign --verify --strict --deep "$fixture"
                    fails /usr/bin/codesign --verify --strict --deep "-R=anchor apple generic and identifier \"io.frostdev.crosspane.audio.driver\" and certificate leaf[subject.OU] = \"$team\"" "$fixture"
                    /usr/bin/sed 's|^readonly CODESIGN=.*|readonly CODESIGN=/usr/bin/codesign|' "$work/copies/install/postinstall" > "$work/changed"; /bin/mv "$work/changed" "$work/copies/install/postinstall" ;;
                verify_failed_preserves_A) printf x > "$work/refuse" ;;
                restart_refused) printf x > "$work/restart-refused" ;;
            esac
            case $name in
                volume_before_rename)
                    fails inject install 'rename_ok "$INSTALLED" "$SLOT"' '/usr/bin/stat() { if [[ $2 == %d && $3 == "$PREVIOUS" ]]; then printf "0\n"; else command /usr/bin/stat "$@"; fi; }'
                    result_is "$OUTCOME" move_failed; hal_is A ;;
                discard_rename_race)
                    fails inject install 'rename_ok "$INSTALLED" "$DISCARD"' '/bin/ln -s "$fixture" "$DISCARD"'
                    result_is "$OUTCOME" move_failed; hal_is A; preserved_A ;;
                exact_transaction_order)
                    ordered; result_is "$OUTCOME" installed; preserved_A
                    verify=$(position '"$CODESIGN" --verify --strict --deep '); describe=$(position 'details=$("$CODESIGN" -dvvv')
                    save=$(position '/bin/mv -- "$INSTALLED" "$SLOT"'); publish=$(position '/bin/mv -- "$STAGED" "$INSTALLED"')
                    restart=$(position '"${RESTART[@]}"'); written=$(position 'outcome installed '\''0.1.0'\'' "$OUTCOME"')
                    printf 'Order positions: verify=%s describe=%s save=%s publish=%s restart=%s outcome=%s\n' "$verify" "$describe" "$save" "$publish" "$restart" "$written"
                    (( verify < describe && describe < save && save < publish && publish < restart && restart < written )) ;;
                move_failed_after_save)
                    fails fail_move install '/bin/mv -- "$STAGED" "$INSTALLED"'
                    result_is "$OUTCOME" move_failed; [[ ! -e $INSTALLED && -d $STAGED ]]; preserved_A
                    /bin/chmod 0755 "$HAL"; run_script install preinstall; stage
                    run_script install postinstall; result_is "$OUTCOME" installed; preserved_A ;;
                interrupt_outcome_publish)
                    outcome installed 0.1.0 "$OUTCOME"; old=$(/usr/bin/stat -f %i "$OUTCOME")
                    interrupted install '/bin/mv -f -- "$INST/.outcome.tmp" "$path"'
                    [[ $(/usr/bin/stat -f %i "$OUTCOME") == "$old" && -f $INST/.outcome.tmp ]]
                    run_script install preinstall; stage; run_script install postinstall
                    result_is "$OUTCOME" installed; [[ $(/usr/bin/stat -f %i "$OUTCOME") != "$old" ]] ;;
                staged_*|unsafe_staging|tree_*|cdhash_mismatch|real_adhoc_refusal|verify_failed_preserves_A)
                    fails run_script install postinstall; result_is "$OUTCOME" verify_failed
                    if [[ $name == unsafe_staging ]]; then [[ -d $STAGED ]]; /bin/chmod 0700 "$STAGING"; else [[ ! -e $STAGED && ! -L $STAGED ]]; fi
                    if [[ $name == verify_failed_preserves_A ]]; then
                        preserved_A; /bin/rm "$work/refuse"; stage
                        printf 'retry C\n' >> "$STAGED/Contents/_CodeSignature/CodeResources"
                        /bin/cp "$STAGED/Contents/_CodeSignature/CodeResources" "$work/published"
                        run_script install postinstall; preserved_A
                    fi ;;
                previous_conflict) fails run_script install postinstall; [[ ! -e $OUTCOME ]] ;;
                interrupt_*)
                    case $name in
                        interrupt_before_previous) cut='/bin/mkdir -m 0700 "$PREVIOUS"' ;;
                        interrupt_before_save) cut='/bin/mv -- "$INSTALLED" "$SLOT"' ;;
                        interrupt_before_publish) cut='/bin/mv -- "$STAGED" "$INSTALLED"' ;;
                        interrupt_before_discard_cleanup) cut='/bin/rm -rf -- "$DISCARD"' ;;
                        interrupt_before_restart) cut='"${RESTART[@]}"' ;;
                        interrupt_before_outcome) cut='outcome installed '\''0.1.0'\'' "$OUTCOME"' ;;
                    esac
                    interrupted install "$cut"
                    if [[ -d $SLOT ]]; then preserved_A; fi
                    run_script install preinstall; stage; run_script install postinstall
                    result_is "$OUTCOME" installed; preserved_A ;;
                *) run_script install postinstall; result_is "$OUTCOME" installed
                    if [[ $name == current_and_empty_previous || $name == occupied_slot_retry ]]; then preserved_A; fi
                    if [[ $name == outcome_replaced ]]; then
                        old=$(/usr/bin/stat -f %i "$OUTCOME"); stage; run_script install postinstall
                        [[ $(/usr/bin/stat -f %i "$OUTCOME") != "$old" ]]
                    fi ;;
            esac ;;
    esac
    calls_exact
    snapshot "$HAL/OtherVendor.plugin" > "$work/hal-after"; /usr/bin/cmp "$work/hal-vendor" "$work/hal-after"
    snapshot "$SYS/Library/Application Support/OtherVendor" > "$work/support-after"; /usr/bin/cmp "$work/support-vendor" "$work/support-after"
    printf 'PASS %s\n' "$name"; count=$(( count + 1 )); tested=$(( tested + 1 ))
done
[[ $count == 75 && $tested != 0 ]]
if [[ ${1:-} != --case ]]; then /bin/bash "$base/tests/package-check.sh" "$fixture" "$CODESIGN"; fi
printf 'PASS transactions: %s ordinary-user cases; native audio/service operations: 0\n' "$tested"
