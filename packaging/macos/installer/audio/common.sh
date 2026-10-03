# Shared functions only; root entry points authenticate this file before sourcing it.
stat_numbers() {
    local value; local -a fields formats
    value=$(/usr/bin/stat -f "$1" "$2") || return 1
    [[ $value =~ ^[0-9]+(\ [0-9]+)*$ ]] || return 1
    read -r -a fields <<< "$value"; read -r -a formats <<< "$1"
    [[ ${#fields[@]} == ${#formats[@]} ]] || return 1
    printf '%s\n' "$value"
}
safe_dir() {
    local p=$1 uid mode acl values physical
    [[ -d $p && ! -L $p ]] || return 1
    values=$(stat_numbers '%u %Lp' "$p") || return 1
    read -r uid mode <<< "$values"
    [[ $mode =~ ^[0-7]+$ ]] || return 1
    [[ $uid == "$ROOT_UID" ]] && (( (8#$mode & 022) == 0 )) || return 1
    acl=$(/bin/ls -lde "$p") || return 1
    [[ $acl != *$'\n'* && ${acl%% *} != *+ ]] || return 1
    physical=$(cd -P -- "$p" && /bin/pwd -P) || return 1
    [[ $physical == "$p" ]]
}
ours() {
    local p=$1 uid identity
    [[ -d $p && ! -L $p ]] || return 1
    uid=$(stat_numbers %u "$p") || return 1
    identity=$(/usr/bin/plutil -extract CFBundleIdentifier raw "$p/Contents/Info.plist") || return 1
    [[ $uid == "$ROOT_UID" && $identity == io.frostdev.crosspane.audio.driver ]]
}
present() { [[ -e $1 || -L $1 ]]; }
installer_ok() {
    # F8 is stricter than the ancestor rule: refuse, never repair, a non-0755/wheel INST.
    local values mode group expected=${OWNER##*:}
    safe_dir "$INST" || return 1
    values=$(stat_numbers '%p %g' "$INST") || return 1
    read -r mode group <<< "$values"
    if [[ $expected == wheel ]]; then expected=0; fi
    [[ $expected =~ ^[0-9]+$ && $mode == 40755 && $group == "$expected" ]]
}
paths() {
    ROOT="$SYS/Library/Application Support/Crosspane"; INST="$ROOT/Installer"
    STAGING="$INST/staging"; STAGED="$STAGING/CrosspaneAudio.driver"
    PREVIOUS="$INST/previous"; SLOT="$PREVIOUS/CrosspaneAudio.driver"
    DISCARD="$INST/discard"; HAL="$SYS/Library/Audio/Plug-Ins/HAL"
    INSTALLED="$HAL/CrosspaneAudio.driver"
    OUTCOME="$INST/audio-outcome.json"; REMOVAL_OUTCOME="$INST/audio-removal-outcome.json"
}
previous_ok() {
    local mode
    safe_dir "$PREVIOUS" || return 1
    mode=$(stat_numbers %Lp "$PREVIOUS") || return 1
    [[ $mode =~ ^[0-7]+$ ]] && (( (8#$mode & 077) == 0 )) || return 1
    (shopt -s nullglob dotglob
     local p
     for p in "$PREVIOUS"/*; do [[ $p == "$SLOT" ]] || return 1; done)
}
admit() {
    local p
    [[ $EUID == "$ROOT_UID" && ${1:-} == / ]] || return 1
    paths
    for p in "$SYS/Library" "$SYS/Library/Audio" "$SYS/Library/Audio/Plug-Ins" "$HAL" "$SYS/Library/Application Support"; do safe_dir "$p" || return 1; done
    for p in "$ROOT" "$INST"; do
        if present "$p"; then safe_dir "$p" || return 1; fi
    done
    if present "$STAGING" && [[ ${2:-} != skip-staging ]]; then safe_dir "$STAGING" || return 1; fi
    if present "$INST"; then installer_ok || return 1; fi
    if present "$PREVIOUS"; then previous_ok || return 1; fi
    if present "$INSTALLED"; then ours "$INSTALLED" || return 1; fi
    # Before mkdir/cleanup, admit the nearest existing parent of each absent directory.
    same_volume "${2:-}"
}
prepare() {
    local p
    for p in "$ROOT" "$INST"; do
        if ! present "$p"; then /bin/mkdir -m 0755 "$p" && /usr/sbin/chown "$OWNER" "$p" || return 1; fi
        safe_dir "$p" || return 1
        if [[ $p == "$INST" ]]; then installer_ok || return 1; fi
    done
    remove_bundle "$DISCARD"
}
on_volume() {
    local p=$1 device hal_device
    while ! present "$p"; do [[ $p != / ]] || return 1; p=${p%/*}; [[ -n $p ]] || p=/; done
    safe_dir "$p" || return 1
    device=$(stat_numbers %d "$p") || return 1
    hal_device=$(stat_numbers %d "$HAL") || return 1
    [[ $device == "$hal_device" ]]
}
same_volume() {
    local p
    for p in "$ROOT" "$INST" "$STAGING" "$PREVIOUS"; do
        # Unsafe STAGING gets no mutation, but may publish verify_failed in admitted INST.
        if [[ $p == "$STAGING" && ${1:-} == skip-staging ]]; then continue; fi
        on_volume "$p" || return 1
    done
}
rename_ok() {
    local from=${1%/*} to=${2%/*}
    safe_dir "$from" && safe_dir "$to" && on_volume "$from" && on_volume "$to"
}
remove_bundle() {
    # No trailing slash: root-only-parent entries, including links, are unlinked without following.
    case $1 in
        "$STAGED") safe_dir "$STAGING" && /bin/rm -rf -- "$STAGED" ;;
        "$DISCARD") safe_dir "$INST" && /bin/rm -rf -- "$DISCARD" ;;
        *) return 1 ;;
    esac
}
driver_tree() {
    local p=$1 owned=$2 q uid mode links size values
    [[ -d $p && ! -L $p ]] || return 1
    for q in . Contents Contents/MacOS Contents/_CodeSignature; do [[ -d $p/$q && ! -L $p/$q ]] || return 1; done
    for q in Contents/Info.plist Contents/MacOS/CrosspaneAudio Contents/_CodeSignature/CodeResources; do [[ -f $p/$q && ! -L $p/$q ]] || return 1; done
    (cd -- "$p" && /usr/bin/find . -print0) | while IFS= read -r -d '' q; do
        case $q in .|./Contents|./Contents/MacOS|./Contents/_CodeSignature|./Contents/Info.plist|./Contents/MacOS/CrosspaneAudio|./Contents/_CodeSignature/CodeResources) ;; *) return 1 ;; esac
        # %Lp omits set-ID bits on Darwin; %p retains them for the F3 06000 check.
        values=$(stat_numbers '%u %p %l %z' "$p/$q") || return 1
        read -r uid mode links size <<< "$values"
        [[ $mode =~ ^[0-7]+$ ]] || return 1
        (( (8#$mode & 06000) == 0 )) || return 1
        if [[ $owned == yes ]]; then [[ $uid == "$ROOT_UID" ]] && (( (8#$mode & 022) == 0 )) || return 1; fi
        if [[ -f $p/$q ]]; then [[ $links == 1 ]] && (( size <= 16777216 )) || return 1; fi
    done
}
verify_driver() {
    local p=$1 version=$2 team=$3 hash=$4 owned=$5 details identity executable extracted_version extracted_hash
    driver_tree "$p" "$owned" || return 1
    identity=$(/usr/bin/plutil -extract CFBundleIdentifier raw "$p/Contents/Info.plist") || return 1
    executable=$(/usr/bin/plutil -extract CFBundleExecutable raw "$p/Contents/Info.plist") || return 1
    extracted_version=$(/usr/bin/plutil -extract CFBundleShortVersionString raw "$p/Contents/Info.plist") || return 1
    [[ $identity == io.frostdev.crosspane.audio.driver && $executable == CrosspaneAudio &&
       $extracted_version == "$version" ]] || return 1
    "$CODESIGN" --verify --strict --deep "-R=anchor apple generic and identifier \"io.frostdev.crosspane.audio.driver\" and certificate leaf[subject.OU] = \"$team\"" "$p" || return 1
    details=$("$CODESIGN" -dvvv "$p" 2>&1) || return 1
    extracted_hash=$(/usr/bin/sed -n 's/^CDHash=//p' <<< "$details") || return 1
    ! /usr/bin/grep -q '^Signature=adhoc' <<< "$details" &&
        [[ $extracted_hash == "$hash" ]]
}
outcome_leaf() { ! present "$1" || [[ -f $1 && ! -L $1 ]]; }
outcome() {
    local result=$1 version=$2 path=$3 line seconds
    installer_ok && outcome_leaf "$path" && outcome_leaf "$INST/.outcome.tmp" || return 1
    seconds=$(/bin/date -u +%s) || return 1
    [[ $seconds =~ ^[0-9]+$ ]] || return 1
    if [[ $path == "$OUTCOME" ]]; then
        line=$(printf '{"schema_version":1,"result":"%s","driver_version":"%s","at_unix_ms":%s000}' "$result" "$version" "$seconds")
    elif [[ $path == "$REMOVAL_OUTCOME" ]]; then
        line=$(printf '{"schema_version":1,"result":"%s","at_unix_ms":%s000}' "$result" "$seconds")
    else return 1; fi
    (( ${#line} + 1 <= 256 )) || return 1
    /bin/rm -f -- "$INST/.outcome.tmp" || return 1
    outcome_leaf "$path" && outcome_leaf "$INST/.outcome.tmp" || return 1
    (set -o noclobber; printf '%s\n' "$line" > "$INST/.outcome.tmp") || return 1
    /usr/sbin/chown "$OWNER" "$INST/.outcome.tmp" && /bin/chmod 0644 "$INST/.outcome.tmp" &&
        installer_ok && rename_ok "$INST/.outcome.tmp" "$path" &&
        outcome_leaf "$path" && outcome_leaf "$INST/.outcome.tmp" && /bin/mv -f -- "$INST/.outcome.tmp" "$path"
}
