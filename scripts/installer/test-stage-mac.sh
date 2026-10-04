#!/bin/bash
# Builds only into a private worktree-local scratch directory; never opens an app or package.
set -euo pipefail
[[ $(uname -s) == Darwin ]] || { echo 'test-stage-mac: macOS only' >&2; exit 1; }
root=$(cd -- "${BASH_SOURCE[0]%/*}/../.." && pwd -P)
cd "$root"
mkdir -p target
scratch=$(mktemp -d "$root/target/wp426-test.XXXXXXXX")
chmod 700 "$scratch"
trap 'rm -rf -- "$scratch"' EXIT
bash scripts/installer/stage-mac.sh "$scratch/output"
# -I ignores PYTHON* (PYTHONOPTIMIZE would strip every assert and pass vacuously).
python3 -I - "$scratch/output" "$root/target/release/crosspane-mac-inventory" <<'PY'
import hashlib, json, os, pathlib, shutil, stat, subprocess, sys
assert sys.flags.optimize == 0
out, tool = map(pathlib.Path, sys.argv[1:])
app = out / "Crosspane Installer.app"
payload = app / "Contents/Resources/payload"
main = app / "Contents/MacOS/Crosspane Installer"
sample = (out / "ApprovedInventory.json").read_bytes().rstrip(b"\n")
inventory = json.loads(sample)
regen = subprocess.check_output([tool, payload, "--product-version", inventory["product_version"],
                                "--features", ",".join(inventory["features"])], timeout=120).rstrip(b"\n")
assert sample == regen, "inventory must equal the signed payload"
marker = b'{"product_version":'
assert sample.startswith(marker)
outer = main.read_bytes()
assert outer.count(sample) == 1 and outer.count(marker) == 1, "outer binary must embed exactly this inventory"
bootstrap = (payload / "crosspane-installer").read_bytes()
assert marker not in bootstrap, "bootstrap must embed no inventory (non-mutating)"
actual = sorted(str(p.relative_to(payload)) for p in payload.rglob("*") if p.is_file())
assert actual == [f["path"] for f in inventory["files"]]
for f in inventory["files"]:
    p = payload / f["path"]
    assert not p.is_symlink()
    assert stat.S_IMODE(p.stat().st_mode) == f["mode"]
    data = p.read_bytes()
    assert len(data) == f["size"] and list(hashlib.sha256(data).digest()) == f["sha256"]
    if f["signing"]:
        # The recorded designated requirement compiles and the code satisfies it.
        subprocess.run(["/usr/bin/codesign", "--verify", "--strict", "-R=" + f["signing"]["designated_requirement"], p],
                       check=True, timeout=30)
audio = payload / "Crosspane.app/Contents/Resources/audio"
packages = json.loads((audio / "packages.json").read_text())
assert [p["kind"] for p in packages["packages"]] == ["install", "remove"]
for p in packages["packages"]:
    assert hashlib.sha256((audio / p["file"]).read_bytes()).hexdigest() == p["sha256"]
assert any(f["path"].startswith("Crosspane.app/Contents/Frameworks/libopus") for f in inventory["files"])
subprocess.run(["/usr/bin/codesign", "--verify", "--deep", "--strict", app], check=True, timeout=120)
listed = {}
for line in (out / "SHA256SUMS").read_text().splitlines():
    digest, name = line.split("  ", 1)
    assert name not in listed
    listed[name] = digest
assert sorted(listed) == sorted(str(p.relative_to(out)) for p in out.rglob("*") if p.is_file() and p.name != "SHA256SUMS")
for name, digest in listed.items():
    assert hashlib.sha256((out / name).read_bytes()).hexdigest() == digest
for f in inventory["files"]:
    if f["signing"]:
        print("inventory sample:", f["path"], f["signing"]["role"], f["size"],
              bytes(f["sha256"]).hex(), f["signing"]["identifier"])
# Corrupt only an independent scratch copy; the signed result remains intact.
bad = out.parent / "invalid-payload"
shutil.copytree(payload, bad)
def refuses(label, *extra, expect=b""):
    result = subprocess.run([tool, bad, "--product-version", inventory["product_version"],
                             "--features", ",".join(inventory["features"]), *extra],
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=120)
    assert result.returncode != 0 and not result.stdout and expect in result.stderr, label
    print("stage-mac: refused", label)
ctl = bad / "crosspanectl"
saved = out.parent / "saved-ctl"
ctl.rename(saved)
refuses("missing mandatory CLI")
ctl.symlink_to(saved)
refuses("symlinked CLI")
ctl.unlink()
shutil.copyfile(saved, ctl)
os.chmod(ctl, 0o644)
refuses("wrong executable mode")
ctl.write_bytes(b"unsigned executable")
os.chmod(ctl, 0o755)
refuses("unsigned executable")
ctl.unlink()
saved.rename(ctl)
link = bad / "Crosspane.app/Contents/Resources/hardlink"
os.link(ctl, link)
refuses("hard-linked file", expect=b"invalid or excessive payload file")
link.unlink()
stray = bad / "Crosspane.app/Contents/MacOS/crosspanectl"
shutil.copy2(ctl, stray)
refuses("unapproved executable")
stray.unlink()
refuses("foreign Team", "--team-id", "ZZZZZZZZZZ", expect=b"different Team")
empty = bad / "unexpected-empty-directory"
empty.mkdir()
refuses("unexpected empty directory")
empty.rmdir()
assert subprocess.check_output([tool, bad, "--product-version", inventory["product_version"],
                                "--features", ",".join(inventory["features"])], timeout=120).rstrip(b"\n") == sample
print("stage-mac: signed bundle, embedded inventory, exact payload and checksums verified")
PY
# Refusing an existing output must preserve it and must happen before another build.
before=$(/usr/bin/shasum -a 256 "$scratch/output/SHA256SUMS" "$scratch/output/Crosspane Installer.app/Contents/MacOS/Crosspane Installer")
if bash scripts/installer/stage-mac.sh "$scratch/output" 2> "$scratch/refusal.txt"; then
    echo 'test-stage-mac: existing output was accepted' >&2; exit 1
fi
grep -q 'output must be a fresh' "$scratch/refusal.txt" || { cat "$scratch/refusal.txt" >&2; exit 1; }
after=$(/usr/bin/shasum -a 256 "$scratch/output/SHA256SUMS" "$scratch/output/Crosspane Installer.app/Contents/MacOS/Crosspane Installer")
[[ $before == "$after" ]] || { echo 'test-stage-mac: refused run changed the output' >&2; exit 1; }
/usr/bin/codesign --verify --deep --strict "$scratch/output/Crosspane Installer.app"
echo 'test-stage-mac: passed (build and signature checks only)'
