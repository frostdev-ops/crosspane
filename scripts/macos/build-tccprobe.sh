#!/bin/bash
set -euo pipefail

script_dir=$(cd -- "${BASH_SOURCE[0]%/*}" && pwd -P)
repo_root=$(cd -- "$script_dir/../.." && pwd -P)
out=$repo_root/target/macos-bundles
/bin/mkdir -p "$out"
/usr/bin/clang -std=c11 -Wall -Wextra -Werror -mmacosx-version-min=26.0 \
    "$script_dir/tccprobe.c" -o "$out/tccprobe" \
    -framework ApplicationServices -framework CoreFoundation -framework IOKit
"$script_dir/bundle.sh" --bin "$out/tccprobe" --id io.frostdev.crosspane.tccprobe \
    --name CrosspaneTccProbe --out "$out" --ui-element
