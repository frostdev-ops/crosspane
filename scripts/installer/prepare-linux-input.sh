#!/usr/bin/env bash
# Lead tooling: build the developer-approved input directory that stage-linux.sh packages
# (docs/setup/installer-tier1.md §Linux). Builds release binaries from a clean checkout, strips
# their debug info (the stager's per-file limit is 64 MiB), copies the approved templates, records
# the runtime libraries the binaries link against, and writes provenance.json.
#
#   scripts/installer/prepare-linux-input.sh <fresh-output-dir>
#
# Builds only: nothing is installed, started or run. The output directory must not exist.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/../.."

[ $# -eq 1 ] || { echo 'usage: prepare-linux-input.sh <fresh-output-dir>' >&2; exit 2; }
out=$1
case $out in /*) ;; *) echo "output must be an absolute path: $out" >&2; exit 2 ;; esac
[ -e "$out" ] && { echo "output already exists: $out" >&2; exit 1; }
[ "$(uname -s)" = Linux ] || { echo 'Linux only' >&2; exit 1; }
case $(uname -m) in
    x86_64) arch=x86_64 ;;
    aarch64) arch=aarch64 ;;
    *) echo "unsupported architecture: $(uname -m)" >&2; exit 1 ;;
esac
# The provenance names one source revision, so the tracked tree must match it exactly.
if ! git diff --quiet HEAD -- || [ -n "$(git ls-files --others --exclude-standard -- apps crates packaging assets)" ]; then
    echo 'refusing: the checkout has uncommitted changes; build from a clean tree' >&2
    exit 1
fi
rev=$(git rev-parse HEAD)
version=$(cargo metadata --no-deps --format-version 1 --locked |
    python3 -I -c 'import json,sys; print(next(p["version"] for p in json.load(sys.stdin)["packages"] if p["name"] == "crosspane-agent"))')

cargo build --release --locked -p crosspane-agent --features crosspane-agent/video \
    -p crosspanectl -p crosspane-ui -p crosspane-installer
target=$(cargo metadata --no-deps --format-version 1 --locked |
    python3 -I -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')

tmp=$(mktemp -d "$(dirname "$out")/.prepare-linux-input.XXXXXX")
trap 'rm -rf "$tmp"' EXIT
mkdir -m 700 "$tmp/bin" "$tmp/resources" "$tmp/libraries"
for b in crosspane-agent crosspanectl crosspane-ui crosspane-installer; do
    install -m 0755 "$target/release/$b" "$tmp/bin/$b"
    strip --strip-debug "$tmp/bin/$b"
done
install -m 0644 packaging/linux/crosspane-agent.service packaging/linux/crosspane-settings.desktop \
    packaging/linux/crosspane-installer.desktop assets/brand/crosspane-icon.svg "$tmp/resources/"
install -m 0644 LICENSE "$tmp/resources/LICENSE"

# Runtime libraries the binaries link against, excluding the C runtime itself. Each is resolved
# through the loader cache for this architecture and copied as a regular file.
abi=$([ "$arch" = x86_64 ] && echo x86-64 || echo AArch64)
for b in "$tmp"/bin/*; do
    readelf -d "$b" | sed -n 's/.*(NEEDED).*\[\(.*\)\]/\1/p'
done | sort -u | grep -v -E '^(libc|libm|libdl|libpthread|librt|libutil|ld-linux[^ ]*)\.so' |
while read -r lib; do
    # awk reads to the end (no early exit), so ldconfig never dies of SIGPIPE under pipefail.
    path=$(ldconfig -p | awk -v l="$lib" -v a="$abi" '!found && $1 == l && index($0, a) {print $NF; found = 1}')
    [ -n "$path" ] || { echo "runtime library not found: $lib" >&2; exit 1; }
    install -m 0644 "$(readlink -f "$path")" "$tmp/libraries/$lib"
done

python3 -I - "$tmp" "$rev" "$version" "$arch" <<'PY'
import hashlib, json, pathlib, sys
src, rev, version, arch = pathlib.Path(sys.argv[1]), *sys.argv[2:]
files = ["bin/crosspane-agent", "bin/crosspanectl", "bin/crosspane-ui", "bin/crosspane-installer",
         "resources/crosspane-agent.service",
         "resources/crosspane-settings.desktop", "resources/crosspane-installer.desktop",
         "resources/crosspane-icon.svg", "resources/LICENSE"]
def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()
members = [{"name": f, "size": (src / f).stat().st_size, "sha256": sha(src / f),
            "features": ["video"] if f == "bin/crosspane-agent" else []} for f in files]
libraries = [{"name": p.name, "sha256": sha(p)} for p in sorted((src / "libraries").iterdir())]
if not libraries:
    sys.exit("no runtime libraries recorded")
provenance = {"schema_version": 1, "product_version": version, "architecture": arch,
              "source_revision": rev, "profile": "release", "libraries": libraries,
              "members": members}
(src / "provenance.json").write_text(json.dumps(provenance, indent=1) + "\n")
PY
chmod -R go-w "$tmp"
chmod 0700 "$tmp"
mv -T "$tmp" "$out"
trap - EXIT
echo "prepared $out (revision ${rev:0:12}, $(ls "$out/libraries" | wc -l) runtime libraries)"
