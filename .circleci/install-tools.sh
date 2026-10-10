#!/usr/bin/env bash
set -euo pipefail

job=${1:?usage: install-tools.sh policy|linux|macos|windows}
cargo_bin="$HOME/.cargo/bin"
export PATH="$cargo_bin:$PATH"
printf 'export PATH=%q:$PATH\n' "$cargo_bin" >> "$BASH_ENV"

if ! command -v rustup >/dev/null 2>&1; then
    case "$(uname -s)" in
        MINGW*|MSYS*)
            # rustup dispatches by executable basename; the installer must be rustup-init.exe.
            mkdir -p /tmp/crosspane-rustup
            curl --fail --location --retry 3 https://win.rustup.rs/x86_64 -o /tmp/crosspane-rustup/rustup-init.exe
            /tmp/crosspane-rustup/rustup-init.exe -y --no-modify-path --profile minimal --default-toolchain none
            ;;
        *)
            curl --fail --location --retry 3 https://sh.rustup.rs -o /tmp/crosspane-rustup-init.sh
            bash /tmp/crosspane-rustup-init.sh -y --no-modify-path --profile minimal --default-toolchain none
            ;;
    esac
fi
rustup toolchain install
rustc --version

case "$job" in
    policy)
        url=https://github.com/EmbarkStudios/cargo-deny/releases/download/0.20.2/cargo-deny-0.20.2-x86_64-unknown-linux-musl.tar.gz
        checksum=9f12ed4c49936e09b48bf862b595cde2fe64fcbd9d74dfacac6131ca824c8d5f
        ;;
    linux)
        target=x86_64-unknown-linux-gnu
        checksum=223ba936714cc861a9cdedad5a8eb664dfdd69d6fbdf6cea6aa8184f523290dc
        ;;
    macos)
        target=universal-apple-darwin
        checksum=6c23b7fb4ca82c571fc8dfedbab45bfa773d45918f933170c85d21cd7f4b852b
        ;;
    windows)
        target=x86_64-pc-windows-msvc
        checksum=bef17504bed9d4994cf860c122e84a90d613b92e6d89584fa9e28bd53ad42e15
        ;;
    *)
        printf 'Unknown CI job: %s\n' "$job" >&2
        exit 1
        ;;
esac

if [[ "$job" != policy ]]; then
    url="https://github.com/nextest-rs/nextest/releases/download/cargo-nextest-0.9.148/cargo-nextest-0.9.148-$target.tar.gz"
fi

scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT
curl --fail --location --retry 3 "$url" -o "$scratch/tool.tar.gz"
if command -v sha256sum >/dev/null 2>&1; then
    actual=$(sha256sum "$scratch/tool.tar.gz")
else
    actual=$(shasum -a 256 "$scratch/tool.tar.gz")
fi
if [[ "${actual%% *}" != "$checksum" ]]; then
    printf 'CI tool archive checksum mismatch\n' >&2
    exit 1
fi
tar -xzf "$scratch/tool.tar.gz" -C "$scratch"
mkdir -p "$cargo_bin"
if [[ "$job" == policy ]]; then
    install -m 755 "$scratch/cargo-deny-0.20.2-x86_64-unknown-linux-musl/cargo-deny" "$cargo_bin/cargo-deny"
    cargo deny --version
elif [[ "$job" == windows ]]; then
    cp "$scratch/cargo-nextest.exe" "$cargo_bin/cargo-nextest.exe"
    cargo nextest --version
else
    install -m 755 "$scratch/cargo-nextest" "$cargo_bin/cargo-nextest"
    cargo nextest --version
fi
