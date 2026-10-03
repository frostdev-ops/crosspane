#!/usr/bin/env bash
# Detached arm64 SDK/Rust ABI checks. No SDK function or audio device is invoked.
# Each step has a wall deadline and at most 256 KiB of combined diagnostics.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/../.."
[[ "$(uname -s)" == Darwin && "$(uname -m)" == arm64 ]] || {
    echo 'CoreAudio ABI check requires arm64 macOS' >&2
    exit 1
}

bounded() {
    python3 -c '
import os, selectors, signal, subprocess, sys, time
label, seconds, *command = sys.argv[1:]
limit = 256 * 1024
output = bytearray()
deadline = time.monotonic() + float(seconds)
child = subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                         start_new_session=True)
with selectors.DefaultSelector() as ready:
    os.set_blocking(child.stdout.fileno(), False)
    ready.register(child.stdout, selectors.EVENT_READ)
    failure = None
    while ready.get_map() or child.poll() is None:
        remaining = deadline - time.monotonic()
        if remaining <= 0 or len(output) > limit:
            failure = label + " deadline/output bound exceeded"
            break
        for key, _ in ready.select(min(0.1, remaining)):
            try:
                data = os.read(key.fd, min(8192, limit + 1 - len(output)))
            except BlockingIOError:
                continue
            if data:
                output.extend(data)
            else:
                ready.unregister(key.fileobj)
    if failure:
        # This isolated process group contains only the command and its descendants.
        try:
            os.killpg(child.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        try:
            child.wait(timeout=3)
        except subprocess.TimeoutExpired:
            failure += "; child cleanup unconfirmed"
        print(failure, file=sys.stderr)
        code = 1
    else:
        code = child.wait()
child.stdout.close()
sys.stdout.buffer.write(output[:limit])
if label == "Rust" and code == 0:
    if b"test result: ok. 1 passed; 0 failed;" not in output:
        print("Rust ABI test did not execute exactly one passing test", file=sys.stderr)
        code = 1
sys.exit(code if code >= 0 else 1)
' "$@"
}

bounded SDK 30 xcrun --sdk macosx clang -arch arm64 -std=gnu17 \
    -Wall -Wextra -Werror -fsyntax-only scripts/installer/check-coreaudio-abi.c
bounded Rust 180 cargo test --offline --locked -p crosspane-installer --lib \
    platform::macos::tutorial::ffi::tests::public_sdk_abi -- --exact
echo 'CoreAudio SDK/Rust ABI checks passed.'
