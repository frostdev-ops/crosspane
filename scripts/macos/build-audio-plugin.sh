#!/bin/bash
# Build the Crosspane audio driver (WP-3.4) and its detached direct tests. Builds only: this script
# never signs, installs, loads or registers anything and never touches the audio server.
#
#   scripts/macos/build-audio-plugin.sh            driver bundle + owner probe + static checks
#   scripts/macos/build-audio-plugin.sh --test     ... plus the direct tests, run
#   scripts/macos/build-audio-plugin.sh --asan     ... plus the tests built with ASan+UBSan, run
#   scripts/macos/build-audio-plugin.sh --tsan     ... plus the tests built with TSan, run
#
# Outputs (under target/macos-audio/):
#   CrosspaneAudio.driver       the unsigned driver bundle (sign it with sign-audio-plugin.sh)
#   audio-loopback-probe        the owner probe (unsigned until sign-audio-plugin.sh runs)
#   tests/ asan/ tsan/          the direct test binaries of the respective mode
#
# The driver links only libSystem and CoreFoundation, has no IPC and no helper process; the static
# checks below fail the build if that ever changes.
set -euo pipefail

usage() {
    echo 'Usage: build-audio-plugin.sh [--test | --asan | --tsan]' >&2
    exit 2
}
fail() {
    echo "build-audio-plugin: $*" >&2
    exit 1
}

mode=build
case ${1:-} in
    '') ;;
    --test) mode=test ;;
    --asan) mode=asan ;;
    --tsan) mode=tsan ;;
    *) usage ;;
esac
[[ $# -le 1 ]] || usage
[[ $(/usr/bin/uname -s) == Darwin ]] || fail 'macOS only'

script_dir=$(cd -- "${BASH_SOURCE[0]%/*}" && pwd -P)
root=$(cd -- "$script_dir/../.." && pwd -P)
src=$root/macos/crosspane-audio
out=$root/target/macos-audio
sdk=$(/usr/bin/xcrun --show-sdk-path)
cc=(/usr/bin/xcrun clang)
warn=(-Wall -Wextra -Wshadow -Wstrict-prototypes -Wundef -Werror)
common=(-std=c11 "${warn[@]}" -arch arm64 -mmacosx-version-min=26.0 -isysroot "$sdk" -g -fno-omit-frame-pointer)

header=$src/src/crosspane_audio.h
define() { /usr/bin/sed -n "s/^#define $1 \"\\(.*\\)\"\$/\\1/p" "$header"; }
bundle_id=$(define CROSSPANE_AUDIO_BUNDLE_ID)
factory_uuid=$(define CROSSPANE_AUDIO_FACTORY_UUID)
[[ $bundle_id == io.frostdev.crosspane.audio.driver ]] || fail "unexpected bundle id '$bundle_id'"
[[ $factory_uuid =~ ^[0-9A-F]{8}-[0-9A-F]{4}-[0-9A-F]{4}-[0-9A-F]{4}-[0-9A-F]{12}$ ]] || fail "bad factory UUID '$factory_uuid'"

/bin/mkdir -p "$out"

# ---- The driver bundle ---------------------------------------------------------------------------
bundle=$out/CrosspaneAudio.driver
exe=$bundle/Contents/MacOS/CrosspaneAudio
/bin/rm -rf "$bundle"
/bin/mkdir -p "$bundle/Contents/MacOS"
echo "== building $bundle"
# -g0: no debug info, so the linker does not drop a .dSYM next to the executable inside the bundle.
"${cc[@]}" "${common[@]}" -g0 -O2 -fvisibility=hidden -fstack-protector-strong -bundle \
    "$src/src/crosspane_audio.c" -o "$exe" -framework CoreFoundation
/usr/bin/sed -e "s/@CROSSPANE_AUDIO_BUNDLE_ID@/$bundle_id/g" \
    -e "s/@CROSSPANE_AUDIO_FACTORY_UUID@/$factory_uuid/g" \
    "$src/Info.plist.in" > "$bundle/Contents/Info.plist"
/usr/bin/plutil -lint "$bundle/Contents/Info.plist"

# ---- Static checks: plist, load commands, exports, imports ----------------------------------------
echo '== checking the bundle'
plist=$bundle/Contents/Info.plist
for forbidden in AudioServerPlugIn_MachServices AudioServerPlugIn_Network AudioServerPlugIn_IOKitUserClients \
    MachServices LaunchAgent LaunchDaemon XPCService; do
    if /usr/bin/grep -q "$forbidden" "$plist"; then fail "Info.plist must not declare $forbidden (the driver has no IPC)"; fi
done
[[ $(/usr/bin/plutil -extract CFBundleIdentifier raw "$plist") == "$bundle_id" ]] || fail 'bundle id mismatch'
[[ $(/usr/bin/plutil -extract "CFPlugInFactories.$factory_uuid" raw "$plist") == CrosspaneAudioFactory ]] || fail 'factory mapping'
[[ $(/usr/bin/plutil -extract 'CFPlugInTypes.443ABAB8-E7B3-491A-B985-BEB9187030DB.0' raw "$plist") == "$factory_uuid" ]] || fail 'type mapping'
[[ $(/usr/bin/plutil -extract CFPlugInUnloadFunction raw "$plist") == CrosspaneAudioUnload ]] || fail 'unload function'
[[ $(/usr/bin/plutil -extract CFBundleExecutable raw "$plist") == CrosspaneAudio ]] || fail 'executable name'

# The bundle holds exactly an Info.plist and the executable (the signature adds its own directory).
files=$(cd "$bundle" && /usr/bin/find . -type f | /usr/bin/sort | /usr/bin/tr '\n' ' ')
[[ $files == './Contents/Info.plist ./Contents/MacOS/CrosspaneAudio ' ]] || fail "unexpected bundle contents: $files"
[[ -z $(/usr/bin/find "$bundle" -type l) ]] || fail 'the bundle contains a symbolic link'

# Linked libraries: libSystem and CoreFoundation only. No Homebrew, no CoreAudio client framework.
libs=$(/usr/bin/otool -L "$exe" | /usr/bin/tail -n +2 | /usr/bin/awk '{print $1}')
while read -r lib; do
    case $lib in
        /usr/lib/libSystem.B.dylib|/System/Library/Frameworks/CoreFoundation.framework/Versions/A/CoreFoundation) ;;
        *) fail "driver links an unexpected library: $lib" ;;
    esac
done <<< "$libs"
# Exactly those two load commands, and no run-path that could point at Homebrew. (The load-command
# allow-list above is what governs libraries; a blanket ".dylib" string match would reject the
# permitted libSystem.B.dylib itself.)
[[ $(/usr/bin/wc -l <<< "$libs" | /usr/bin/tr -d ' ') == 2 ]] || fail "driver must link exactly libSystem and CoreFoundation: $libs"
load_commands=$(/usr/bin/otool -l "$exe")
[[ $(/usr/bin/grep -c 'cmd LC_RPATH' <<< "$load_commands" || true) == 0 ]] || fail 'driver has an LC_RPATH load command'
# Embedded absolute paths into Homebrew, from fully consumed `strings` output (no early-exit pipe).
strings_out=$(/usr/bin/strings -a "$exe")
if hits=$(/usr/bin/grep -E '/opt/homebrew|/usr/local' <<< "$strings_out"); then
    fail "driver binary mentions a Homebrew path: $hits"
fi

# Exported symbols: exactly the factory and the CFPlugIn unload hook.
exports=$(/usr/bin/nm -gU "$exe" | /usr/bin/awk '{print $3}' | /usr/bin/grep -v '^__mh_bundle_header$' | /usr/bin/sort)
[[ $exports == $'_CrosspaneAudioFactory\n_CrosspaneAudioUnload' ]] || fail "unexpected exports: $exports"

# Imports: an allow-list. No Mach IPC, no XPC, no sockets, no HAL client API, no threads.
deny='mach_msg|mach_port|mach_vm|bootstrap_|xpc_|^_task_|^_host_|socket|connect|sendto|recvfrom|getaddrinfo|CFMessagePort|CFSocket|CFStream|CFNotificationCenter|CFRunLoop|CFHost|CFNetService|^_Audio(Object|Device|Hardware|Unit|Component|Queue|Services|Converter)|pthread_create|^_dispatch_|^_fork|^_exec|^_system$|^_dlsym|^_dlclose|^_posix_spawn'
allow='^(_CF[A-Za-z0-9]*|___CFConstantStringClassReference|_kCF[A-Za-z0-9]*|_mach_absolute_time|_mach_timebase_info|_pthread_mutex_(lock|unlock)|_calloc|_memcpy|_memmove|_memset|_memcmp|_bzero|_clock_gettime|_nanosleep|_dladdr|_dlopen|___stack_chk_(fail|guard)|___udivti3|___umodti3|__os_log_default|__os_log_error_impl|__os_log_impl|_os_log_type_enabled)$'
imports=$(/usr/bin/nm -u "$exe" | /usr/bin/awk '{print $1}' | /usr/bin/sort)
[[ -n $imports ]] || fail 'the driver has no imports at all: nm failed'
if bad=$(/usr/bin/grep -E "$deny" <<< "$imports"); then fail "driver imports forbidden symbols: $bad"; fi
if bad=$(/usr/bin/grep -Ev "$allow" <<< "$imports"); then fail "driver imports symbols outside the allow-list: $bad"; fi
# The production image carries no test hooks. The whole symbol table is captured first and searched
# as a here-string: a `nm | grep -q` pipeline could lose the match to SIGPIPE under pipefail.
all_symbols=$(/usr/bin/nm "$exe")
[[ -n $all_symbols ]] || fail 'nm produced no symbols'
if hooks=$(/usr/bin/grep -E 'g_test_hook|g_test_clock|g_test_live_rings|g_test_pin_fail|cp_test_reset|CROSSPANE_AUDIO_TESTING' <<< "$all_symbols"); then
    fail "test hooks leaked into the production driver: $hooks"
fi
echo "   links: $(echo "$libs" | /usr/bin/sed 's|.*/||' | /usr/bin/tr '\n' ' ')"
echo "   exports: $(echo "$exports" | /usr/bin/tr '\n' ' ')"
echo "   imports ($(echo "$imports" | /usr/bin/wc -l | /usr/bin/tr -d ' ')): $(echo "$imports" | /usr/bin/tr '\n' ' ')"

# ---- The owner probe -------------------------------------------------------------------------------
probe=$out/audio-loopback-probe
echo "== building $probe"
/usr/bin/plutil -lint "$src/probe/Info.plist"
"${cc[@]}" "${common[@]}" -O2 "$src/probe/audio_loopback_probe.c" -o "$probe" \
    -framework CoreAudio -framework CoreFoundation \
    -Wl,-sectcreate,__TEXT,__info_plist,"$src/probe/Info.plist"

# ---- Direct tests ----------------------------------------------------------------------------------
[[ $mode != build ]] || { echo "build ok: $bundle"; exit 0; }

case $mode in
    test) tdir=$out/tests; flags=(-O1); runenv=() ;;
    asan) tdir=$out/asan; flags=(-O1 -fsanitize=address,undefined -fno-sanitize-recover=all)
          runenv=(ASAN_OPTIONS=detect_stack_use_after_return=1:halt_on_error=1 UBSAN_OPTIONS=print_stacktrace=1:halt_on_error=1) ;;
    tsan) tdir=$out/tsan; flags=(-O1 -fsanitize=thread)
          runenv=(TSAN_OPTIONS=halt_on_error=1:second_deadlock_stack=1) ;;
esac
/bin/rm -rf "$tdir"
/bin/mkdir -p "$tdir"

# Tests drive the real vtable with deliberately invalid (NULL) arguments, which the SDK's
# nullability attributes would otherwise reject at compile time.
tests=(test_topology test_clock test_io test_history test_lifecycle test_bundle)
for t in "${tests[@]}"; do
    echo "== building $mode/$t"
    "${cc[@]}" "${common[@]}" "${flags[@]}" -Wno-nonnull -DCROSSPANE_AUDIO_TESTING=1 \
        "$src/tests/$t.c" -o "$tdir/$t" -framework CoreFoundation
done

run_test() { # run_test seconds cmd...
    local seconds=$1
    shift
    /usr/bin/env ${runenv[@]+"${runenv[@]}"} /usr/bin/perl -e 'alarm shift; exec @ARGV or die "exec: $!"' "$seconds" "$@"
}

echo "== running $mode tests (no HAL registration, no audio I/O, no installed driver)"
for t in "${tests[@]}"; do
    args=()
    [[ $t == test_bundle ]] && args=("$bundle")
    echo "-- $t"
    run_test 600 "$tdir/$t" ${args[@]+"${args[@]}"} || fail "$mode/$t failed"
done
echo "all direct tests passed ($mode build)"

# Leak check with the system tool (LeakSanitizer is not available on arm64 macOS): every test
# process must exit with zero leaked blocks.
if [[ $mode == test && -x /usr/bin/leaks ]]; then
    echo '== leak check (/usr/bin/leaks --atExit)'
    for t in "${tests[@]}"; do
        args=()
        [[ $t == test_bundle ]] && args=("$bundle")
        report=$(run_test 900 /usr/bin/leaks --atExit -- "$tdir/$t" ${args[@]+"${args[@]}"} 2>&1) || true
        if [[ $(/usr/bin/grep -c ' 0 leaks for 0 total leaked bytes' <<< "$report" || true) == 0 ]]; then
            echo "$report" | /usr/bin/tail -30 >&2
            fail "$t leaked memory"
        fi
        echo "   $t: $(/usr/bin/grep -E ' leaks for ' <<< "$report" | /usr/bin/sed 's/^Process [0-9]*: //')"
    done
fi
