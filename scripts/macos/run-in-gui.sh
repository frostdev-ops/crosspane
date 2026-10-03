#!/bin/bash
set -euo pipefail

usage() {
    echo 'Usage: run-in-gui.sh [--timeout SECS] -- COMMAND [ARGS…]' >&2
    exit 2
}

timeout=120
if [[ ${1:-} == --timeout ]]; then
    [[ $# -ge 2 ]] || usage
    timeout=$2
    shift 2
fi
[[ $timeout =~ ^[1-9][0-9]*$ ]] || usage
[[ ${1:-} == -- && $# -ge 2 ]] || usage
shift

tmp=$(/usr/bin/mktemp -d /tmp/crosspane-run.XXXXXXXX)
label=io.frostdev.crosspane.run.${tmp##*.}
cleanup() {
    local result=$?
    set +e
    trap - EXIT
    if ! /bin/launchctl remove "$label" 2>/dev/null; then
        if /bin/launchctl list "$label" >/dev/null 2>&1; then
            echo "Could not remove launchd job: $label" >&2
            result=1
        fi
    fi
    [[ ! -f $tmp/stdout ]] || /bin/cat "$tmp/stdout"
    [[ ! -f $tmp/stderr ]] || /bin/cat "$tmp/stderr" >&2
    /bin/rm -rf "$tmp"
    exit "$result"
}
trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

/bin/cat > "$tmp/wrapper.sh" <<'WRAPPER'
#!/bin/sh
tmp=$1
workdir=$2
PATH=$3
export PATH
shift 3
# launchd restarts submitted jobs even after a clean exit: run the command once, then idle until
# the caller removes the job.
if [ -e "$tmp/started" ]; then
    exec /bin/sleep 3600
fi
: > "$tmp/started"
if cd "$workdir"; then
    "$@"
    result=$?
else
    result=$?
fi
printf '%s\n' "$result" > "$tmp/status.tmp"
/bin/mv "$tmp/status.tmp" "$tmp/status"
# The caller returns the recorded command status and removes the job.
exit 0
WRAPPER

/bin/launchctl submit -l "$label" -o "$tmp/stdout" -e "$tmp/stderr" -- \
    /bin/sh "$tmp/wrapper.sh" "$tmp" "$PWD" "$PATH" "$@"
deadline=$((SECONDS + timeout))
while [[ ! -f $tmp/status ]]; do
    if (( SECONDS >= deadline )); then
        echo "GUI command timed out after ${timeout}s" >&2
        exit 124
    fi
    /bin/sleep 0.1
done
read -r result < "$tmp/status"
exit "$result"
