#!/usr/bin/env bash
# Tests create only new scratch trees and never execute any artifact.
set -euo pipefail
exec python3 - "$(dirname "$0")/stage-linux.sh" <<'PY'
import hashlib, io, json, os, pathlib, shutil, subprocess, sys, tarfile, tempfile

stage = pathlib.Path(sys.argv[1]).resolve()
FILES = ["bin/crosspane-agent", "bin/crosspanectl", "bin/crosspane-ui",
         "bin/crosspane-installer",
         "resources/crosspane-agent.service", "resources/crosspane-settings.desktop",
         "resources/crosspane-installer.desktop", "resources/crosspane-icon.svg", "resources/LICENSE"]
TEMPLATES = {FILES[4]: b"[Service]\nExecStart={{agent_executable}} run\nEnvironment={{xdg_config_environment}}\nEnvironment={{xdg_state_environment}}\nEnvironment={{xdg_runtime_environment}}\nEnvironment={{crosspane_runtime_environment}}\n",
             FILES[5]: b"[Desktop Entry]\nExec={{settings_executable}}\n",
             FILES[6]: b"[Desktop Entry]\nExec={{installer_executable}}\n"}
DESKTOP = ["resources/io.frostdev.crosspane.agent.desktop",
           "resources/gnome-shell-extension/extension.js",
           "resources/gnome-shell-extension/metadata.json",
           "resources/gnome-shell-extension/io.frostdev.Crosspane.Shell1.xml"]
DESKTOP_DATA = {DESKTOP[0]: b"[Desktop Entry]\nType=Application\nName=Crosspane\nExec=crosspane-agent\n",
                DESKTOP[1]: b"// inert fixture extension \xc3\xa9\n",
                DESKTOP[2]: b'{"uuid": "crosspane@frostdev.io", "shell-version": ["48"]}\n',
                DESKTOP[3]: b'<node><interface name="io.frostdev.Crosspane.Shell1"/></node>\n'}
def sha(data):
    return hashlib.sha256(data).hexdigest()
def elf(machine=62):
    data = bytearray(64)
    data[:7] = b"\x7fELF\x02\x01\x01"
    data[16:18] = (3).to_bytes(2, "little")
    data[18:20] = machine.to_bytes(2, "little")
    data[20] = 1
    data[52] = 64
    return bytes(data)
def write(path, data):
    path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    path.write_bytes(data)
    path.chmod(0o600)
def fixture(root, machine=62, schema=1):
    root.mkdir(mode=0o700)
    metadata = {"schema_version": schema, "product_version": "0.0.0", "architecture": "x86_64" if machine == 62 else "aarch64",
                "source_revision": "1" * 40, "profile": "dev", "libraries": [], "members": []}
    for index, name in enumerate(FILES):
        data = elf(machine) if index < 4 else TEMPLATES.get(name, b"inert-resource\n")
        write(root / name, data)
        metadata["members"].append({"name": name, "size": len(data), "sha256": sha(data),
                                    "features": ["video"] if index == 0 else []})
    for name in DESKTOP if schema == 2 else []:
        write(root / name, DESKTOP_DATA[name])
        metadata["members"].append({"name": name, "size": len(DESKTOP_DATA[name]),
                                    "sha256": sha(DESKTOP_DATA[name]), "features": []})
    write(root / "libraries/libavcodec.so.61", elf(machine))
    metadata["libraries"] = [{"name": "libavcodec.so.61", "sha256": sha(elf(machine))}]
    write(root / "provenance.json", json.dumps(metadata).encode())
    return metadata
def run(source, output, succeeds, existing=False):
    result = subprocess.run(["bash", str(stage), str(source), str(output)],
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=10)
    assert (result.returncode == 0) == succeeds, result.stderr.decode()
    if not succeeds and not existing:
        assert not output.exists()
    return result
checks = 0
with tempfile.TemporaryDirectory(prefix="cp47b-stage-", dir="/tmp") as directory:
    scratch = pathlib.Path(directory)
    source, output = scratch / "good", scratch / "output"
    metadata = fixture(source)
    run(source, output, True)
    archive = (output / "payload.tar").read_bytes()
    assert (output / "payload.sha256").read_text() == sha(archive) + "\n"
    assert output.stat().st_mode & 0o777 == 0o700
    assert (output / "payload.tar").stat().st_mode & 0o777 == 0o600
    with tarfile.open(fileobj=io.BytesIO(archive), mode="r:") as bundle:
        assert bundle.getnames() == ["manifest.json", *FILES]
        embedded = json.load(bundle.extractfile("manifest.json"))
        assert embedded == metadata
        for member in bundle.getmembers():
            assert member.isreg() and member.uid == member.gid == member.mtime == 0
            assert member.mode == (0o755 if member.name.startswith("bin/") else 0o644)
            if member.name != "manifest.json":
                data = bundle.extractfile(member).read()
                record = next(row for row in embedded["members"] if row["name"] == member.name)
                assert record["size"] == len(data) and record["sha256"] == sha(data)
    run(source, scratch / "repeat", True)
    assert archive == (scratch / "repeat/payload.tar").read_bytes()
    checks += 2
    for case in range(33):
        broken, out = scratch / f"bad-{case}", scratch / f"out-{case}"
        m = fixture(broken)
        if case == 0:
            (broken / FILES[4]).unlink()
        elif case == 1:
            write(broken / "extra", b"extra")
        elif case == 2:
            m["members"][0]["features"] = []
        elif case == 3:
            m["members"][0]["sha256"] = "0" * 64
        elif case == 4:
            m["architecture"] = "aarch64"
        elif case == 5:
            write(broken / "libraries/libavcodec.so.61", b"bad library")
        elif case == 6:
            m["libraries"][0]["sha256"] = "0" * 64
        elif case == 7:
            m["members"].append(m["members"][0])
        elif case == 8:
            target = broken / FILES[0]
            target.unlink()
            target.symlink_to(source / FILES[0])
        elif case == 9:
            shutil.rmtree(broken / "bin")
            (broken / "bin").symlink_to(source / "bin", target_is_directory=True)
        elif case == 10:
            (broken / FILES[0]).chmod(0o4755)
        elif case == 11:
            write(broken / "provenance.json", b'{"schema_version":1,"schema_version":1}')
        else:
            content = TEMPLATES[FILES[4]]
            if case == 12:
                content = content.replace(b"{{agent_executable}}", b"{{unknown}}")
            elif case == 13:
                content += b"{{agent_executable}}"
            elif case == 14:
                content = content.replace(b"{{xdg_config_environment}}", b"")
            elif case == 15:
                content += b"\0"
            elif case == 16:
                content = content.replace(b"{{agent_executable}}", b"{{agent_executable}}suffix")
            elif case == 17:
                content = content.replace(b"{{agent_executable}}", b'"{{agent_executable}}"')
            elif case == 18:
                content = content.replace(b"ExecStart={{agent_executable}} run", b"#{{agent_executable}}\nExecStart=/bin/false")
            elif case == 19:
                content += b"ExecStart=/bin/false\n"
            elif case == 20:
                content += b"Description=raw %h\n"
            elif case == 21:
                content = content.replace(b"[Service]", b"[Wrong]")
            elif case < 25:
                content = TEMPLATES[FILES[5]]
                content = (content + b"Exec=/bin/false\n" if case == 22 else
                           content.replace(b"{{settings_executable}}", b'"{{settings_executable}}"') if case == 23 else
                           content + b"Name=raw %f\n")
                write(broken / FILES[5], content)
                record = m["members"][6]
                record["size"], record["sha256"] = len(content), sha(content)
                content = TEMPLATES[FILES[4]]
            else:
                # Native-parser normalization cases; our ASCII physical grammar refuses them.
                index = 5 if case < 29 else 6
                content = TEMPLATES[FILES[index]]
                variant = (case - 25) % 4
                command = b"ExecStart" if index == 5 else b"Exec"
                if variant == 0:
                    content = content.replace(command + b"=", b"Type=exec\\\n" + command + b"=")
                elif variant == 1:
                    content += b"\xef\xbb\xbf" + command + b"=/bin/false\n"
                elif variant == 2:
                    content += b"# non-ASCII \xc3\xa9\n"
                else:
                    content = content.replace(b"\n", b"\r\n")
                if index != 5:
                    write(broken / FILES[index], content)
                    record = m["members"][index]
                    record["size"], record["sha256"] = len(content), sha(content)
                    content = TEMPLATES[FILES[4]]
            write(broken / FILES[4], content)
            record = next(row for row in m["members"] if row["name"] == FILES[4])
            record["size"], record["sha256"] = len(content), sha(content)
        if case != 11:
            write(broken / "provenance.json", json.dumps(m).encode())
        run(broken, out, False)
        checks += 1
    alias = scratch / "source-alias"
    alias.symlink_to(source, target_is_directory=True)
    run(alias, scratch / "alias-output", False)
    checks += 1
    original = {name: (source / name).read_bytes() for name in FILES}
    run(source, output, False, existing=True)
    assert (output / "payload.tar").read_bytes() == archive
    alias_output = scratch / "alias-output-existing"
    alias_output.symlink_to(output, target_is_directory=True)
    run(source, alias_output, False, existing=True)
    assert (output / "payload.tar").read_bytes() == archive
    checks += 2
    weak = scratch / "weak-parent"
    weak.mkdir(mode=0o700)
    weak.chmod(0o770)
    run(source, weak / "output", False)
    weak.chmod(0o700)
    checks += 1
    arm = scratch / "arm"
    fixture(arm, 183)
    run(arm, scratch / "arm-output", True)
    with tarfile.open(scratch / "arm-output/payload.tar", mode="r:") as bundle:
        assert json.load(bundle.extractfile("manifest.json"))["architecture"] == "aarch64"
    checks += 1
    oversize = scratch / "oversize"
    oversized_metadata = fixture(oversize)
    with (oversize / FILES[0]).open("r+b") as stream:
        stream.truncate(64 * 1024 * 1024 + 1)
    oversized_bytes = (oversize / FILES[0]).read_bytes()
    oversized_metadata["members"][0]["size"] = len(oversized_bytes)
    oversized_metadata["members"][0]["sha256"] = sha(oversized_bytes)
    del oversized_bytes
    write(oversize / "provenance.json", json.dumps(oversized_metadata).encode())
    run(oversize, scratch / "oversize-output", False)
    checks += 1
    assert all((source / name).read_bytes() == content for name, content in original.items())
    checks += 1
    # Schema 2: the nine core members and the four GNOME/KDE desktop members, in that order.
    desktop_source, desktop_output = scratch / "desktop", scratch / "desktop-output"
    desktop_metadata = fixture(desktop_source, schema=2)
    run(desktop_source, desktop_output, True)
    with tarfile.open(desktop_output / "payload.tar", mode="r:") as bundle:
        assert bundle.getnames() == ["manifest.json", *FILES, *DESKTOP]
        assert json.load(bundle.extractfile("manifest.json")) == desktop_metadata
        for name in DESKTOP:
            assert bundle.extractfile(name).read() == DESKTOP_DATA[name]
            assert bundle.getmember(name).mode == 0o644
    checks += 1
    for case in range(6):
        broken, out = scratch / f"desktop-bad-{case}", scratch / f"desktop-out-{case}"
        m = fixture(broken, schema=2)
        if case == 0:
            # A schema-2 payload never half-carries the desktop files.
            (broken / DESKTOP[1]).unlink()
        elif case == 1:
            m["members"].pop()
        elif case == 2:
            content = DESKTOP_DATA[DESKTOP[2]].replace(b"crosspane@frostdev.io", b"other@example.org")
            write(broken / DESKTOP[2], content)
            m["members"][11]["size"], m["members"][11]["sha256"] = len(content), sha(content)
        elif case == 3:
            content = DESKTOP_DATA[DESKTOP[0]] + b"Exec={{agent_executable}}\n"
            write(broken / DESKTOP[0], content)
            m["members"][9]["size"], m["members"][9]["sha256"] = len(content), sha(content)
        elif case == 4:
            content = DESKTOP_DATA[DESKTOP[1]] + b"\0"
            write(broken / DESKTOP[1], content)
            m["members"][10]["size"], m["members"][10]["sha256"] = len(content), sha(content)
        else:
            # A schema-1 manifest may not list the desktop members.
            m["schema_version"] = 1
        write(broken / "provenance.json", json.dumps(m).encode())
        run(broken, out, False)
        checks += 1
print(f"stage-linux: {checks} scratch checks passed; artifacts stayed inert")
PY
