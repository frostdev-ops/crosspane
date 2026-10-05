#!/usr/bin/env bash
# Local developer inputs only: no build, installation, discovery, or artifact execution.
# Lead build-time staging requires a fresh private output parent. Same-UID path replacement
# between checks and opens is outside this shell stager's threat model; installed mutations use fds.
set -euo pipefail
exec python3 - "$@" <<'PY'
import hashlib, json, os, pathlib, re, stat, sys

FILES = ["bin/crosspane-agent", "bin/crosspanectl", "bin/crosspane-ui",
         "bin/crosspane-installer",
         "resources/crosspane-agent.service", "resources/crosspane-settings.desktop",
         "resources/crosspane-installer.desktop", "resources/crosspane-icon.svg", "resources/LICENSE"]
# Each placeholder is a whole quoted Exec argument or unit Environment assignment, never a fragment.
FIELDS = {FILES[4]: ["{{agent_executable}}", "{{xdg_config_environment}}", "{{xdg_state_environment}}",
                     "{{xdg_runtime_environment}}", "{{crosspane_runtime_environment}}"],
          FILES[5]: ["{{settings_executable}}"], FILES[6]: ["{{installer_executable}}"]}
LIMIT, RECORD, ARCHIVE = 64 * 1024 * 1024, 65536, 256 * 1024 * 1024
def require(condition):
    if not condition:
        raise ValueError("invalid staging input")
def pairs(items):
    result = {}
    for key, value in items:
        require(key not in result)
        result[key] = value
    return result
def token(value):
    return isinstance(value, str) and re.fullmatch(r"[A-Za-z0-9._+-]{1,64}", value)
def read(path, limit):
    for ancestor in (path, *path.parents):
        require(not ancestor.is_symlink())
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    try:
        facts = os.fstat(fd)
        require(stat.S_ISREG(facts.st_mode) and facts.st_uid == os.getuid()
                and facts.st_nlink == 1 and not facts.st_mode & 0o7022 and 0 < facts.st_size <= limit)
        with os.fdopen(fd, "rb", closefd=False) as stream:
            result = stream.read(limit + 1)
        require(0 < len(result) <= limit)
        return result
    finally:
        os.close(fd)
def elf(data, machine):
    require(len(data) >= 64 and data[:7] == b"\x7fELF\x02\x01\x01"
            and int.from_bytes(data[16:18], "little") in (2, 3)
            and int.from_bytes(data[18:20], "little") == machine
            and data[20:24] == b"\x01\0\0\0" and data[52:54] == b"\x40\0")
def sha(data):
    return hashlib.sha256(data).hexdigest()
def header(name, data):
    result = bytearray(512)
    result[:len(name)] = name.encode("ascii")
    def number(start, length, value):
        encoded = f"{value:0{length-1}o}".encode() + b"\0"
        require(len(encoded) == length)
        result[start:start+length] = encoded
    number(100, 8, 0o755 if name.startswith("bin/") else 0o644)
    number(108, 8, 0)
    number(116, 8, 0)
    number(124, 12, len(data))
    number(136, 12, 0)
    result[148:156] = b"        "
    result[156] = ord("0")
    result[257:265] = b"ustar\x0000"
    result[148:156] = f"{sum(result):06o}".encode() + b"\0 "
    return result
def write(dirfd, name, data):
    fd = os.open(name, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600, dir_fd=dirfd)
    try:
        with os.fdopen(fd, "wb", closefd=False) as stream:
            stream.write(data)
            stream.flush()
        os.fsync(fd)
    finally:
        os.close(fd)
    os.fsync(dirfd)
try:
    require(len(sys.argv) == 3)
    source, output = (pathlib.Path(value) for value in sys.argv[1:])
    require(source.is_absolute() and output.is_absolute())
    metadata = json.loads(read(source / "provenance.json", RECORD), object_pairs_hook=pairs)
    require(set(metadata) == {"schema_version", "product_version", "architecture", "source_revision",
                              "profile", "libraries", "members"})
    require(type(metadata["schema_version"]) is int and metadata["schema_version"] == 1
            and token(metadata["product_version"]) and metadata["architecture"] in ("x86_64", "aarch64")
            and isinstance(metadata["source_revision"], str)
            and re.fullmatch(r"[0-9a-f]{40}", metadata["source_revision"])
            and metadata["profile"] in ("dev", "release"))
    machine = 62 if metadata["architecture"] == "x86_64" else 183
    require(isinstance(metadata["members"], list) and len(metadata["members"]) == 9
            and isinstance(metadata["libraries"], list) and 0 < len(metadata["libraries"]) <= 32)
    data, expected = {}, {"provenance.json"}
    for artifact in metadata["members"]:
        require(set(artifact) == {"name", "size", "sha256", "features"})
        name, features = artifact["name"], artifact["features"]
        require(name in FILES and name not in data and isinstance(features, list)
                and len(features) <= 32 and all(token(v) for v in features)
                and features == sorted(set(features)))
        require(name != FILES[0] or "video" in features)
        content = read(source / name, LIMIT)
        require(type(artifact["size"]) is int and artifact["size"] == len(content)
                and artifact["sha256"] == sha(content))
        if name.startswith("bin/"):
            elf(content, machine)
        if name in FIELDS:
            require(len(content) <= RECORD)
            require(content.isascii())
            text = content.decode("utf-8")
            require(all(c == "\n" or (ord(c) >= 32 and not 127 <= ord(c) <= 159) for c in text))
            section, seen, commands = "", set(), 0
            require("%" not in text)
            for line in text.split("\n"):
                require(not line.rstrip().endswith("\\"))
                if line.startswith("["):
                    section = line
                directive = line.split("=", 1)[0].strip() if "=" in line else ""
                unit = name == FILES[4]
                expected_section = "[Service]" if unit else "[Desktop Entry]"
                values = {field: ("ExecStart=" + field + " run" if unit and i == 0 else
                                  "Environment=" + field if unit else "Exec=" + field)
                          for i, field in enumerate(FIELDS[name])}
                if directive.startswith("Exec") or directive == "TryExec":
                    commands += 1
                    require(line == values[FIELDS[name][0]] and section == expected_section)
                if "{{" in line or "}}" in line:
                    matches = [field for field, value in values.items() if line == value]
                    require(len(matches) == 1 and matches[0] not in seen and section == expected_section)
                    seen.add(matches[0])
                else:
                    require(directive != "Environment")
            require(commands == 1 and seen == set(FIELDS[name]))
        data[name] = content
        expected.add(name)
    libraries = set()
    for library in metadata["libraries"]:
        require(set(library) == {"name", "sha256"} and token(library["name"])
                and ".so" in library["name"] and library["name"] not in libraries)
        name = "libraries/" + library["name"]
        content = read(source / name, LIMIT)
        elf(content, machine)
        require(library["sha256"] == sha(content))
        libraries.add(library["name"])
        expected.add(name)
    observed = set()
    for directory, dirs, files in os.walk(source, followlinks=False):
        require(all(not (pathlib.Path(directory) / name).is_symlink() for name in dirs))
        observed.update(str((pathlib.Path(directory) / name).relative_to(source)) for name in files)
    require(observed == expected)
    manifest = json.dumps(metadata, sort_keys=True, separators=(",", ":")).encode()
    require(len(manifest) <= RECORD)
    archive = bytearray()
    for name, content in [("manifest.json", manifest), *[(name, data[name]) for name in FILES]]:
        archive += header(name, content) + content + bytes((-len(content)) % 512)
        require(len(archive) + 1024 <= ARCHIVE)
    archive += bytes(1024)
    for ancestor in output.parents:
        require(not ancestor.is_symlink())
    parent = output.parent.stat()
    require(stat.S_ISDIR(parent.st_mode) and parent.st_uid == os.getuid() and not parent.st_mode & 0o022)
    os.mkdir(output, 0o700)
    dirfd = os.open(output, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
    try:
        write(dirfd, "payload.tar", archive)
        write(dirfd, "payload.sha256", (sha(archive) + "\n").encode())
    finally:
        os.close(dirfd)
except (ValueError, KeyError, TypeError, OSError) as error:
    print("staging refused: " + type(error).__name__, file=sys.stderr)
    sys.exit(1)
PY
