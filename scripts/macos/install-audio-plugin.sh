#!/bin/bash
# Install / uninstall the Crosspane audio driver (WP-3.4). PREPARED FOR THE OWNER: implementers
# never run this script. The owner runs it from the reviewed Mac worktree, after explicit approval
# for that run, with CROSSPANE_AUDIO_OWNER_ATTENDED=1:
#
#   scripts/macos/build-audio-plugin.sh --test
#   scripts/macos/sign-audio-plugin.sh --identity 'Apple Development: Created via API (UMB4CJ832G)'
#   CROSSPANE_AUDIO_OWNER_ATTENDED=1 scripts/macos/install-audio-plugin.sh --install target/macos-audio/CrosspaneAudio.driver
#   CROSSPANE_AUDIO_OWNER_ATTENDED=1 target/macos-audio/audio-loopback-probe --synthetic-speakers
#   CROSSPANE_AUDIO_OWNER_ATTENDED=1 scripts/macos/install-audio-plugin.sh --uninstall
#
# Options:
#   --install BUNDLE   validate BUNDLE and copy it to the one fixed destination
#   --uninstall        validate the installed driver's identity, then remove exactly that bundle
#   --team-id ID       expected Team ID (default: team_id under [macos] in
#                      ~/src/crosspane/crosspane.local.toml)
#   --check            do every validation and print the commands, but run no sudo command
#   --yes              skip the typed confirmation (otherwise INSTALL / UNINSTALL must be typed)
#
# What it does and refuses (all enforced below):
#   - The ONLY path ever written is /Library/Audio/Plug-Ins/HAL/CrosspaneAudio.driver. Other
#     plug-ins in that directory are never touched, listed or modified.
#   - Physical confinement: /Library, /Library/Audio, /Library/Audio/Plug-Ins and the HAL directory
#     must each be a real directory (not a symlink), owned by root, resolving to itself, with NO
#     group or other write bit (mode & 022) and NO ACL (`ls -lde` shows no "+" and no entry lines).
#     Checked at the start and again after the confirmation, for install and uninstall alike. The
#     script never changes the permissions of any of them; a failing one is simply refused.
#   - The bundle must be the exact reviewed driver: bundle id io.frostdev.crosspane.audio.driver,
#     executable CrosspaneAudio, only the plist, the executable and the signature's own files, no
#     symlinks or special files, no setuid bits, a valid strict signature whose identifier is the
#     bundle id, not ad hoc, and the expected Team ID.
#   - The destination is reserved ATOMICALLY and CLOSED: `sudo -n /bin/mkdir -m 0700 "$DEST"` (no
#     -p) fails if anything exists there, including something created while you were reading the
#     confirmation prompt, and a success leaves a root-owned directory that no non-root process can
#     enter. It stays 0700 through the copy, the recursive chown/chmod and the verification, so
#     nothing running as you can plant a hard link or swap an entry in the tree before it is
#     normalized. The copy is made with `ditto "$src/Contents" "$DEST/Contents"` so ditto never
#     touches the attributes of the closed top directory, and the top directory's owner and mode are
#     re-checked after each step. Only after verification passes is the directory exposed, by the
#     final `sudo -n /bin/chmod 0755 "$DEST"`, followed by one more verification as you.
#   - Verification of the closed tree is done through `sudo -n` with exact absolute commands
#     (/usr/bin/plutil, /usr/bin/find, /usr/bin/codesign, /bin/test): the alternative, exposing a
#     0755 view first, would publish the tree before its contents had been verified.
#   - Only a successful mkdir marks the destination as this run's; only then is cleanup armed, and
#     cleanup removes it only if it is still the very directory (device:inode) this run created.
#     An existing destination is never replaced: --uninstall first.
#   - Password prompts happen in exactly one place: `sudo -v` right after the confirmation. After
#     it, the final revalidation runs, and then EVERY privileged step (including cleanup) is
#     `sudo -n`, which never prompts, so no blocking prompt can sit between a check and the action
#     it guards. If the sudo credential expires mid-run, the step fails and cleanup tells you the
#     exact command to run by hand.
#   - --uninstall validates the installed driver's identity (bundle id, executable name, signing
#     identifier, Team ID, contents), asks for confirmation, authenticates, then validates it AGAIN
#     immediately before removing and requires it to be the same directory (device:inode) that was
#     validated.
#   - install and uninstall are serialized by a lock directory in a user-owned place
#     (~/Library/Caches/io.frostdev.crosspane/audio-plugin-install.lock). This is a per-user lock:
#     the script assumes a single-user Mac (this one is), where the owner is the only account that
#     can run it; it does not defend against another administrator racing it. A stale lock is
#     reported, never silently stolen.
#   - The only privileged commands are the ones printed before anything runs; the script itself
#     refuses to run as root. If the copy fails verification it is removed again BEFORE the audio
#     daemon is restarted.
#   - The audio daemon is restarted with `launchctl kickstart -k system/com.apple.audio.coreaudiod`
#     (all system audio is interrupted for a moment). No other process is killed or signalled. The
#     script changes no keychain item, TCC entry, system audio device or default device.
set -euo pipefail

readonly HAL_DIR=/Library/Audio/Plug-Ins/HAL
readonly DEST=$HAL_DIR/CrosspaneAudio.driver
readonly BUNDLE_ID=io.frostdev.crosspane.audio.driver
readonly EXE_NAME=CrosspaneAudio
readonly LOCK_DIR=$HOME/Library/Caches/io.frostdev.crosspane/audio-plugin-install.lock

usage() {
    echo 'Usage: CROSSPANE_AUDIO_OWNER_ATTENDED=1 install-audio-plugin.sh (--install BUNDLE | --uninstall) [--team-id ID] [--check] [--yes]' >&2
    exit 2
}
fail() {
    echo "install-audio-plugin: $*" >&2
    exit 1
}

action= src= team= assume_yes=false check_only=false
while [[ $# -gt 0 ]]; do
    case $1 in
        --install) [[ -z $action && $# -ge 2 && -n $2 ]] || usage; action=install; src=$2; shift 2 ;;
        --uninstall) [[ -z $action ]] || usage; action=uninstall; shift ;;
        --team-id) [[ $# -ge 2 && -n $2 ]] || usage; team=$2; shift 2 ;;
        --yes) assume_yes=true; shift ;;
        --check) check_only=true; shift ;;
        *) usage ;;
    esac
done
[[ -n $action ]] || usage
[[ ${CROSSPANE_AUDIO_OWNER_ATTENDED:-} == 1 ]] || \
    fail 'refusing to run: this is an explicitly owner-attended step (set CROSSPANE_AUDIO_OWNER_ATTENDED=1)'
[[ $(/usr/bin/uname -s) == Darwin ]] || fail 'macOS only'
[[ $EUID -ne 0 ]] || fail 'do not run as root: the script calls sudo only for the exact commands it prints'
[[ $DEST == /Library/Audio/Plug-Ins/HAL/CrosspaneAudio.driver ]] || fail 'internal error: destination changed'

if [[ -z $team ]]; then
    config=$HOME/src/crosspane/crosspane.local.toml
    [[ -r $config ]] || fail "cannot read the expected Team ID from $config; use --team-id"
    team=$(/usr/bin/sed -n '/^[[:space:]]*\[macos\][[:space:]]*$/,/^[[:space:]]*\[/p' "$config" |
        /usr/bin/sed -n 's/^[[:space:]]*team_id[[:space:]]*=[[:space:]]*"\([A-Z0-9]*\)".*/\1/p')
fi
[[ $team =~ ^[A-Z0-9]{10}$ ]] || fail 'the expected Team ID must be 10 upper-case letters/digits'

# ---- Cleanup and serialization --------------------------------------------------------------------
lock_held=false
reserved=false      # true only after `sudo mkdir "$DEST"` succeeded: the directory is this run's
reserved_id=        # its device:inode, captured right after the reservation

remove_reservation() {
    echo 'install did not complete: removing the destination this run reserved (the audio daemon is NOT restarted)' >&2
    if [[ -n $reserved_id && -d $DEST && ! -L $DEST && $(/usr/bin/stat -f '%d:%i' "$DEST") == "$reserved_id" ]]; then
        # sudo -n: never prompts, so nothing can block between the identity check and the removal.
        if ! /usr/bin/sudo -n /bin/rm -rf -- "$DEST"; then
            echo "could not remove $DEST (sudo credential expired?): run  sudo /bin/rm -rf -- $DEST  by hand" >&2
        fi
    else
        echo "$DEST is no longer the directory this run created: leaving it untouched" >&2
    fi
}
cleanup() {
    local status=$?
    set +e
    trap - EXIT
    if [[ $reserved == true ]]; then remove_reservation; fi
    if [[ $lock_held == true ]]; then /bin/rm -rf -- "$LOCK_DIR"; fi
    exit "$status"
}
trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

acquire_lock() {
    /bin/mkdir -p -- "${LOCK_DIR%/*}"
    if /bin/mkdir -- "$LOCK_DIR" 2>/dev/null; then
        printf '%s\n' "$$" > "$LOCK_DIR/pid"
        lock_held=true
        return 0
    fi
    local holder
    holder=$(/bin/cat "$LOCK_DIR/pid" 2>/dev/null || true)
    if [[ $holder =~ ^[0-9]+$ ]] && kill -0 "$holder" 2>/dev/null; then
        fail "another install/uninstall (pid $holder) holds $LOCK_DIR"
    fi
    fail "stale lock $LOCK_DIR (no running holder): check that no install is running, then remove that directory by hand"
}
acquire_lock

# ---- Validation -------------------------------------------------------------------------------------
# Every ancestor of the destination must be a real directory owned by root that nobody else can
# modify: no symlink can redirect a privileged write or removal, and no group/other write bit or
# ACL entry can let another account swap a path component. Never changes any permission.
check_physical_path() {
    local p mode listing
    for p in /Library /Library/Audio /Library/Audio/Plug-Ins "$HAL_DIR"; do
        [[ -d $p && ! -L $p ]] || fail "$p is missing or is a symlink: refusing (a missing HAL directory must be created by hand, as root:wheel, before installing)"
        [[ $(/usr/bin/stat -f %u "$p") == 0 ]] || fail "$p is not owned by root: refusing"
        [[ $(cd -P -- "$p" && /bin/pwd -P) == "$p" ]] || fail "$p does not resolve to itself: refusing"
        mode=$(/usr/bin/stat -f %Lp "$p") || fail "$p: cannot read its mode: refusing"
        [[ $mode =~ ^[0-7]+$ ]] || fail "$p: unreadable mode '$mode': refusing"
        (( (8#$mode & 8#022) == 0 )) || fail "$p is writable by group or others (mode $mode): refusing; this script never changes permissions"
        listing=$(/bin/ls -lde -- "$p") || fail "$p: cannot list it to look for ACLs: refusing"
        [[ -n $listing ]] || fail "$p: empty listing: refusing"
        [[ $(/usr/bin/wc -l <<< "$listing" | /usr/bin/tr -d ' ') == 1 ]] || fail "$p has ACL entries: refusing"
        [[ ${listing%% *} != *+ ]] || fail "$p has an ACL: refusing"
    done
}

# rt CMD...: run an exact absolute command, through `sudo -n` when validating the closed
# (root-owned, 0700) copy, directly otherwise.
view_root=false
rt() {
    if [[ $view_root == true ]]; then /usr/bin/sudo -n "$@"; else "$@"; fi
}

# validate_bundle PATH [root]: the identity and signature checks shared by install (source),
# uninstall (target) and the post-copy verification. With "root" every read of the tree goes
# through rt/sudo -n (the freshly installed copy is unreadable to this user until it is exposed).
validate_bundle() {
    local b=$1 plist exe details view=user
    local stray links specials setuids
    view_root=false
    if [[ ${2:-} == root ]]; then view_root=true; view=root; fi
    [[ -d $b && ! -L $b ]] || fail "$b is not a plain directory"
    plist=$b/Contents/Info.plist
    exe=$b/Contents/MacOS/$EXE_NAME
    rt /bin/test -f "$plist" || fail "$b has no Info.plist"
    if rt /bin/test -L "$plist"; then fail "$b: Info.plist is a symlink"; fi
    [[ $(rt /usr/bin/plutil -extract CFBundleIdentifier raw "$plist" 2>/dev/null) == "$BUNDLE_ID" ]] || \
        fail "$b: CFBundleIdentifier is not $BUNDLE_ID"
    [[ $(rt /usr/bin/plutil -extract CFBundleExecutable raw "$plist" 2>/dev/null) == "$EXE_NAME" ]] || \
        fail "$b: CFBundleExecutable is not $EXE_NAME"
    rt /bin/test -f "$exe" || fail "$b: executable missing"
    if rt /bin/test -L "$exe"; then fail "$b: the executable is a symlink"; fi
    # Exactly the reviewed contents: the plist, the executable, and the signature's own files.
    # Every scan is captured by an EXPLICITLY CHECKED assignment: a `find` that fails part-way
    # (permission error, vanished entry) must abort validation, never read as "nothing found".
    # (`local` is declared apart: `local x=$(cmd)` would hide cmd's exit status.)
    stray=$(rt /usr/bin/find "$b" -type f ! -path "$b/Contents/Info.plist" \
        ! -path "$b/Contents/MacOS/CrosspaneAudio" ! -path "$b/Contents/_CodeSignature/*" -print -quit) || \
        fail "$b: the scan for stray files failed (cannot traverse the bundle)"
    [[ -z $stray ]] || fail "$b contains files other than Info.plist, the executable and its signature"
    links=$(rt /usr/bin/find "$b" -type l -print -quit) || fail "$b: the scan for symbolic links failed (cannot traverse the bundle)"
    [[ -z $links ]] || fail "$b contains a symbolic link"
    specials=$(rt /usr/bin/find "$b" ! -type f ! -type d -print -quit) || fail "$b: the scan for special files failed (cannot traverse the bundle)"
    [[ -z $specials ]] || fail "$b contains a special file"
    setuids=$(rt /usr/bin/find "$b" -perm +6000 -print -quit) || fail "$b: the scan for setuid/setgid files failed (cannot traverse the bundle)"
    [[ -z $setuids ]] || fail "$b contains a setuid/setgid file"
    rt /usr/bin/codesign --verify --strict --deep --verbose=2 "$b" || fail "$b: strict signature verification failed"
    details=$(rt /usr/bin/codesign -dvv "$b" 2>&1) || fail "$b: reading the signature failed"
    [[ -n $details ]] || fail "$b: the signature description is empty"
    [[ $(/usr/bin/sed -n 's/^Identifier=//p' <<< "$details") == "$BUNDLE_ID" ]] || \
        fail "$b: signing identifier is not $BUNDLE_ID"
    [[ $(/usr/bin/grep -c '^Signature=adhoc' <<< "$details" || true) == 0 ]] || fail "$b: ad hoc signature"
    [[ $(/usr/bin/sed -n 's/^TeamIdentifier=//p' <<< "$details") == "$team" ]] || \
        fail "$b: Team ID is not $team"
    echo "   validated $b ($view view: bundle id $BUNDLE_ID, strict signature, Team $team)"
    view_root=false
}

# The closed reservation must still be exactly what this run created: root-owned, mode 0700.
require_closed() {
    [[ $(/usr/bin/stat -f '%u:%Lp' "$DEST") == 0:700 ]] || fail "$DEST is no longer root-owned mode 0700 ($(/usr/bin/stat -f '%u:%Lp' "$DEST")): refusing to continue"
    [[ $(/usr/bin/stat -f '%d:%i' "$DEST") == "$reserved_id" ]] || fail "$DEST is not the directory this run created: refusing to continue"
}

confirm() { # confirm WORD
    if [[ $check_only == true ]]; then
        echo '--check: validation finished, no command was run.'
        exit 0
    fi
    if [[ $assume_yes != true ]]; then
        local reply
        read -r -p "Type $1 to run exactly these commands: " reply
        [[ $reply == "$1" ]] || fail 'aborted; nothing was changed'
    fi
}

if [[ $action == install ]]; then
    src=${src%/}
    [[ -d $src && ! -L $src ]] || fail "$src is not a plain directory"
    src=$(cd -- "$src" && pwd -P)
    [[ ${src##*/} == CrosspaneAudio.driver ]] || fail 'the bundle must be named CrosspaneAudio.driver'
    echo '== validating the bundle and the destination path'
    validate_bundle "$src"
    check_physical_path
    if [[ -e $DEST || -L $DEST ]]; then
        fail "$DEST already exists: refusing to replace it. Run --uninstall first (it validates the identity before removing)."
    fi
    cat <<EOF

About to run, in this order. Only the lines starting with sudo use privileges. After the one
password prompt (sudo -v, first line) they all run as sudo -n, which never prompts:
  0. sudo -v                              (the only place a password can be asked for)
     then the final revalidation of the source, the destination path and the absent destination
  1. sudo -n /bin/mkdir -m 0700 "$DEST"   (no -p: fails if anything exists there; success reserves
     it as a root-owned directory no other process can enter)
  2. sudo -n /usr/bin/ditto --norsrc --noextattr --noqtn "$src/Contents" "$DEST/Contents"
  3. sudo -n /usr/sbin/chown -R root:wheel "$DEST"
  4. sudo -n /bin/chmod -R go-w "$DEST"
  5. verification of the still-closed tree as root: sudo -n with /usr/bin/plutil, /usr/bin/find,
     /usr/bin/codesign --verify --strict --deep, and the identity/Team/contents checks
     (if anything fails the reserved directory is removed again, only if it is still the one
     step 1 created: sudo -n /bin/rm -rf -- "$DEST"; the audio daemon is NOT restarted)
  6. sudo -n /bin/chmod 0755 "$DEST"      (the final exposure step), then one more verification as you
  7. sudo -n /bin/launchctl kickstart -k system/com.apple.audio.coreaudiod
Step 7 restarts the system audio daemon: all audio on this Mac is interrupted for a moment.
Nothing else under $HAL_DIR is touched. No default device, TCC entry or keychain item is changed.
EOF
    confirm INSTALL
    # The one place a password prompt can happen. Everything below is `sudo -n`: no blocking prompt
    # can sit between a check and the action it guards.
    /usr/bin/sudo -v || fail 'sudo authentication failed: nothing was changed'
    # Everything may have changed while the prompt was open: check again, then reserve atomically.
    check_physical_path
    validate_bundle "$src"
    [[ ! -e $DEST && ! -L $DEST ]] || fail "$DEST appeared while waiting for confirmation: refusing"
    /usr/bin/sudo -n /bin/mkdir -m 0700 "$DEST" # no -p: succeeds only if this run created the directory
    reserved=true
    reserved_id=$(/usr/bin/stat -f '%d:%i' "$DEST") || fail "cannot identify the reserved $DEST"
    [[ $reserved_id =~ ^[0-9]+:[0-9]+$ ]] || fail "unreadable identity '$reserved_id' for the reserved $DEST"
    require_closed
    # Copy the contents, never the top directory: ditto must not touch the closed directory's attributes.
    /usr/bin/sudo -n /usr/bin/ditto --norsrc --noextattr --noqtn "$src/Contents" "$DEST/Contents"
    require_closed
    /usr/bin/sudo -n /usr/sbin/chown -R root:wheel "$DEST"
    /usr/bin/sudo -n /bin/chmod -R go-w "$DEST"
    require_closed
    echo '== verifying the closed copy (as root, through sudo -n)'
    validate_bundle "$DEST" root
    require_closed
    /usr/bin/sudo -n /bin/chmod 0755 "$DEST" # the final exposure step: only now can other accounts enter
    echo '== verifying the exposed copy (as you)'
    validate_bundle "$DEST"
    reserved=false # from here the copy stays, even if the daemon restart fails
    /usr/bin/sudo -n /bin/launchctl kickstart -k system/com.apple.audio.coreaudiod
    echo "installed $DEST and restarted coreaudiod."
    echo 'Next: CROSSPANE_AUDIO_OWNER_ATTENDED=1 target/macos-audio/audio-loopback-probe --synthetic-speakers'
else
    if [[ ! -e $DEST && ! -L $DEST ]]; then
        echo "nothing to uninstall: $DEST is absent"
        exit 0
    fi
    check_physical_path
    [[ -d $DEST && ! -L $DEST ]] || fail "$DEST is not a plain directory: refusing to remove it"
    echo '== validating the installed driver identity'
    validate_bundle "$DEST"
    validated_id=$(/usr/bin/stat -f '%d:%i' "$DEST") || fail "cannot identify the validated $DEST"
    [[ $validated_id =~ ^[0-9]+:[0-9]+$ ]] || fail "unreadable identity '$validated_id' for the validated $DEST"
    cat <<EOF

About to run, in this order. Only the lines starting with sudo use privileges. After the one
password prompt (sudo -v) they all run as sudo -n, which never prompts:
  0. sudo -v, then validate the installed driver again (no privileges needed to read it) and require
     it to be the same directory that was just validated
  1. sudo -n /bin/rm -rf -- "$DEST"
  2. sudo -n /bin/launchctl kickstart -k system/com.apple.audio.coreaudiod
Step 2 restarts the system audio daemon: all audio on this Mac is interrupted for a moment.
Nothing else under $HAL_DIR is touched.
EOF
    confirm UNINSTALL
    /usr/bin/sudo -v || fail 'sudo authentication failed: nothing was changed'
    # Revalidate identity IMMEDIATELY before the removal, after the only password prompt: from here
    # every privileged step is `sudo -n`, so nothing can block between this check and the removal.
    check_physical_path
    [[ -d $DEST && ! -L $DEST ]] || fail "$DEST changed while waiting for confirmation: refusing to remove it"
    validate_bundle "$DEST"
    [[ $(/usr/bin/stat -f '%d:%i' "$DEST") == "$validated_id" ]] || \
        fail "$DEST is not the directory that was validated: refusing to remove it"
    /usr/bin/sudo -n /bin/rm -rf -- "$DEST"
    [[ ! -e $DEST && ! -L $DEST ]] || fail "$DEST is still present after removal"
    /usr/bin/sudo -n /bin/launchctl kickstart -k system/com.apple.audio.coreaudiod
    echo "removed $DEST and restarted coreaudiod."
fi
