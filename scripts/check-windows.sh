#!/usr/bin/env bash
# Lane W proves buildability only; runtime validation needs a real Windows machine.
set -euo pipefail

repo_root=$(cd -- "${BASH_SOURCE[0]%/*}/.." && pwd -P)
cd -- "$repo_root"
target=x86_64-pc-windows-msvc
installed=false
while IFS= read -r installed_target; do
    if [[ "$installed_target" == "$target" ]]; then
        installed=true
    fi
done <<< "$(rustup target list --installed)"
if [[ "$installed" == false ]]; then
    printf 'Windows cross-check requires: rustup target add %s\n' "$target" >&2
    exit 1
fi

export CARGO_TARGET_DIR="$repo_root/target/wincheck"
packages=(
    -p crosspane-types
    -p crosspane-protocol
    -p crosspane-input
    -p crosspane-platform
    -p crosspane-platform-windows
)
cargo check --locked --target "$target" "${packages[@]}"
cargo clippy --locked --target "$target" "${packages[@]}" -- -D warnings
